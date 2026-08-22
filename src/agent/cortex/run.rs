//! `CortexAgent` 的 `Agent::run` 主循环实现。
//!
//! 把 ~1000 行的 run 方法抽到独立文件，减少 `mod.rs` 体积并隔离运行时代码。

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use adk_rust::async_trait;
use adk_rust::serde_json::{Value, json};
use adk_rust::{
    Agent, Content, Event, EventStream, FunctionResponseData, InvocationContext, Part, Result, Tool,
};
use async_stream::stream;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::analytics::emit_compaction;
use super::builder::{MAX_STEPS_PROMPT, CortexAgent};
use super::compaction::{
    build_compaction_event, is_context_window_exceeded, llm_compact, plan_user_retention,
};
use super::context_tool::GetContextRemainingTool;
use super::hook::{CompactionContext, CompactionDecision, CompactionResult};
use super::llm_call::{generate_with_retry, make_text_event};
use super::multi_agent::{AgentTree, ChildAgentFactory, ParentMailbox};
use super::prompt::{StablePrefixParams, build_preamble, build_stable_prefix, build_volatile_context};
use super::soft_landing::{SoftLandingDecision, borrow_message, evaluate_soft_landing, reminder_message};
use super::thinking::{
    clear_thinking_from_config, config_has_thinking, looks_like_thinking_param_error,
};
use super::tool_exec::execute_one_tool_safe;
use super::trim::trim_tool_outputs_to_fit;
use super::window::{WindowState, persist_window};

