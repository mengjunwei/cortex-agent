//! `CortexAgent` 运行时主循环（adk `Agent` trait 实现）。
//!
//! 对外只导出 [`CortexAgent`] 与 [`CortexAgentBuilder`]（路径不变）：
//! `crate::agent::runtime::cortex_agent::{CortexAgent, CortexAgentBuilder}`。
//!
//! 实现按职责拆到子模块：
//! - [`builder`]：`CortexAgent` / `CortexAgentBuilder` 字段定义与链式装配
//! - [`prompt`]：system prompt 分层构建（stable 前缀 / volatile 段 / skill 正文 preamble）
//! - [`compaction`]：上下文压缩（LLM 摘要）
//! - [`thinking`]：思考参数（thinking/effort/reasoning_effort）兜底重试
//! - [`llm_call`]：带指数退避的 LLM 调用 + 纯文本收尾事件
//! - [`tool_exec`]：单工具超时/panic 防护 + 工具上下文 `ToolCtx`

mod analytics;
mod builder;
mod compaction;
mod context_tool;
mod hook;
mod multi_agent;
mod llm_call;
mod prompt;
mod soft_landing;
mod thinking;
mod tool_exec;
mod trim;
mod window;

pub use builder::{CortexAgent, CortexAgentBuilder};

use std::collections::HashMap;
use std::sync::Arc;

use adk_rust::async_trait;
use adk_rust::serde_json::{Value, json};
use adk_rust::{
    Agent, Content, Event, EventStream, FunctionResponseData, InvocationContext, Part, Result, Tool,
};
use async_stream::stream;
use futures::StreamExt;

use builder::MAX_STEPS_PROMPT;
use analytics::emit_compaction;
use context_tool::GetContextRemainingTool;
use multi_agent::{ChildAgentFactory, ChildAgentRegistry};
pub use multi_agent::{ChildAgentEvent, ChildEventSink};
/// 上下文预算只读句柄（crate 内 SSE 层轮询推 token 用量）
pub(crate) use context_tool::SharedBudget;

/// CortexAgent 额外方法（非 Agent trait）：暴露预算只读句柄。
#[allow(private_interfaces)]
impl CortexAgent {
    /// 上下文预算只读句柄（effective_tokens / context_window / 窗口号）。
    /// SSE 层在 run 期间轮询，向前端推 token 用量（对齐 codex token 显示）。
    pub fn budget(&self) -> SharedBudget {
        Arc::clone(&self.budget_handle)
    }
}
use compaction::{build_compaction_event, is_summary_content, llm_compact};
use hook::{CompactionContext, CompactionDecision, CompactionResult};
use llm_call::{generate_with_retry, make_text_event};
use prompt::{build_preamble, build_stable_prefix, build_volatile_context};
use soft_landing::{borrow_message, evaluate_soft_landing, reminder_message, SoftLandingDecision};
use thinking::{clear_thinking_from_config, config_has_thinking, looks_like_thinking_param_error};
use tool_exec::execute_one_tool_safe;
use trim::trim_tool_outputs_to_fit;
use window::WindowState;

// ── 窗口压缩阈值比例（对齐 codex，固化为常量不可配）──
/// 软闸：context_window × 0.9，到软闸进入 buffer 区（借一轮/早压缩）
const SOFT_GATE_RATIO: f64 = 0.9;
/// 硬闸：context_window × 0.95，到硬闸强制压缩
const HARD_GATE_RATIO: f64 = 0.95;
/// 提醒阈值：剩余 token 占窗口比例 ≤ 0.15 时提醒模型收尾
const REMINDER_THRESHOLD_RATIO: f64 = 0.15;

#[async_trait]
impl Agent for CortexAgent {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn sub_agents(&self) -> &[Arc<dyn Agent>] {
        &self.sub_agents
    }