// ── 窗口压缩阈值比例（对齐 codex，固化为常量不可配）──
/// 软闸：context_window × 0.95，到软闸进入 buffer 区（借一轮/早压缩）
const SOFT_GATE_RATIO: f64 = 0.95;
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
        // 根 run 派生树级 child token，RunEndGuard 收尾只 cancel 它：async 生成器 body
        // 结束即 drop 局部守卫，时序上先于 SSE 侧收尾代码（stream.rs 的落库/兜底推送）——
        // 若直接 cancel SSE 层共享的父 token，自然收尾时 stream.rs 全部 `!is_cancelled()`
        // 门槛恒为假，assistant 正文/token 用量落库自 v1.0.0 起从未执行过（模型回复只在
        // partial=true 增量帧里，runner 按 partial 跳过，手动落库是唯一通道）。
        // child_token 单向级联（父 cancel → 子感知；子 cancel 不触父）：用户停止语义
        // 不变，子 agent 本就经 factory 从本 token 派生，树级联终止覆盖不变。
        // 子 agent（spawn_depth>0）的 self.cancel_token 已是树级 token，原样使用。
        let cancel_token = if self.spawn_depth == 0 {
            self.cancel_token.child_token()
        } else {
            self.cancel_token.clone()
        };
        // steer 队列消费句柄（仅 root run 由 server 层注入；子 agent / 非 SSE run = None）。
        // 运行中提交的用户消息由它在主循环消费（见循环顶 drain 与 fcs 空分支的终局判定）。
        let steer_port = self.steer_port.clone();
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
        all_tools
            .push(Arc::new(GetContextRemainingTool::new(Arc::clone(&budget))) as Arc<dyn Tool>);

        // 内建多智能体 V2 工具集（spawn/send_message/followup_task/wait/interrupt/list）。
        // 子 agent 持久会话 + mailbox 通信（对齐 codex multi_agents_v2）。
        // max_spawn_depth = 0 时禁用本特性。
        let mut parent_mailbox: Option<Arc<ParentMailbox>> = None;
        // spawn fork 快照槽：主循环每轮把 conv 增量写入，spawn_agent 工具读取作 fork 输入。
        let mut conv_snapshot_slot: Option<Arc<StdMutex<Vec<Content>>>> = None;
        if self.context_config.max_spawn_depth > 0 {
            // 树级注册表：root 新建；子 agent 经 blueprint 继承同一棵树（孙 agent 与
            // 全树兄弟可互相寻址，对齐 codex AgentRegistry 单树语义）。
            let tree = match &self.inherited_tree {
                Some(t) => t.clone(),
                None => Arc::new(AgentTree::new(self.context_config.max_concurrent_children)),
            };
            // 本 agent 的 canonical 路径：root = /root；子 agent 经 blueprint 传 canonical。
            let self_path = if self.spawn_depth == 0 {
                "/root".to_string()
            } else {
                self.child_path
                    .clone()
                    .unwrap_or_else(|| "/root".to_string())
            };
            // root 的 mailbox：子 agent FINAL_ANSWER / MESSAGE 投回主循环 conv（每轮 drain）。
            // 子 agent 不建 ParentMailbox——发给它的消息进其 ChildHandle.inbox，
            // 由 run_child_loop 在轮间 drain、由本 run 循环在轮内 drain（self_inbox）。
            let mb = Arc::new(ParentMailbox::new());
            let snapshot: Arc<StdMutex<Vec<Content>>> = Arc::new(StdMutex::new(Vec::new()));
            if self.spawn_depth == 0 {
                // root 绑定收件箱到树（子 agent 显式 send_message("/root") 可寻址投递）
                tree.bind_root_mailbox(mb.clone());
                parent_mailbox = Some(mb.clone());
            }
            let ma_factory = Arc::new(ChildAgentFactory::new(
                self.child_blueprint_with(tree.clone(), mb.clone()),
                tree,
                ctx.clone(),
                cancel_token.clone(),
                self.child_event_sink.clone(),
                self.spawn_depth,
                self.context_config.max_spawn_depth,
                self_path,
                self.agents_config.clone(),
                snapshot.clone(),
                if self.spawn_depth == 0 {
                    Some(mb.clone())
                } else {
                    None
                },
                self.model_resolver.clone(),
            ));
            for t in ma_factory.toolset() {
                all_tools.push(t);
            }
            conv_snapshot_slot = Some(snapshot);
        }
        // 子 agent 轮内 drain 自身 inbox 的句柄（root = None，走 ParentMailbox）
        let self_inbox = self.self_inbox.clone();
        let tool_decls: HashMap<String, Value> = all_tools
            .iter()
            .map(|t| (t.name().to_string(), t.declaration()))
            .collect();
        let tool_map: HashMap<String, Arc<dyn Tool>> = all_tools
            .iter()
            .map(|t| (t.name().to_string(), t.clone()))
            .collect();

        // Build system prompt: stable 前缀（跨请求不变，命中缓存）+ volatile 段（时间，每次刷新）
        let is_subagent = self.spawn_depth > 0;
        let stable_prompt = build_stable_prefix(StablePrefixParams {
            instruction: &self.instruction,
            memory_block: &self.memory_block,
            skill_catalog: &self.skill_catalog,
            policy: self.policy,
            workspace_cwd: self.workspace_cwd.as_deref(),
            max_spawn_depth: self.context_config.max_spawn_depth,
            max_concurrent_children: self.context_config.max_concurrent_children,
            mode_hint: super::multi_agent_mode_hint(
                self.context_config.multi_agent_mode,
                self.session_thinking_level.as_deref(),
            ),
            is_subagent,
        });
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
        let reminder_threshold_tokens =
            ((context_window as f64) * REMINDER_THRESHOLD_RATIO) as usize;
        // 会话级窗口状态句柄（root run 由 SSE 按 thread_id 注入；子 agent None → 独立窗口）。
        // run 开始先恢复（对齐 codex AutoCompactWindow::restore）——软着陆 flag「每窗一次」
        // 跨 run 存活，否则退化为「每个用户轮次一次」→ 无限借轮、压缩永不触发。
        let session_window = self.window_state.clone();
        let mut window = match &session_window {
            Some(shared) => {
                let snap = *shared.lock().unwrap_or_else(|e| e.into_inner());
                WindowState::restore(snap)
            }
            None => WindowState::new(),
        };
        // ★ 跨 run usage 种子：上一 run 最后一次请求的 gross total_tokens。
        // last_usage_tokens 是 run 内局部量，没有种子则每 run 从 None 起，
        // 闸门判定只剩字符估算（FC args 记 64 严重低估工具密集会话）→
        // borrow 后 run 一结束，下一 run 永远够不着硬闸、压缩不触发。
        // 种子无 mark → effective = t 本身：total 含末轮 completion，恰覆盖
        // run 开头的 preamble+history（差量下轮 usage 自然校正）。
        let seed_last_usage = window.last_usage_total;
        // ★ 模型切换失配检测：会话不随模型切换重建（按 thread_id 存），种子是旧模型
        // 下的占用，闸门却按本次 run 的模型算。切到小窗口模型时（如 1M 用到 500K 后
        // 切 128K），占用对旧窗口只是 50%、对新窗口已超硬闸 → 首循环即 ForceCompact。
        // 此处识别并：① warn 暴露；② 合成 usage-only 首帧让 SSE 立刻按新窗口推
        // CONTEXT_USAGE（前端剩余百分比即刻刷新，压缩不再「看似凭空」触发）；③ 重置
        // per-window 一次性 flag——切模型视同开新窗，flag 语义重新绑定新窗口。
        let mut emit_seed_usage_event = false;
        if let Some(seed) = seed_last_usage
            && let Some(seed_window) = window.context_window_at_seed
            && seed_window != context_window
        {
            let seed_pct = (seed.max(0) as f64 / seed_window as f64 * 100.0).round() as i64;
            let new_pct = (seed.max(0) as f64 / context_window as f64 * 100.0).round() as i64;
            tracing::warn!(
                "[cortex_agent] 模型切换窗口失配：种子 total={} 是窗口 {}（占用 ~{}%）下记录的，\
                 本次 run 窗口 {}（占用 ~{}%），软闸 {soft_gate}",
                seed, seed_window, seed_pct, context_window, new_pct
            );
            emit_seed_usage_event = true;
            // 失配即视同开新窗：无条件重置 per-window 一次性 flag。flag 的「每窗一次」
            // 语义绑定当前窗口——旧 flag 属于旧模型的窗口：如旧窗末段已 reminder_shown、
            // 切大窗后模型接近新窗闸线时会被旧 flag 挡掉第一次提醒（borrowed 同理，
            // 虽然软=硬闸下 buffer 区为空、borrow 分支当前不可达，语义仍应对齐）。
            // 种子已超新窗硬闸时 ForceCompact 照常立即触发（无需 flag 参与）——
            // 历史本就发不进小窗请求，此为正确行为。
            window.reminder_shown = false;
            window.borrowed = false;
            persist_window(&session_window, &window);
        }
        // 初始化预算快照的静态字段（gates/context_window/window 号/压缩计数）；effective_tokens
        // 由主循环每轮刷新，get_context_remaining 工具据此回答模型。
        {
            let mut b = budget.write().expect("budget lock poisoned");
            b.context_window = context_window;
            b.soft_gate = soft_gate;
            b.hard_gate = hard_gate;
            b.window_number = window.window_number;
            b.compaction_count = window.compaction_count;
        }
        let compact_model = self.compact_model.clone();
        let hooks = self.hooks.clone();
        // 子 agent 用量出口：仅子 agent（spawn_depth>0）把每次请求末帧真实 usage 累计进共享
        // 计数（父不写 —— 父用量由 SSE 从主事件流读取，数据源不相交 → 不双重计数）。
        let usage_out = (self.spawn_depth > 0).then(|| self.child_usage_total.clone());

        // Run 收尾 guard（仅 root，对齐 codex「root session 结束才终止整棵树」）：
        // 流 drop/结束时 cancel 本 run 的 token → child_token 级联终止全部子 agent。
        // 子 agent 不挂（其 turn 结束不杀持久会话——followup 复活依赖它）。
        let run_guard = (self.spawn_depth == 0).then(|| RunEndGuard {
            token: cancel_token.clone(),
        });

        let s = stream! {
            let _run_guard = run_guard;
            let mut iteration = 0u32;
            // 上一轮模型返回的 interaction_id，用于本次请求的 previous_response_id（链式续接，
            // 可省去重复 prefill 的 token）。注意：本项目当前用 OpenAI 兼容 / Anthropic 协议，
            // 这两类 client 不 populate interaction_id（恒 None），故此处目前为 no-op；
            // 切到 Gemini Interactions 协议时才生效。
            let mut last_interaction_id: Option<String> = None;
            // 上一轮模型返回的真实 token 用量（来自 usage_metadata），优先于字符估算。
            // run 开头用会话级种子（上一 run 末次请求的 gross total）——见上方 seed 注释。
            let mut last_usage_tokens: Option<i32> = seed_last_usage;
            // 上一轮缓存命中 token（仅压缩埋点参考；不参与占用量判定——见下方口径注释）
            let mut last_cache_read: Option<i32> = None;
            // usage 采样点的 conv 代际与长度 (epoch, len)：占用量 = 该次 total_tokens +
            // conv[len..] 的字符估算（该响应覆盖到 len，之后追加的条目是增量）。
            // 代际变化（压缩/超窗删条重建 conv）→ 下标失效，弃用增量、等下一笔 usage 重置。
            let mut last_usage_mark: Option<(u64, usize)> = None;
            // chunk 循环里见到有效 usage 的标记（响应 Content 此时尚未 push 进 conv，
            // 采样点须等 push 后再记——conv.len() 才含本响应）。
            let mut saw_usage_this_request = false;
            // 思考参数兜底：模型不支持 thinking/effort/reasoning_effort 时，去参数重试一次（本次 run 内）
            let mut thinking_retry_done = false;
            let mut conv: Vec<Content> = preamble;
            // steer 暂存（对齐 codex pending_input 的「队列 → 注入」两级缓冲）：
            // 模型回合结束时 `finish` 已取走的排队项先入 stash，下一轮循环顶注入 ——
            // 注入点收敛在循环顶（与 mailbox 同构），stash 先于新 drain（FIFO 保序）。
            let mut steer_stash: Vec<Content> = Vec::new();
            // 持久历史条数（快照基线）：fork 快照只存 run 增量，spawn 时与 parent_history
            // 拼接才不会双份历史（factory 侧 fork_history 会拼 parent_history）。
            let history_len = history.len();
            conv.extend(history);
            // 快照增量追踪状态（epoch=conv 代际，压缩/删条 +1；copied_len=已追加的绝对下标）
            #[derive(Clone, Copy)]
            struct SnapState { epoch: u64, copied_len: usize }
            let mut snapshot_state = SnapState { epoch: 0, copied_len: preamble_len + history_len };
            let mut conv_epoch: u64 = 0;

            // ★ 模型切换失配时（见 run 开头 emit_seed_usage_event 注释）：合成 usage-only
            // 首帧，SSE emit_usage 按新窗口立算 remaining 推 CONTEXT_USAGE——前端剩余
            // 百分比即刻从旧模型的值刷新为新窗口口径，首循环 ForceCompact 不再「看似
            // 凭空」。content=None + turn_complete=true，SSE 只取 usage 不渲染气泡；
            // runner 落库后经回放边界跳过（end_timestamp 同刻），不污染历史。
            if emit_seed_usage_event {
                let mut ev = Event::new(&invocation_id);
                ev.author = agent_name.clone();
                ev.llm_response.usage_metadata = Some(adk_rust::UsageMetadata {
                    total_token_count: seed_last_usage.unwrap_or_default().max(0),
                    ..Default::default()
                });
                ev.llm_response.turn_complete = true;
                yield Ok(ev);
            }

            'turn: loop {
                iteration += 1;

                // ===== mailbox 消费（对齐 codex：子 agent 完成回投 FINAL_ANSWER，随下轮注入 conv）=====
                if let Some(mb) = &parent_mailbox {
                    for rendered in mb.drain() {
                        // yield user-role 事件持久化（提前收尾时消息经 runner 落库不丢）。
                        // author 固定 "user"：SSE 层据此跳过助手正文累积/气泡推送——
                        // inter-agent 信封是模型间通信（codex 走 analysis channel），
                        // 不能泄进用户可见的助手回复。
                        let mut ev = Event::new(&invocation_id);
                        ev.author = "user".to_string();
                        ev.llm_response.content = Some(Content {
                            role: "user".to_string(),
                            parts: vec![Part::Text { text: rendered.clone() }],
                        });
                        yield Ok(ev);
                        conv.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part::Text { text: rendered }],
                        });
                    }
                }
                // 子 agent 轮内消费自身 inbox（孙 agent 的 FINAL_ANSWER / 兄弟 MESSAGE；
                // 对齐 codex「turn 内消息边界 drain」——root 走上面的 ParentMailbox，
                // 子 agent 走 ChildHandle.inbox，二者互斥）。注入同时记 injected_log，
                // 由 run_child_loop 在 turn 结束后落回 session（局部 conv 不持久化）。
                if let Some(inbox_handle) = &self_inbox {
                    let drained: Vec<String> = {
                        let mut q = inbox_handle.inbox.lock().expect("child inbox poisoned");
                        q.drain(..).map(|i| i.rendered).collect()
                    };
                    if !drained.is_empty() {
                        let mut log = inbox_handle
                            .injected_log
                            .lock()
                            .expect("injected log poisoned");
                        for rendered in drained {
                            let c = Content {
                                role: "user".to_string(),
                                parts: vec![Part::Text { text: rendered }],
                            };
                            conv.push(c.clone());
                            log.push(c);
                        }
                    }
                }
                // ===== steer 消费（对齐 codex：运行中提交的用户消息，随下轮注入 conv）=====
                // codex 在 run_turn 每次迭代开头 drain pending_input 并记录进历史；此处同构：
                // stash（上轮 finish 已取走的）先注入，再 drain 新到的。yield user-role 事件由
                // runner 落库持久化；SSE 层按 author=user 跳过前端气泡（前端提交时已本地渲染，
                // 与 mailbox 注入同一约定）。上一个 run 遗留的排队项也会在此被消费。
                for content in steer_stash.drain(..) {
                    let mut ev = Event::new(&invocation_id);
                    ev.author = "user".to_string();
                    ev.llm_response.content = Some(content.clone());
                    yield Ok(ev);
                    conv.push(content);
                }
                if let Some(port) = &steer_port {
                    for item in port.drain().await {
                        let mut ev = Event::new(&invocation_id);
                        ev.author = "user".to_string();
                        ev.llm_response.content = Some(item.content.clone());
                        yield Ok(ev);
                        conv.push(item.content);
                    }
                }
                // spawn fork 快照刷新（增量追加）：只 append 新消息——每轮全量 clone 长会话
                // 增量区是 O(N²) memcpy，而 spawn 大概率不读。
                // 压缩/超窗删条使 conv 变短：快照保留已落定的消息不删（对齐 codex——fork 读
                // rollout 持久层，压缩是上下文层操作，只追加 Compacted 检查点、不删旧条目），
                // 仅重置 copied_len 继续追加。
                if let Some(slot) = &conv_snapshot_slot {
                    let mut guard = slot.lock().expect("conv snapshot poisoned");
                    let mut st = snapshot_state;
                    if conv_epoch != st.epoch {
                        // 代际变化（压缩重建/删条）：绝对下标失效，全量重拷
                        *guard = conv[(preamble_len + history_len).min(conv.len())..].to_vec();
                        st.epoch = conv_epoch;
                        st.copied_len = conv.len();
                        snapshot_state = st;
                    } else if conv.len() > st.copied_len {
                        guard.extend_from_slice(&conv[st.copied_len..]);
                        st.copied_len = conv.len();
                        snapshot_state = st;
                    }
                }

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
                    super::normalize_function_pairs(&mut conv);
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

                // ===== 上下文治理：token 占用估算 + 三级软着陆 + 可回放压缩 =====
                // 口径对齐 codex Total scope（context_manager/history.rs get_total_token_usage）：
                // 占用 = 上一响应的 total_tokens（gross，不减 cache_read）+ 该响应之后新增
                // 条目的字符估算。gross 单调、与 provider prompt cache 解耦——缓存命中并不
                // 减小真实请求体积；旧「净 token = total − cache_read」会随缓存命中波动，
                // 在软/硬闸间振荡，导致硬闸永远够不着、压缩永不触发（且 run 开头的字符
                // 估算是 gross 口径，两口径来回切更放大振荡）。无 usage（run 开头）回退
                // 全量字符估算；cache_read 仅留作压缩埋点参考。
                let effective_tokens: usize = match (last_usage_tokens, last_usage_mark) {
                    (Some(t), Some((epoch, len))) if epoch == conv_epoch => {
                        let growth = if len < conv.len() {
                            super::estimate_conv_tokens(&conv[len..], chars_per_token)
                        } else {
                            0
                        };
                        (t.max(0) as usize).saturating_add(growth)
                    }
                    (Some(t), _) => t.max(0) as usize,
                    (None, _) => super::estimate_conv_tokens(&conv, chars_per_token),
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
                        persist_window(&session_window, &window);
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
                        persist_window(&session_window, &window);
                        tracing::info!(
                            "[cortex_agent] 软着陆：借最后一轮（buffer 区，窗口 #{}）",
                            window.window_number
                        );
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
                            // L2 历史级裁剪：先把超大的旧工具输出截短，摘要请求与保留历史更小。
                            // 对齐 codex（run_pre_sampling_compact → run_auto_compact 无条件压缩）：
                            // trim 只服务于压缩本身，**没有「裁完够了就跳过压缩」的逃生门**。
                            // 旧逃生门用字符估算口径判 under_budget，与 ForceCompact 的真实
                            // usage 触发口径分叉——中文/工具密集会话估算长期低估真实占用，
                            // 「真实已超、估算未超」时静默跳过整个压缩分支且无任何日志
                            // （borrow 后下一 run 压缩被吞、前端持续超阈值的根因）。
                            let (trim_stats, _) =
                                trim_tool_outputs_to_fit(&mut conv, preamble_len, soft_gate, chars_per_token);
                            if trim_stats.trimmed_outputs > 0 {
                                tracing::info!(
                                    "[cortex_agent] 历史裁剪：{} 条工具输出截短，去除 ~{} 字节",
                                    trim_stats.trimmed_outputs, trim_stats.chars_removed
                                );
                            }

                                let older: Vec<Content> = conv[preamble_len..split_point].to_vec();
                                let tail: Vec<Content> = conv[split_point..].to_vec();

                                // 旧 user 消息原文保留（按预算从后往前），旧非 user（含上一轮摘要）摘要成一条。
                                // 注意：旧摘要（model role）必须纳入 to_summarize 再摘要，否则重复压缩时
                                // 上一轮的进度/决策会被静默丢弃（渐进失忆）；接受「摘要的摘要」级联——
                                // 轻微失真远优于完全丢失。
                                //
                                // 信封保留语义（NEW_TASK 保留、MESSAGE/FINAL_ANSWER 与超预算条目只摘要）
                                // 见 [`plan_user_retention`]——对齐 codex compact_remote_v2 +
                                // 4f6d06d485；未保留的 user 条目并入 to_summarize，历史不静默丢失
                                // （旧实现 break 掉整个扫描 + user 恒不进摘要 = 双重丢失）。
                                let user_budget_chars = 80_000usize;
                                // 单条 NEW_TASK 信封上限（10k token 换算字符；chars_per_token
                                // 与 estimate_conv_tokens 同源）
                                let envelope_cap_chars = 10_000usize.saturating_mul(chars_per_token.max(1));
                                let retain = plan_user_retention(
                                    &older,
                                    user_budget_chars,
                                    envelope_cap_chars,
                                );
                                let retained_users: Vec<Content> = older.iter().zip(&retain)
                                    .filter(|(_, r)| **r)
                                    .map(|(c, _)| c.clone())
                                    .collect();

                                let to_summarize: Vec<Content> = older.iter().zip(&retain)
                                    .filter(|(c, r)| c.role != "user" || !**r)
                                    .map(|(c, _)| c.clone())
                                    .collect();
                                let summarize_count = to_summarize.len();
                                let summary = if to_summarize.is_empty() {
                                    // older 全是**被保留**的 user 消息（非 user 与被丢弃的 user 都已并入
                                    // to_summarize）：无东西可摘要，但窗口仍推进、历史仍重写，
                                    // 用占位摘要保证检查点事件照发（前端清 floor、compaction_count
                                    // 连续、下次 run 回放有边界）。带统一前缀 → is_summary_content
                                    // 可识别，连续压缩时并入 to_summarize 再摘要。
                                    format!(
                                        "{} [Context compacted: the trimmed range contained only user \
                                         messages, which were dropped; later user messages are retained.]",
                                        crate::prompts::COMPACT_SUMMARY_PREFIX
                                    )
                                } else {
                                    llm_compact(&model, compact_model.as_ref(), &to_summarize, &cancel_token).await
                                };

                                // 取消则不落库半截摘要
                                if cancel_token.is_cancelled() {
                                    tracing::info!("[cortex_agent] 压缩期间用户取消，不持久化摘要");
                                    break;
                                }

                                // 重建：[preamble(stable+volatile), summary?, ...retained_users, ...tail]
                                // 代际 +1：conv 被重建，快照的绝对下标（copied_len）失效，
                                // 下次刷新须全量重拷（净长度判断不可靠——回缩后同轮追加
                                // 越过旧 copied_len 会错位追加错误切片）。
                                conv_epoch += 1;
                                let preamble_msgs: Vec<Content> = conv[..preamble_len].to_vec();
                                conv.clear();
                                conv.extend(preamble_msgs);
                                // 开新窗 + 预算快照即时刷新（必须先于下方 yield：SSE 消费压缩
                                // 事件时 on_compaction 会读快照取 compaction_count），
                                // 并写回会话级窗口句柄（跨 run 持久）。
                                window.advance();
                                persist_window(&session_window, &window);
                                {
                                    let mut b = budget.write().expect("budget lock poisoned");
                                    b.window_number = window.window_number;
                                    b.compaction_count = window.compaction_count;
                                }
                                // 摘要落 conv + ★ 可回放：yield compaction 检查点事件。
                                // 框架自动持久化（runner 非partial Event 落库）+ 下次 turn 经
                                // conversation_history_for_agent_impl 以本条为回放边界。summary 恒
                                // 非空（llm_compact 失败有兜底文案；older 全 user 时上方占位摘要），
                                // 不发事件会导致前端 floor 不清、compaction_count 跳号、回放无边界。
                                conv.push(Content {
                                    role: "model".to_string(),
                                    parts: vec![Part::Text { text: summary.clone() }],
                                });
                                yield Ok(build_compaction_event(&invocation_id, summary));
                                conv.extend(retained_users);
                                conv.extend(tail);

                                let after_tokens = super::estimate_conv_tokens(&conv, chars_per_token);
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
                                    "[cortex_agent] compacted {summarize_count} msgs (non-user + dropped users) into summary, window #{}",
                                    window.window_number
                                );
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
                    b.compaction_count = window.compaction_count;
                }

                // 请求级 FC/FR 配对归一化（对标 codex ensure_call_outputs_present +
                // remove_orphan_outputs）：压缩/删条/回滚可能破坏 FunctionCall/Response 配对，
                // 发请求前清理——删孤立 FunctionResponse、为孤立 FunctionCall 补占位 FunctionResponse，
                // 避免触发严格模式 400（修高危④）。
                super::normalize_function_pairs(&mut conv);

                let request = adk_rust::LlmRequest {
                    model: model.name().to_string(),
                    contents: conv.clone(),
                    config: config.clone(),
                    tools: tool_decls.clone(),
                    previous_response_id: last_interaction_id.clone(),
                };

                // 建连 + 超窗兜底：超窗时钉满占用转压缩分支（见下方 is_context_window_exceeded）。
                let mut stream = match generate_with_retry(&model, request.clone(), llm_timeout, 3, &cancel_token).await {
                    Ok(s) => s,
                    Err(e) => {
                        // cancel 引起的 Err 静默退出，不发"调用失败"错误文本（用户点停止不应看到错误）
                        if cancel_token.is_cancelled() {
                            tracing::info!("[cortex_agent] agent 因用户取消退出（LLM 建连阶段）");
                            return;
                        }
                        // 超窗兜底（对齐 codex ContextWindowExceeded → set_total_tokens_full +
                        // 下轮强制压缩，turn.rs）：把占用钉到窗口满格并持久种子，跳回循环顶
                        // —— 硬闸必命中，走「L2 裁剪 + LLM 摘要」的正式压缩，历史进摘要。
                        // 旧实现「静默删最旧一条重试」有两重祸：删后 usage 回落到软闸之下，
                        // 压缩永远不触发；且用户毫无感知地永久丢历史。
                        if is_context_window_exceeded(&e)
                            && conv.len() > preamble_len + 1
                        {
                            last_usage_tokens = Some(context_window as i32);
                            last_usage_mark = None;
                            window.last_usage_total = last_usage_tokens;
                            // 与正常 usage 持久点同构：记录种子窗口，否则此路径的种子
                            // 在下一 run 会静默跳过模型切换失配检测。
                            window.context_window_at_seed = Some(context_window);
                            persist_window(&session_window, &window);
                            tracing::warn!(
                                "[cortex_agent] 上下文超窗（window={}），转正式压缩分支（当前 {} 条）",
                                context_window,
                                conv.len()
                            );
                            continue 'turn;
                        }
                        tracing::error!("[cortex_agent] LLM 调用最终失败: {e}");
                        yield Ok(make_text_event(&invocation_id, &agent_name, "[LLM call failed after retries.]"));
                        return;
                    }
                };

                let mut parts: Vec<Part> = Vec::new();
                // 本请求的真实 usage（末帧 last-wins：流式下末帧即该请求总量，
                // 中间帧赋值覆盖 → 请求结束时加一次，不重复计数）。
                let mut req_usage: u64 = 0;

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
                    // 记录模型返回的真实 token 用量 + 缓存命中（BodyAfterPrefix），优先用于 compaction 判定。
                    // 仅当 total>0 才采纳：部分 provider 流式回 usage 但字段恒为 0（占位），
                    // 若存 Some(0) 会把下轮 effective_tokens 钉成 0 → 预算估算与 token 显示全归零。
                    if let Some(u) = &chunk.usage_metadata
                        && u.total_token_count > 0
                    {
                        last_usage_tokens = Some(u.total_token_count);
                        last_cache_read = u.cache_read_input_token_count;
                        // 跨 run usage 种子在此即刻持久（不等 push 后）：usage-only 末帧
                        // （content=None → parts 空）会走下方 parts.is_empty() 的提前
                        // break，push 后的持久点不可达 → 种子丢失、下一 run 退回估算，
                        // 而 SSE 已从事件取到同一 usage 照常显示 —— 前端显示已过阈值、
                        // 闸门却够不着的口径分叉即此。
                        window.last_usage_total = last_usage_tokens;
                        // 同步记录种子窗口：下一 run 若换了模型（窗口不同），开头可据此
                        // 识别「占用对旧窗口未满、对新窗口已超闸」的失配并告警。
                        window.context_window_at_seed = Some(context_window);
                        persist_window(&session_window, &window);
                        // 本请求见过有效 usage：等响应 Content push 进 conv 后再记采样点
                        // （push 前的 conv.len() 不含本响应，total 却已包含）。
                        saw_usage_this_request = true;
                        // 子 agent 计数用净口径（total − cache_read）。与主会话 effective_tokens
                        // 的 gross 口径**刻意不同**：子 agent 计数是「计费/花费」语义（前端
                        // 「+子任务 N」），毛值含缓存命中前缀，长前缀子 agent 每轮虚增一个
                        // cache_read 量级，「已用 30k (+子任务 180k)」误导；主会话是「上下文
                        // 占用」语义（压缩判定 + 进度条），gross 才单调可信。
                        req_usage = (u.total_token_count.max(0) as u64)
                            .saturating_sub(u.cache_read_input_token_count.unwrap_or(0).max(0) as u64);
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

                // 本请求真实 usage 计入共享累加器（仅子 agent；每请求末帧加一次，
                // 供 SSE 随 CONTEXT_USAGE 上报子 agent 总花费）。
                if let Some(acc) = &usage_out {
                    acc.fetch_add(req_usage, std::sync::atomic::Ordering::Relaxed);
                }

                // （重复退化重导向 / 硬跳过分支已移除：见 builder.rs 文件头说明。循环回归协议驱动。）

                // 无 id 的 FC（文本标签/弱供应商解析产生 id=None）：push conv 前补全局唯一合成 id，
                // 否则回填的 FR 拿到空 id、normalize 会把它当孤立 FR 误删 → 触发严格模式 400。
                // 用全局单调计数器（跨轮/跨 run 唯一），避免局部序号在不同迭代重复导致 normalize 错配。
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

                // usage 采样点：本响应已 push 进 conv，total_tokens 覆盖 conv[..len]；
                // conv[len..] 即后续新增（工具响应 / steer 注入 / 下一轮用户消息）。
                // 种子持久已在 chunk 循环内 usage 到达时即刻完成（见上），此处只记 mark。
                if saw_usage_this_request {
                    last_usage_mark = Some((conv_epoch, conv.len()));
                    saw_usage_this_request = false;
                }

                let fcs: Vec<(String, Value, Option<String>)> = content.parts.iter()
                    .filter_map(|p| match p { Part::FunctionCall { name, args, id, .. } => Some((name.clone(), args.clone(), id.clone())), _ => None }).collect();

                if fcs.is_empty() {
                    // 纯文本输出、流正常结束 → 终止 turn（模型本轮未调用工具即视为回答完成）。
                    // 例外（对齐 codex `needs_follow_up = model_needs_follow_up || has_pending_input`）：
                    // steer 队列还有运行中提交的用户消息 → finish 原子取走（封「提交恰逢收尾」
                    // 竞态），入 stash 后 continue，下一轮循环顶注入再请求模型。
                    // 队列空 → finish 只标记 draining（注销延迟到流侧持久化完成后——
                    // assistant 正文此刻未落库，早注销会让紧接着的新 run 读到缺尾历史）；
                    // 已取消 → 清队列 + 注销。
                    let mut steer_continue = false;
                    if let Some(port) = &steer_port {
                        if let crate::infra::run_registry::SteerFinish::Continue(items) =
                            port.finish(cancel_token.is_cancelled()).await
                        {
                            steer_stash.extend(items.into_iter().map(|i| i.content));
                            steer_continue = true;
                            tracing::info!(
                                "[cortex_agent] steer 续跑：注入 {} 条运行中提交的输入",
                                steer_stash.len()
                            );
                        }
                    }
                    if steer_continue {
                        continue;
                    }
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
                // 工具结果回填后刷新 fork 快照（增量追加）：同 iteration 内后续 spawn 能
                // 拿到含本轮模型输出+工具结果的最新 conv。压缩/删条走代际全量重拷。
                if let Some(slot) = &conv_snapshot_slot {
                    let mut guard = slot.lock().expect("conv snapshot poisoned");
                    let mut st = snapshot_state;
                    if conv_epoch != st.epoch {
                        *guard = conv[(preamble_len + history_len).min(conv.len())..].to_vec();
                        st.epoch = conv_epoch;
                        st.copied_len = conv.len();
                        snapshot_state = st;
                    } else if conv.len() > st.copied_len {
                        guard.extend_from_slice(&conv[st.copied_len..]);
                        st.copied_len = conv.len();
                        snapshot_state = st;
                    }
                }
            }

            // ===== run 收尾：最后 drain 一次 root mailbox =====
            // 对齐 codex「root turn 结束后未消费消息留在 input_queue、下次 turn drain」的
            // 持久化等价物：模型没 wait_agent 就收尾时，子 agent 已投递的 FINAL_ANSWER
            // 会随树消亡——收尾前 yield 成 user 事件走既有持久化路径（下次 run 的
            // conversation_history 仍可见），钱不白花。
            if let Some(mb) = &parent_mailbox {
                for rendered in mb.drain() {
                    let mut ev = Event::new(&invocation_id);
                    ev.author = "user".to_string();
                    ev.llm_response.content = Some(Content {
                        role: "user".to_string(),
                        parts: vec![Part::Text { text: rendered }],
                    });
                    yield Ok(ev);
                }
            }
        };

        Ok(Box::pin(s))
    }
}

/// run 流收尾 guard：drop 时 cancel 本 run 的 token，级联终止全部子 agent。
/// （多智能体 V2：spawn 出的后台会话循环与 detached task 靠 cancel_token 级联收尾。）
struct RunEndGuard {
    token: CancellationToken,
}
impl Drop for RunEndGuard {
    fn drop(&mut self) {
        self.token.cancel();
    }
}