    async fn run(&self, ctx: Arc<dyn InvocationContext>) -> Result<EventStream> {
        let agent_name = self.name.clone();
        let model = self.model.clone();
        // 采样层防重复退化：惩罚已出现 token，从源头降低模型生成重复/死循环倾向
        //（OpenAI 兼容协议标准参数）。用户未显式配置时给适度默认 0.3。
        let mut config = {
            let mut c = self.config.clone().unwrap_or_default();
            if c.frequency_penalty.is_none() {
                c.frequency_penalty = Some(0.3);
            }
            if c.presence_penalty.is_none() {
                c.presence_penalty = Some(0.3);
            }
            Some(c)
        };
        let max_iter = self.max_iterations;
        let llm_timeout = self.llm_timeout;
        let tool_timeout = self.tool_timeout;
        let cancel_token = self.cancel_token.clone();
        let sub_names: Vec<String> = self
            .sub_agents
            .iter()
            .map(|a| a.name().to_string())
            .collect();

        // 共享预算快照（get_context_remaining 工具只读、主循环每轮刷新 effective_tokens）。
        // 对齐 codex `get_context_remaining`：让模型自查剩余 token，减少盲目撞墙/中途压缩。
        // 复用 build 时创建的 budget_handle：SSE 层经 budget() 只读轮询，向前端推 token 用量。
        let budget = self.budget_handle.clone();

        // Collect all tools
        let mut all_tools: Vec<Arc<dyn Tool>> = self.tools.clone();
        for ts in &self.toolsets {
            if let Ok(ts_tools) = ts.tools(ctx.clone()).await {
                all_tools.extend(ts_tools);
            }
        }
        // 内建 get_context_remaining：模型可主动查询剩余 token 预算与窗口号（只读、可并发）。
        all_tools.push(Arc::new(GetContextRemainingTool::new(Arc::clone(&budget))) as Arc<dyn Tool>);

        // 内建 spawn_agent / wait_agent：动态多 agent 并行（减少交互轮次）。
        // 子 agent 与父同构，后台独立跑、主循环不阻塞，wait 收齐结果。
        // max_spawn_depth = 0 时禁用本特性。
        if self.context_config.max_spawn_depth > 0 {
            let ma_registry = Arc::new(ChildAgentRegistry::new());
            let ma_factory = Arc::new(ChildAgentFactory::new(
                self.child_blueprint(),
                ma_registry.clone(),
                ctx.clone(),
                cancel_token.clone(),
                self.child_event_sink.clone(),
                self.spawn_depth,
                self.context_config.max_spawn_depth,
                self.context_config.max_concurrent_children,
            ));
            all_tools.push(ma_factory.spawn_handle());
            all_tools.push(ma_factory.wait_handle(ma_registry));
        }
        let tool_decls: HashMap<String, Value> = all_tools
            .iter()
            .map(|t| (t.name().to_string(), t.declaration()))
            .collect();
        let tool_map: HashMap<String, Arc<dyn Tool>> = all_tools
            .iter()
            .map(|t| (t.name().to_string(), t.clone()))
            .collect();

        // Build system prompt: stable 前缀（跨请求不变，命中缓存）+ volatile 段（时间，每次刷新）
        let stable_prompt = build_stable_prefix(
            &self.instruction,
            &self.memory_block,
            &self.skill_catalog,
            self.policy,
        );
        let volatile_prompt = build_volatile_context();

        // preamble = [stable_system, volatile_system, user(bodies)?]；stable 在最前保证前缀缓存命中。
        // skill 正文以 user-role 第三条注入（对齐 codex body=user），不进 stable 缓存前缀。
        let preamble = build_preamble(stable_prompt, volatile_prompt, &self.skill_bodies);
        // preamble 长度动态（有 body 时 3，否则 2）；compaction 据此保护整个前缀不被压缩。
        let preamble_len = preamble.len();

        // 告知 Anthropic 客户端 stable 边界（用于 system 分 block 打 cache_control）。
        // OpenAI 端忽略此键，靠消息顺序天然命中前缀缓存。
        if let Some(c) = config.as_mut() {
            let cortex = c
                .extensions
                .entry("cortex".to_string())
                .or_insert_with(|| json!({}));
            if let Some(obj) = cortex.as_object_mut() {
                obj.insert("stable_system_count".to_string(), json!(1u64));
            } else {
                // cortex extension 已存在但非 object(理论边缘):count 写不进 → client 走兜底
                // 把 volatile(时间)也归 stable → 时间每请求变 → prompt cache 静默失效。记 warn 暴露。
                tracing::warn!(
                    "[cortex_agent] cortex extension 非 object,stable_system_count 未写入(prompt cache 可能失效)"
                );
            }
        }

        let history = ctx.session().conversation_history();
        let invocation_id = uuid::Uuid::now_v7().to_string();
        let parent_ctx = ctx.clone();
        let should_stream = matches!(
            ctx.run_config().streaming_mode,
            adk_rust::StreamingMode::SSE | adk_rust::StreamingMode::Bidi
        );
        let ended_ctx = ctx.clone();
        // 窗口阈值压缩（对齐 codex：仅按 context_window 触发，不按轮数/单轮 token）。
        // 软闸 ×0.9 早压缩 / 硬闸 ×0.95 强压缩 / 提醒阈值 ×0.15（剩余占比），比例固化为 codex 常量。
        // context_window 由 model_provider 注入（Builder.context_window），未配走 fallback。
        let chars_per_token = self.context_config.chars_per_token as usize;
        let context_window = self
            .context_window
            .unwrap_or(self.context_config.fallback_context_window);
        let soft_gate = ((context_window as f64) * SOFT_GATE_RATIO) as usize;
        let hard_gate = ((context_window as f64) * HARD_GATE_RATIO) as usize;
        let reminder_threshold_tokens = ((context_window as f64) * REMINDER_THRESHOLD_RATIO) as usize;
        // 初始化预算快照的静态字段（gates/context_window/window 号）；effective_tokens 由
        // 主循环每轮刷新，get_context_remaining 工具据此回答模型。
        {
            let mut b = budget.write().expect("budget lock poisoned");
            b.context_window = context_window;
            b.soft_gate = soft_gate;
            b.hard_gate = hard_gate;
            b.window_number = 1; // WindowState 从 1 开始
        }
        let compact_model = self.compact_model.clone();
        let hooks = self.hooks.clone();

        let s = stream! {
            let mut iteration = 0u32;
            // 上一轮模型返回的 interaction_id，用于本次请求的 previous_response_id（链式续接，
            // 可省去重复 prefill 的 token）。注意：本项目当前用 OpenAI 兼容 / Anthropic 协议，
            // 这两类 client 不 populate interaction_id（恒 None），故此处目前为 no-op；
            // 切到 Gemini Interactions 协议时才生效。
            let mut last_interaction_id: Option<String> = None;
            // 上一轮模型返回的真实 token 用量（来自 usage_metadata），优先于字符估算。
            let mut last_usage_tokens: Option<i32> = None;
            // 上一轮缓存命中 token（BodyAfterPrefix：计量时扣除缓存前缀，避免稳定长前缀反复误触压缩）
            let mut last_cache_read: Option<i32> = None;
            // 上下文窗口状态（窗口号 / 软着陆 per-window flag / 压缩计数）
            let mut window = WindowState::new();
            // 思考参数兜底：模型不支持 thinking/effort/reasoning_effort 时，去参数重试一次（本次 run 内）
            let mut thinking_retry_done = false;
            let mut conv: Vec<Content> = preamble;
            conv.extend(history);

            loop {
                iteration += 1;

                if ended_ctx.ended() || cancel_token.is_cancelled() { break; }

                if iteration > max_iter {
                    // 软降级（对齐 opencode max-steps.ts）：已达轮次上限，不硬塞系统提示文本，
                    // 而是关掉工具 + 注入总结 prompt，让模型用纯文本总结已完成工作与剩余阻碍，
                    // 给用户一个有意义的收尾。模型在无工具约束下只能文字回复，自然 turn_complete。
                    tracing::info!(
                        "[cortex_agent] 达到最大轮次 {max_iter}，软降级（关工具 + 注入总结 prompt）"
                    );
                    conv.push(Content {
                        role: "user".to_string(),
                        parts: vec![Part::Text { text: MAX_STEPS_PROMPT.to_string() }],
                    });
                    // 软降级请求同样先归一化配对（历史 conv 可能含孤立 FR）
                    normalize_function_pairs(&mut conv);
                    let soft_req = adk_rust::LlmRequest {
                        model: model.name().to_string(),
                        contents: conv.clone(),
                        config: config.clone(),
                        tools: HashMap::new(),
                        previous_response_id: last_interaction_id.clone(),
                    };
                    match generate_with_retry(&model, soft_req, llm_timeout, 3, &cancel_token).await {
                        Ok(mut soft_stream) => {
                            while let Some(r) = soft_stream.next().await {
                                let chunk = match r {
                                    Ok(c) => c,
                                    Err(e) => {
                                        yield Err(e);
                                        return;
                                    }
                                };
                                if should_stream {
                                    let mut ev = Event::with_id(
                                        uuid::Uuid::now_v7().to_string(),
                                        &invocation_id,
                                    );
                                    ev.author = agent_name.clone();
                                    ev.llm_response = chunk.clone();
                                    yield Ok(ev);
                                }
                                if chunk.turn_complete || chunk.finish_reason.is_some() {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            if cancel_token.is_cancelled() {
                                tracing::info!("[cortex_agent] agent 因用户取消退出（软降级阶段）");
                            } else {
                                tracing::warn!("[cortex_agent] 软降级总结请求失败，回退提示文本: {e}");
                                yield Ok(make_text_event(
                                    &invocation_id,
                                    &agent_name,
                                    "已达到本轮最大交互步数，自动停止。可以：换一种问法继续、新建会话精简上下文，或更换模型重试。",
                                ));
                            }
                        }
                    }
                    break;
                }

                // ===== 上下文治理：BodyAfterPrefix token 估算 + 三级软着陆 + 可回放压缩 =====
                // 净 token = 真实 usage − 缓存命中前缀（对齐 codex BodyAfterPrefix scope），
                // 避免缓存命中的稳定长前缀反复误触压缩。无 usage（首轮）回退字符估算。
                let effective_tokens: usize = if let Some(t) = last_usage_tokens {
                    (t.max(0) as usize)
                        .saturating_sub(last_cache_read.unwrap_or(0).max(0) as usize)
                } else {
                    conv.iter()
                        .map(|c| c.parts.iter().map(|p| match p {
                            Part::Text { text } => text.len(),
                            Part::Thinking { thinking, .. } => thinking.len(),
                            Part::FunctionResponse { function_response, .. } => function_response.response.to_string().len(),
                            _ => 64,
                        }).sum::<usize>()).sum::<usize>() / chars_per_token.max(1)
                };
                let before_tokens = effective_tokens;
                // 报给 get_context_remaining 的 token 数：默认用上一轮 usage 估计；
                // 若本轮触发压缩，压缩分支会用压缩后的 after_tokens 覆盖（更准）。
                let mut budget_tokens = effective_tokens;

                match evaluate_soft_landing(
                    effective_tokens, soft_gate, hard_gate, reminder_threshold_tokens, &window,
                ) {
                    SoftLandingDecision::Remind => {
                        // ① 软着陆提醒：窗口将满，让模型主动收尾（每窗一次）
                        let remaining = soft_gate.saturating_sub(effective_tokens);
                        conv.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part::Text { text: reminder_message(remaining, window.window_number) }],
                        });
                        window.reminder_shown = true;
                        tracing::info!(
                            "[cortex_agent] 软着陆：注入提醒（剩余 ~{} tokens，窗口 #{}）",
                            remaining, window.window_number
                        );
                    }
                    SoftLandingDecision::BorrowOneTurn => {
                        // ② 借最后一轮：在 buffer 区给模型写交接的机会（每窗一次）
                        conv.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part::Text { text: borrow_message().to_string() }],
                        });
                        window.borrowed = true;
                        tracing::info!("[cortex_agent] 软着陆：借最后一轮（buffer 区）");
                    }
                    SoftLandingDecision::ForceCompact if conv.len() > preamble_len + 1 => {
                        // ③ 硬压缩：trim → LLM 摘要 → yield compaction 检查点（可回放）→ 开新窗
                        // pre_compact hook：允许 veto（保持原历史不变，跳过本次压缩）
                        let pre_ctx = CompactionContext {
                            window_number: window.window_number,
                            compaction_count: window.compaction_count,
                            before_tokens,
                        };
                        let mut vetoed = false;
                        for h in &hooks {
                            if matches!(h.pre_compact(&pre_ctx).await, CompactionDecision::Abort) {
                                vetoed = true;
                                tracing::info!("[cortex_agent] 压缩被 pre_compact hook 中止");
                                break;
                            }
                        }
                        let compact_started = std::time::Instant::now();
                        let keep_tail = 4; // 最近 4 条原样保留（保护进行中的 tool 流）
                        let mut split_point = conv.len().saturating_sub(keep_tail);
                        // FunctionCall/Response 配对保护：避免 older 末尾 FunctionCall 被摘要后，
                        // tail 留下孤立 FunctionResponse 触发严格模式 400。
                        while split_point > preamble_len + 1 {
                            let older_last_is_fc = conv[split_point - 1]
                                .parts.iter().any(|p| matches!(p, Part::FunctionCall { .. }));
                            let tail_first_is_fr = conv[split_point]
                                .parts.iter().any(|p| matches!(p, Part::FunctionResponse { .. }));
                            if older_last_is_fc && tail_first_is_fr {
                                split_point -= 1;
                            } else {
                                break;
                            }
                        }

                        if !vetoed && split_point > preamble_len + 1 {
                            // L2 历史级裁剪：先把超大的旧工具输出截短。可能直接降到预算内，省一次 LLM 摘要。
                            let (trim_stats, under_budget) =
                                trim_tool_outputs_to_fit(&mut conv, preamble_len, soft_gate, chars_per_token);
                            if trim_stats.trimmed_outputs > 0 {
                                tracing::info!(
                                    "[cortex_agent] 历史裁剪：{} 条工具输出截短，去除 ~{} 字节",
                                    trim_stats.trimmed_outputs, trim_stats.chars_removed
                                );
                            }

                            if !under_budget {
                                let older: Vec<Content> = conv[preamble_len..split_point].to_vec();
                                let tail: Vec<Content> = conv[split_point..].to_vec();

                                // 旧 user 消息原文保留（按预算从后往前），旧非 user（含上一轮摘要）摘要成一条。
                                // 注意：旧摘要（model role）必须纳入 to_summarize 再摘要，否则重复压缩时
                                // 上一轮的进度/决策会被静默丢弃（渐进失忆）；接受「摘要的摘要」级联——
                                // 轻微失真远优于完全丢失。
                                let user_budget_chars = 80_000usize;
                                let mut retained_users: Vec<Content> = Vec::new();
                                let mut retained_chars = 0usize;
                                for c in older.iter().rev() {
                                    if c.role == "user" && !is_summary_content(c) {
                                        let len: usize = c.parts.iter().map(|p| match p {
                                            Part::Text { text } => text.len(), _ => 0,
                                        }).sum();
                                        if retained_chars + len > user_budget_chars { break; }
                                        retained_chars += len;
                                        retained_users.push(c.clone());
                                    }
                                }
                                retained_users.reverse();

                                let to_summarize: Vec<Content> = older.iter()
                                    .filter(|c| c.role != "user")
                                    .cloned().collect();
                                let non_user_count = to_summarize.len();
                                let summary = if to_summarize.is_empty() {
                                    String::new()
                                } else {
                                    llm_compact(&model, compact_model.as_ref(), &to_summarize, &cancel_token).await
                                };

                                // 取消则不落库半截摘要
                                if cancel_token.is_cancelled() {
                                    tracing::info!("[cortex_agent] 压缩期间用户取消，不持久化摘要");
                                    break;
                                }

                                // 重建：[preamble(stable+volatile), summary?, ...retained_users, ...tail]
                                let preamble_msgs: Vec<Content> = conv[..preamble_len].to_vec();
                                conv.clear();
                                conv.extend(preamble_msgs);
                                if !summary.is_empty() {
                                    conv.push(Content {
                                        role: "model".to_string(),
                                        parts: vec![Part::Text { text: summary.clone() }],
                                    });
                                    // ★ 可回放：yield compaction 检查点事件。
                                    // 框架自动持久化（runner 非partial Event 落库）+
                                    // 下次 turn 经 conversation_history_for_agent_impl 以本条为回放边界。
                                    yield Ok(build_compaction_event(&invocation_id, summary));
                                }
                                conv.extend(retained_users);
                                conv.extend(tail);

                                window.advance();
                                let after_tokens = conv.iter()
                                    .map(|c| c.parts.iter().map(|p| match p {
                                        Part::Text { text } => text.len(),
                                        Part::Thinking { thinking, .. } => thinking.len(),
                                        Part::FunctionResponse { function_response, .. } => function_response.response.to_string().len(),
                                        _ => 64,
                                    }).sum::<usize>()).sum::<usize>() / chars_per_token.max(1);
                                // 压缩后真实余量，覆盖默认估计，报给 get_context_remaining。
                                budget_tokens = after_tokens;
                                emit_compaction(
                                    "intra_turn", "context_limit",
                                    before_tokens, after_tokens, last_cache_read,
                                    compact_started.elapsed().as_millis() as u64,
                                    window.window_number, keep_tail,
                                );
                                // post_compact hook：通知压缩完成
                                let post_ctx = CompactionContext {
                                    window_number: window.window_number,
                                    compaction_count: window.compaction_count,
                                    before_tokens,
                                };
                                let pres = CompactionResult {
                                    after_tokens,
                                    window_number: window.window_number,
                                };
                                for h in &hooks {
                                    h.post_compact(&post_ctx, &pres).await;
                                }
                                tracing::info!(
                                    "[cortex_agent] compacted {non_user_count} non-user msgs into summary, window #{}",
                                    window.window_number
                                );
                            }
                        }
                    }
                    _ => {} // Nominal 或可压缩内容不足，正常发请求
                }

                // 统一刷新预算快照：此时压缩/提醒均已落定，window_number 与 budget_tokens 都是
                // 请求即将发出那一刻的真实值（压缩当轮 = after_tokens + 新窗口号）。
                {
                    let mut b = budget.write().expect("budget lock poisoned");
                    b.effective_tokens = budget_tokens;
                    b.window_number = window.window_number;
                }

                // 请求级 FC/FR 配对归一化（对标 codex ensure_call_outputs_present +
                // remove_orphan_outputs）：压缩/删条/回滚可能破坏 FunctionCall/Response 配对，
                // 发请求前清理——删孤立 FunctionResponse、为孤立 FunctionCall 补占位 FunctionResponse，
                // 避免触发严格模式 400（修高危④）。
                normalize_function_pairs(&mut conv);

                let mut request = adk_rust::LlmRequest {
                    model: model.name().to_string(),
                    contents: conv.clone(),
                    config: config.clone(),
                    tools: tool_decls.clone(),
                    previous_response_id: last_interaction_id.clone(),
                };

                // 建连 + 超窗兜底：超窗时删最旧一条非 preamble 消息重试（保前缀缓存），最多 3 次。
                let mut stream;
                let mut overrun_attempts = 0u32;
                loop {
                    match generate_with_retry(&model, request.clone(), llm_timeout, 3, &cancel_token).await {
                        Ok(s) => { stream = s; break; }
                        Err(e) => {
                            // cancel 引起的 Err 静默退出，不发"调用失败"错误文本（用户点停止不应看到错误）
                            if cancel_token.is_cancelled() {
                                tracing::info!("[cortex_agent] agent 因用户取消退出（LLM 建连阶段）");
                                return;
                            }
                            // 超窗兜底（对齐 codex ContextWindowExceeded：删最旧一条保前缀缓存后重试）。
                            // 删条后下方立即 normalize_function_pairs 清理因删除产生的孤立 FC/FR。
                            if compaction::is_context_window_exceeded(&e)
                                && overrun_attempts < 3
                                && conv.len() > preamble_len + 1
                            {
                                overrun_attempts += 1;
                                conv.remove(preamble_len);
                                // 删条后立即归一化配对（删到 FC/FR 会产生孤立）—修高危④
                                normalize_function_pairs(&mut conv);
                                request.contents = conv.clone();
                                tracing::warn!(
                                    "[cortex_agent] 上下文超窗，删最旧一条消息重试 #{}（剩 {} 条）",
                                    overrun_attempts, conv.len()
                                );
                                continue;
                            }
                            tracing::error!("[cortex_agent] LLM 调用最终失败: {e}");
                            yield Ok(make_text_event(&invocation_id, &agent_name, "[LLM call failed after retries.]"));
                            return;
                        }
                    }
                }

                let mut parts: Vec<Part> = Vec::new();

                loop {
                    let chunk_result = tokio::select! {
                        r = stream.next() => match r {
                            Some(r) => r,
                            None => break,
                        },
                        _ = cancel_token.cancelled() => {
                            tracing::info!("[cortex_agent] LLM 流读取被用户取消");
                            break;
                        }
                    };
                    let chunk = match chunk_result { Ok(c) => c, Err(e) => { yield Err(e); return; } };
                    // ★ 思考参数兜底：不支持的模型收到 thinking/effort/reasoning_effort 会以参数错误
                    // 首事件返回（error_code + error_message）。去掉思考参数重试一次，并更新 config
                    // 使后续轮次也走模型默认（静默忽略，任务继续而非中断）。
                    if !thinking_retry_done
                        && chunk.error_code.is_some()
                        && config_has_thinking(&config)
                        && looks_like_thinking_param_error(&chunk)
                    {
                        thinking_retry_done = true;
                        clear_thinking_from_config(&mut config);
                        tracing::warn!(
                            "[cortex_agent] 模型不支持思考参数，去掉后重试一次：{}",
                            chunk.error_message.as_deref().unwrap_or("")
                        );
                        let req2 = adk_rust::LlmRequest {
                            model: model.name().to_string(),
                            contents: conv.clone(),
                            config: config.clone(),
                            tools: tool_decls.clone(),
                            previous_response_id: last_interaction_id.clone(),
                        };
                        match generate_with_retry(&model, req2, llm_timeout, 3, &cancel_token).await {
                            Ok(s) => {
                                stream = s;
                                continue;
                            }
                            Err(e) => {
                                yield Err(e);
                                return;
                            }
                        }
                    }
                    if let Some(c) = &chunk.content {
                        // 收集本轮所有 parts（含 Text / Thinking / FunctionCall 等），不再为重复检测
                        // 做文本累积——流式 delta 只用于展示与回填，不参与循环控制（对齐 codex/opencode）。
                        parts.extend(c.parts.iter().cloned());
                    }
                    // 记录 interaction_id，下一轮作为 previous_response_id 链式续接
                    if chunk.interaction_id.is_some() { last_interaction_id = chunk.interaction_id.clone(); }
                    // 记录模型返回的真实 token 用量 + 缓存命中（BodyAfterPrefix），优先用于 compaction 判定
                    if let Some(u) = &chunk.usage_metadata {
                        last_usage_tokens = Some(u.total_token_count);
                        last_cache_read = u.cache_read_input_token_count;
                    }
                    if should_stream {
                        let mut ev = Event::with_id(uuid::Uuid::now_v7().to_string(), &invocation_id);
                        ev.author = agent_name.clone();
                        ev.llm_response = chunk.clone();
                        yield Ok(ev);
                    }
                    // turn 是否结束完全由协议信号决定（对齐 codex / opencode）：模型给出
                    // turn_complete / finish_reason 即视为本轮回答完成，退出 chunk 循环。
                    // 不再对流式文本做重复检测——退化治理交给 thinking budget 与 max_iterations 兜底。
                    if chunk.turn_complete || chunk.finish_reason.is_some() { break; }
                }

                // （重复退化重导向 / 硬跳过分支已移除：见 builder.rs 文件头说明。循环回归协议驱动。）

                // 无 id 的 FC（文本标签/弱供应商解析产生 id=None）：push conv 前补全局唯一合成 id，
                // 否则回填的 FR 拿到空 id、normalize 会把它当孤立 FR 误删 → 触发严格模式 400。
                // 用全局单调计数器（跨轮/跨 run 唯一），避免局部序号在不同迭代重复导致 normalize 错配。
                let mut parts = parts;
                for p in parts.iter_mut() {
                    if let Part::FunctionCall { id, .. } = p {
                        if id.as_deref().map(str::is_empty).unwrap_or(true) {
                            *id = Some(crate::llm::next_synthetic_call_id());
                        }
                    }
                }

                let content = match if parts.is_empty() { None } else { Some(Content { role: "model".to_string(), parts }) } {
                    Some(c) => c, None => break,
                };

                conv.push(content.clone());

                let fcs: Vec<(String, Value, Option<String>)> = content.parts.iter()
                    .filter_map(|p| match p { Part::FunctionCall { name, args, id, .. } => Some((name.clone(), args.clone(), id.clone())), _ => None }).collect();

                if fcs.is_empty() {
                    // 纯文本输出、流正常结束 → 终止 turn（模型本轮未调用工具即视为回答完成）
                    break;
                }

                // 先处理 transfer_to_agent（遇到有效目标立即转交并结束本次 run）
                let mut pending: Vec<(usize, String, Value, Option<String>)> = Vec::with_capacity(fcs.len());
                for (i, (name, args, id)) in fcs.into_iter().enumerate() {
                    if name == "transfer_to_agent" {
                        let target = args.get("agent_name").and_then(|v| v.as_str()).unwrap_or("");
                        if sub_names.iter().any(|n| n == target) {
                            let mut ev = Event::new(&invocation_id);
                            ev.author = agent_name.clone();
                            ev.actions.transfer_to_agent = Some(target.to_string());
                            yield Ok(ev);
                            return;
                        }
                        continue; // 无效 transfer：跳过，不回填
                    }
                    pending.push((i, name, args, id));
                }

                // Auto 策略（对齐 adk-rust ToolExecutionStrategy::Auto）：read_only 工具并发，
                // 有副作用的串行；结果按模型给的原序回填。
                let mut results: Vec<(usize, Value, String, Option<String>)> = Vec::with_capacity(pending.len());

                // read_only 组：并发执行（join_all）
                let ro_items: Vec<(usize, String, Value, Option<String>)> = pending.iter()
                    .filter(|(_, name, _, _)| tool_map.get(name).map(|t| t.is_read_only()).unwrap_or(false))
                    .cloned()
                    .collect();
                if !ro_items.is_empty() {
                    let ro_results: Vec<(usize, Value, String, Option<String>)> = {
                        let tm = &tool_map;
                        let pc = &parent_ctx;
                        let ct = &cancel_token;
                        let futs = ro_items.into_iter().map(move |(i, name, args, id)| async move {
                            let r = execute_one_tool_safe(tm, pc, &name, &args, &id, tool_timeout, ct).await;
                            (i, r, name, id)
                        });
                        futures::future::join_all(futs).await
                    };
                    results.extend(ro_results);
                }

                // 副作用组：串行执行
                for (i, name, args, id) in pending.iter()
                    .filter(|(_, name, _, _)| !tool_map.get(name).map(|t| t.is_read_only()).unwrap_or(false))
                {
                    let r = execute_one_tool_safe(&tool_map, &parent_ctx, name, args, id, tool_timeout, &cancel_token).await;
                    results.push((*i, r, name.clone(), id.clone()));
                }

                // 按原序排序后回填模型 + 发事件
                results.sort_by_key(|(i, _, _, _)| *i);
                for (_, result, name, id) in results {
                    let resp = Content { role: "function".to_string(), parts: vec![Part::FunctionResponse {
                        function_response: FunctionResponseData::new(name.clone(), result), id: Some(id.clone().unwrap_or_default()),
                        annotations: None,
                    }] };
                    let mut ev = Event::new(&invocation_id);
                    ev.author = agent_name.clone();
                    ev.llm_response.content = Some(resp.clone());
                    yield Ok(ev);
                    conv.push(resp);
                }
            }
        };

        Ok(Box::pin(s))
    }
}

/// 请求级 FunctionCall/FunctionResponse 配对归一化（对标 codex
/// `context_manager::normalize` 的 `ensure_call_outputs_present` + `remove_orphan_outputs`）。
///
/// 压缩切点、超窗删条、回滚等操作可能破坏 FC/FR 配对，导致发给 API 的历史出现「孤立
/// FunctionResponse（无对应 FunctionCall）」或「孤立 FunctionCall（无对应 FunctionResponse）」，
/// 触发 Anthropic/OpenAI 严格模式 400。本函数在每次发请求前就地清理 `conv`：
///
/// 1. **删孤立 FunctionResponse**：id 不在任何 FunctionCall 中的 FR（含空 id 的回填兜底）直接删除；
///    删空后若 function-role 消息无 parts，整条移除。
/// 2. **补孤立 FunctionCall**：id 不在任何 FunctionResponse 中的 FC，在其所在 model 消息之后
///    插入一条占位 FunctionResponse（标注 aborted），让配对闭合。
///
/// 仅在请求边界调用，不改 `conv` 的语义结构。
fn normalize_function_pairs(conv: &mut Vec<Content>) {
    use std::collections::HashSet;

    // 第一遍：收集所有非空 FunctionCall id
    let mut call_ids: HashSet<String> = HashSet::new();
    for c in conv.iter() {
        for p in &c.parts {
            if let Part::FunctionCall { id, .. } = p {
                if let Some(id) = id.as_ref().filter(|s| !s.is_empty()) {
                    call_ids.insert(id.clone());
                }
            }
        }
    }

    // 第二遍：删除孤立的 FunctionResponse，并记录已配对的 response id
    let mut matched_resp_ids: HashSet<String> = HashSet::new();
    for c in conv.iter_mut() {
        let mut orphan: Vec<usize> = Vec::new();
        for (i, p) in c.parts.iter().enumerate() {
            if let Part::FunctionResponse { id, .. } = p {
                let rid = id.as_deref().unwrap_or("");
                if rid.is_empty() || !call_ids.contains(rid) {
                    orphan.push(i); // 空 id 或无对应 FC → 删
                } else {
                    matched_resp_ids.insert(rid.to_string());
                }
            }
        }
        for i in orphan.into_iter().rev() {
            c.parts.remove(i);
        }
    }
    // 删除因孤立 FR 清空后的 function-role 消息
    conv.retain(|c| !(c.role == "function" && c.parts.is_empty()));

    // 第三遍：为孤立的 FunctionCall 补占位 FunctionResponse。
    // 空 id FC 先回写一个全局唯一合成 id 到本体——序列化端（openai_custom/anthropic_custom）
    // 对 None id 的兜底是 `call_{name}`，若不回写，同消息里两个不同名的空 id FC 会各自生成
    // `call_shell`/`call_read`，而这里只用单个 `call_{name}` 占位，wire 层 mismatch 触发 400。
    // 回写后 FC 有了稳定 id，占位 FR 用同一个 id 即可配成对。
    let mut inserts: Vec<(usize, Content)> = Vec::new();
    for (i, c) in conv.iter_mut().enumerate() {
        if c.role != "model" {
            continue;
        }
        for p in c.parts.iter_mut() {
            if let Part::FunctionCall { name, id, .. } = p {
                // 空 id 回写全局唯一合成 id；有 id 且已配对则跳过
                let cid = match id.as_deref() {
                    Some(s) if !s.is_empty() => {
                        if matched_resp_ids.contains(s) {
                            continue;
                        }
                        s.to_string()
                    }
                    _ => {
                        let synth = crate::llm::next_synthetic_call_id();
                        *id = Some(synth.clone());
                        synth
                    }
                };
                let placeholder = Content {
                    role: "function".to_string(),
                    parts: vec![Part::FunctionResponse {
                        function_response: FunctionResponseData::new(
                            name.clone(),
                            json!({ "error": "[aborted: tool result missing after context compaction]" }),
                        ),
                        id: Some(cid),
                        annotations: None,
                    }],
                };
                inserts.push((i + 1, placeholder));
            }
        }
    }
    // 从后往前插入，保持下标稳定；同一 model 消息多个 FC 按原序排列。
    for (pos, content) in inserts.into_iter().rev() {
        conv.insert(pos, content);
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    fn text(role: &str, t: &str) -> Content {
        Content { role: role.to_string(), parts: vec![Part::Text { text: t.to_string() }] }
    }

    fn fc(id: &str, name: &str) -> Content {
        Content {
            role: "model".to_string(),
            parts: vec![Part::FunctionCall {
                name: name.to_string(),
                args: json!({}),
                id: Some(id.to_string()),
                thought_signature: None,
            }],
        }
    }

    fn fr(id: &str, name: &str) -> Content {
        Content {
            role: "function".to_string(),
            parts: vec![Part::FunctionResponse {
                function_response: FunctionResponseData::new(name.to_string(), json!({"result": "ok"})),
                id: Some(id.to_string()),
                annotations: None,
            }],
        }
    }

    fn count_fr(c: &Content) -> usize {
        c.parts.iter().filter(|p| matches!(p, Part::FunctionResponse { .. })).count()
    }

    #[test]
    fn paired_history_is_unchanged() {
        let mut conv = vec![text("system", "sys"), text("user", "q"), fc("c1", "shell"), fr("c1", "shell"), text("model", "done")];
        let len = conv.len();
        normalize_function_pairs(&mut conv);
        assert_eq!(conv.len(), len, "正常配对不应增删消息");
        assert!(conv.iter().any(|c| count_fr(c) == 1));
    }

    #[test]
    fn removes_orphan_function_response() {
        // 高危④：压缩切点把 FC 摘要掉，tail 留下孤立 FR → 应删除，避免 400
        let mut conv = vec![text("system", "sys"), text("user", "q"), fr("ghost", "shell"), text("user", "more")];
        normalize_function_pairs(&mut conv);
        assert!(!conv.iter().any(|c| count_fr(c) > 0), "孤立 FunctionResponse 应被删除");
    }

    #[test]
    fn backfills_missing_function_response() {
        // 高危④：超窗删条删掉 FR 后留下孤立 FC → 应补占位 FR
        let mut conv = vec![text("system", "sys"), text("user", "q"), fc("c1", "shell")];
        normalize_function_pairs(&mut conv);
        // conv = [system, user, model(FC c1), function(占位 FR c1)]
        assert_eq!(conv.len(), 4);
        let placeholder = &conv[3];
        assert_eq!(placeholder.role, "function");
        assert_eq!(count_fr(placeholder), 1);
    }

    #[test]
    fn orphan_fr_after_compaction_split_is_removed() {
        // 模拟高危①的最小复现：older 摘要掉 model(多FC)，tail 残留 FR2 孤立
        let mut conv = vec![
            text("system", "sys"),
            text("user", "q"),
            fc("c1", "read_a"),
            fr("c1", "read_a"), // c1 配对完整，保留
            fr("c2", "read_b"), // c2 的 FC 已被摘要掉 → 孤立，应删
            text("user", "q2"),
        ];
        normalize_function_pairs(&mut conv);
        let fr_ids: Vec<String> = conv.iter()
            .flat_map(|c| c.parts.iter())
            .filter_map(|p| match p {
                Part::FunctionResponse { id, .. } => id.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(fr_ids, vec!["c1".to_string()], "只应保留配对完整的 FR(c1)，孤立的 c2 应删");
    }

    #[test]
    fn empty_id_fc_gets_synthetic_placeholder() {
        // F1：空 id 的孤立 FC（弱供应商/文本标签来源）回写全局唯一合成 id 并补占位 FR，
        // 占位 FR 用同一 id 配成对，不触发 400。
        let empty_id_fc = Content {
            role: "model".to_string(),
            parts: vec![Part::FunctionCall {
                name: "shell".to_string(),
                args: json!({}),
                id: None,
                thought_signature: None,
            }],
        };
        let mut conv = vec![text("system", "sys"), empty_id_fc];
        normalize_function_pairs(&mut conv);
        assert_eq!(conv.len(), 3, "空 id FC 应补占位 FR");
        // 占位 FR id 应是合成 id（call_s{n}）
        let placeholder_id = conv[2].parts.iter()
            .filter_map(|p| match p { Part::FunctionResponse { id, .. } => id.clone(), _ => None })
            .next()
            .expect("应有占位 FR id");
        assert!(placeholder_id.starts_with("call_s"), "占位 id 应为合成 id: {placeholder_id}");
        // 关键：FC 本体被回写同一个 id，wire 层配成对
        let fc_id = conv[1].parts.iter()
            .filter_map(|p| match p { Part::FunctionCall { id, .. } => id.clone(), _ => None })
            .next();
        assert_eq!(fc_id, Some(placeholder_id), "FC 本体应回写与占位 FR 相同的 id");
    }

    #[test]
    fn multiple_orphan_fcs_in_one_message_each_backfilled() {
        // 一条 model 消息含多个孤立 FC → 每个都补占位 FR，且按原序
        let multi = Content {
            role: "model".to_string(),
            parts: vec![
                Part::FunctionCall { name: "a".to_string(), args: json!({}), id: Some("ia".to_string()), thought_signature: None },
                Part::FunctionCall { name: "b".to_string(), args: json!({}), id: Some("ib".to_string()), thought_signature: None },
            ],
        };
        let mut conv = vec![text("system", "sys"), multi];
        normalize_function_pairs(&mut conv);
        assert_eq!(conv.len(), 4, "应补 2 条占位 FR（system + model + 2 占位）");
        let ids: Vec<String> = conv[2..].iter()
            .flat_map(|c| c.parts.iter())
            .filter_map(|p| match p { Part::FunctionResponse { id, .. } => id.clone(), _ => None })
            .collect();
        assert_eq!(ids, vec!["ia".to_string(), "ib".to_string()], "占位 FR 按 FC 原序插入");
    }
}
