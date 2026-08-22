//! SSE 事件流核心：消费 adk-rust Runner 事件流，转换为前端 SSE 事件。
//!
//! [`create_event_stream`] 在独立 tokio 任务中运行 Agent；事件分派逻辑由
//! [`EventSink`] 承载——它收敛一次 run 内的可变状态（当前文本/思考块 id、
//! 已抑制的纯下载标记命令、累积的 assistant 正文等），并把每种 Part 的发送
//! 逻辑拆成独立方法，避免主循环膨胀。控制流（confirmation 的 return、
//! compaction / suppress 的 continue）留在主循环，用方法返回值表达。

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, RwLock};

use axum::response::sse::Event as SseEvent;
use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio_stream::StreamExt;
use tracing::Instrument;

use adk_rust::agent::Agent;

use super::attachment::build_user_content;
use super::error::send_run_error;
use super::screenshot;
use super::tool_display::{
    is_pure_artifact_command, mcp_server_name, strip_artifact_markers, tool_display_name,
};
use super::types::{InputMessage, SseEventMsg};
use crate::agent::cortex::SharedBudget;
use crate::server::AppState;

/// 系统提示词等固定开销（对齐 codex BASELINE_TOKENS）：计算剩余百分比时扣除，
/// 使显示反映「有效内容」占用而非包含基线的 gross 值。
const BASELINE_TOKENS: u64 = 12_000;

/// 对齐 codex `percent_of_context_window_remaining`：减去基线后计算剩余百分比，clamp(0,100)。
fn calc_context_remaining_percent(effective: u64, context_window: u64) -> u8 {
    if context_window <= BASELINE_TOKENS {
        return 0;
    }
    let effective_window = context_window - BASELINE_TOKENS;
    let used = effective.saturating_sub(BASELINE_TOKENS);
    let remaining = effective_window.saturating_sub(used);
    ((remaining as f64 / effective_window as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round() as u8
}

/// 一次 run 内的 SSE 事件状态机：持有发送通道与可变状态，按 Part 类型分发发送逻辑。
struct EventSink<'a> {
    // 只读上下文
    tx: &'a Sender<SseEvent>,
    thread_id: &'a str,
    run_id: &'a str,
    mcp_slug_to_name: &'a HashMap<String, String>,
    state: &'a AppState,
    budget_handle: &'a Option<SharedBudget>,
    /// 子 agent token 用量累加器（只读）：子 agent run 循环写入，这里随 CONTEXT_USAGE 上报。
    child_usage: &'a Option<crate::agent::cortex::ChildUsageTotal>,
    // 可变状态
    current_msg_id: String,
    text_open: bool,
    current_thinking_id: String,
    thinking_open: bool,
    assistant_text: String,
    agent_author: String,
    /// 本轮最后一次实际推送的 (total, threshold, window_size) 快照（占用口径，压缩后回落）。
    /// run 结束时据此落库（对齐 codex 会话级 token_info 持久化，重进会话恢复显示）。
    last_usage_snapshot: Option<(u64, u64, u64)>,
    /// 被抑制的纯下载标记命令 call_id 集合：FunctionCall 跳过发送时记入，
    /// 配对的 FunctionResponse 据此跳过 RESULT，使整条工具在前端不可见。
    suppressed_call_ids: Arc<RwLock<HashSet<String>>>,
    /// 在途工具调用（已发 TOOL_CALL_START、尚未收到 RESULT）的 call_id→tool_name。
    /// 流结束时（尤其用户取消）若仍有残留，说明工具结果未回流（runner 被取消打断），
    /// 需补发一条 RESULT 把前端状态从 running 翻成终态，否则永久卡「沙箱执行中」。
    in_flight_tool_calls: HashMap<String, String>,
}

impl<'a> EventSink<'a> {
    /// 推送一条 SSE 事件。
    async fn send(&self, msg: SseEventMsg) {
        let _ = self
            .tx
            .send(SseEvent::default().data(msg.to_sse_data()))
            .await;
    }

    /// 关闭打开的文本块（发 `TextMessageEnd`）。
    async fn close_text(&mut self) {
        if self.text_open {
            self.send(SseEventMsg::TextMessageEnd {
                message_id: self.current_msg_id.clone(),
            })
            .await;
            self.text_open = false;
        }
    }

    /// 关闭打开的思考块（发 `ThinkingMessageEnd`）。
    async fn close_thinking(&mut self) {
        if self.thinking_open {
            self.send(SseEventMsg::ThinkingMessageEnd {
                message_id: self.current_thinking_id.clone(),
            })
            .await;
            self.thinking_open = false;
        }
    }

    /// Token 用量上报（进度条语义 = 当前上下文占用，与软着陆判定同口径）。
    ///
    /// `total_tokens` = 本帧真实 usage 的 gross total（prompt+completion，不减
    /// cache_read）——与主循环 effective_tokens / codex Total scope 同口径，压缩后
    /// 随上下文变小自然回落（前端 floor 闸在 CONTEXT_COMPACTED 清零放行）。
    /// 旧实现发「会话累计高水位」（max 闸）：缓存丢失轮的全量净值钉死峰值后
    /// 永不回落，压缩又因软着陆 bug 从不触发 → 「138K/115K」卡死超 100%。
    ///
    /// `session_total_tokens` = 会话累计消耗（计费语义，`session_token_usage` max 闸，
    /// `on_compaction` 清零，对齐 codex token_info 随历史重写）；前端暂不渲染。
    /// 无 usage 的 provider（mimo）→ run 收尾由 budget 快照（gross 估算）兜底推送。
    async fn emit_usage(&mut self, event: &adk_rust::Event) {
        let evt_total = event
            .llm_response
            .usage_metadata
            .as_ref()
            .map(|u| u.total_token_count);
        let real = event
            .llm_response
            .usage_metadata
            .as_ref()
            .filter(|u| u.total_token_count > 0);

        // 会话级累计（计费语义）：真实 usage 才入累计；budget 是估算、不入。
        let mut cumulative_guard = self.state.session_token_usage.lock().await;
        let cumulative = cumulative_guard
            .entry(self.thread_id.to_string())
            .or_insert(0);

        let (usage_total, usage_prompt, usage_completion) = if let Some(usage) = real {
            // 真实 usage：发 gross 当轮值（占用口径，与压缩判定一致）；
            // 累计闸只喂 session_total_tokens（计费口径）。
            let p = usage.prompt_token_count.max(0) as u64;
            let c = usage.candidates_token_count.max(0) as u64;
            let t = usage.total_token_count.max(0) as u64;
            if t > *cumulative {
                *cumulative = t;
            }
            (t, p, c)
        } else if *cumulative > 0 {
            // 中间帧但本会话曾有过真实 usage → 沿用累计值（仅用于占用为 0 的兜底判定，
            // 中间帧一律不推，见下方；不发 budget 低估算）。
            (*cumulative, 0, 0)
        } else if let Some(budget) = self.budget_handle
            && let Ok(snap) = budget.read()
            && snap.effective_tokens > 0
        {
            // 从未返回 usage（mimo）→ budget 的 effective_tokens（内容只增 → 单调）。
            (snap.effective_tokens as u64, 0, 0)
        } else {
            (0u64, 0u64, 0u64)
        };
        let cumulative_val = *cumulative;
        drop(cumulative_guard);

        // 诊断（定位「token 用量恒为 0」）：记录每帧来源——
        //   evt_total=事件自带 usage（None=本帧无 usage）/ cumulative=会话累计最大真实值 /
        //   budget_eff=budget 字符估算。据此判断 0 的根因是 provider 不回 usage 还是估算失效。
        let budget_eff = self
            .budget_handle
            .as_ref()
            .and_then(|b| b.read().ok())
            .map(|s| s.effective_tokens)
            .unwrap_or(0);
        if usage_total > 0 {
            // 流式中间帧一律不推(严格对齐 codex:流式 chunk 期间零 TokenCount,
            // 响应 Completed 才置 should_emit 标志、收尾发一次;RateLimits 分支注释
            // 'defer sending until token usage is available to avoid duplicate
            // TokenCount events')。判定 = 本帧无真实 usage(evt_total 无效):
            //   - cumulative>0 的沿用帧 → 中间帧,不推(此前值不变跳过是错的对齐——
            //     mimo budget 估算流式期间持续增长,值变即推,一个 chunk 一条);
            //   - budget 估算帧同样只在响应边界有效,中间帧不推。
            // 真实 usage 帧(evt_total 有效,即响应完成)才推;mimo 无 usage 的会话
            // 在 run 结束落库时仍有 last_usage_snapshot 兜底(见下方 Completed 处理)。
            if evt_total.is_none() || evt_total == Some(0) {
                tracing::trace!(
                    "[emit_usage] 流式中间帧(evt_total 无效),不推送: cumulative={} budget_eff={}",
                    cumulative_val,
                    budget_eff
                );
                return;
            }
            // 进度条分母 = 压缩软闸（context_window × 0.95，对齐 codex 窗口 95% 触发点）。
            // 从 budget 快照读；首帧尚无快照时回退 fallback 窗口 × 0.9。
            // 同时取 context_window 用于 cap total_tokens：borrow 期间真实 usage 可
            // 短暂超过窗口总量，前端显示 "59K/58K" 观感差，cap 到窗口上限。
            let budget_snap = self
                .budget_handle
                .as_ref()
                .and_then(|b| b.read().ok());
            let (threshold, window_cap) = if let Some(snap) = budget_snap.filter(|s| s.soft_gate > 0)
            {
                (snap.soft_gate as u64, snap.context_window as u64)
            } else {
                let cw = self.state.config.context.fallback_context_window as u64;
                ((cw * 9 / 10), cw)
            };
            // 子 agent 花费（独立字段上报，不并入 total —— total 是「上下文已用/阈值」的
            // 进度条语义，掺入子 agent 花费会把进度条顶满）。
            let child_tokens = self
                .child_usage
                .as_ref()
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);
            // cap: borrow 期间 usage 可超窗口总量，前端不应显示超限数值。
            let usage_total = usage_total.min(window_cap);
            tracing::debug!(
                "[emit_usage] 推送 CONTEXT_USAGE: evt_total={:?} cumulative={} budget_eff={} \
                 => total={} prompt={} completion={} child={} threshold={}",
                evt_total,
                cumulative_val,
                budget_eff,
                usage_total,
                usage_prompt,
                usage_completion,
                child_tokens,
                threshold
            );
            let remaining_pct = calc_context_remaining_percent(usage_total, window_cap);
            self.send(SseEventMsg::ContextUsage {
                prompt_tokens: usage_prompt,
                completion_tokens: usage_completion,
                total_tokens: usage_total,
                child_tokens,
                threshold,
                window_size: window_cap,
                context_remaining_percent: remaining_pct,
                session_total_tokens: cumulative_val,
            })
            .await;
            // 记录本轮最后一次推送的快照（占用口径，压缩后回落），供 run 结束落库
            // （重进会话恢复「已用/阈值」显示，与推送同语义）。
            self.last_usage_snapshot = Some((usage_total, threshold, window_cap));
        } else {
            tracing::warn!(
                "[emit_usage] usage_total=0 未推送（provider 未回有效 usage 且 budget 估算为 0）: \
                 evt_total={:?} cumulative={} budget_eff={}",
                evt_total,
                cumulative_val,
                budget_eff
            );
        }
    }

    /// run 收尾兜底：整个 run 无真实 usage 帧（mimo 等不回 usage 的 provider）时，
    /// 用 budget 的 effective_tokens 补推一次 CONTEXT_USAGE 并填落库快照。
    /// 对齐 codex 的收尾发送位（turn 结束 send_token_count_event）——codex 的
    /// provider 在响应 Completed 必带 usage，无此分支；cortex 的 mimo 兜底是
    /// 本项目已知取舍（见 emit_usage 的 budget 分支注释）。
    async fn emit_final_budget_usage(&mut self) {
        let Some(budget) = self.budget_handle else {
            return;
        };
        // 先取值再 drop 守卫(RwLockReadGuard 非 Send,不能跨 await)
        let (effective, soft_gate, window_cap) = {
            let Ok(snap) = budget.read() else { return };
            (snap.effective_tokens, snap.soft_gate, snap.context_window)
        };
        // effective_tokens 无符号:0 = 尚无任何用量,跳过(无负值分支)
        if effective == 0 {
            return;
        }
        let (threshold, window_cap) = if soft_gate > 0 {
            (soft_gate as u64, window_cap as u64)
        } else {
            let cw = self.state.config.context.fallback_context_window as u64;
            ((cw * 9 / 10), cw)
        };
        // cap: borrow 期间 usage 可超窗口总量，前端不应显示超限数值。
        let usage_total = (effective as u64).min(window_cap);
        let child_tokens = self
            .child_usage
            .as_ref()
            .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        tracing::debug!(
            "[emit_usage] run 收尾 budget 兜底推送: total={usage_total} child={child_tokens} threshold={threshold}"
        );
        let remaining_pct = calc_context_remaining_percent(usage_total, window_cap);
        self.send(SseEventMsg::ContextUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: usage_total,
            child_tokens,
            threshold,
            window_size: window_cap,
            context_remaining_percent: remaining_pct,
            session_total_tokens: 0,
        })
        .await;
        self.last_usage_snapshot = Some((usage_total, threshold, window_cap));
    }

    /// L3 压缩检查点（actions.compaction，content=None）：flush 打开的消息块 +
    /// 推 `CONTEXT_COMPACTED` 通知前端「上下文已自动整理」。调用方随后 `continue`。
    async fn on_compaction(&mut self) {
        self.close_text().await;
        self.close_thinking().await;
        // 压缩重写历史 → 会话级 token 累计清零（对齐 codex：压缩时随历史重写 token_info）。
        {
            let mut map = self.state.session_token_usage.lock().await;
            map.insert(self.thread_id.to_string(), 0);
        }
        // 落库快照同步作废：此刻它还持有压缩前的高位 usage，若压缩是本 run 最后一个
        // 动作，收尾 set_token_usage 会把峰值持久化、重进会话时前端 floor 恢复到峰值。
        // 置 None → 收尾 emit_final_budget_usage 兜底用压缩后的 budget 值补推并重填。
        self.last_usage_snapshot = None;
        // 压缩次数读预算快照：agent 在 yield 压缩事件**之前**已把 advance 后的
        // compaction_count 写进快照（会话级跨 run 累计，非 per-run 计数）。
        // 读不到时回退 1（快照缺失只影响「第 N 次」提示文案，不影响功能）。
        let compaction_count = self
            .budget_handle
            .as_ref()
            .and_then(|b| b.read().ok())
            .map(|snap| snap.compaction_count)
            .filter(|c| *c > 0)
            .unwrap_or(1);
        self.send(SseEventMsg::ContextCompacted { compaction_count })
            .await;
    }

    /// 工具确认：发 `ToolConfirmation` + `RunFinished(tool_confirmation)`。
    /// 调用方随后负责移除 cancel_token 并 `return` 结束 spawn 任务。
    async fn on_confirmation(
        &self,
        tool_name: &str,
        function_call_id: &Option<String>,
        args: &Value,
    ) {
        self.send(SseEventMsg::ToolConfirmation {
            tool_name: tool_display_name(tool_name),
            function_call_id: function_call_id.clone().unwrap_or_default(),
            args: args.clone(),
        })
        .await;
        self.send(SseEventMsg::RunFinished {
            thread_id: self.thread_id.to_string(),
            run_id: self.run_id.to_string(),
            reason: "tool_confirmation".to_string(),
        })
        .await;
    }

    /// 处理一个 Text part：切关思考块、累积正文、剥 ARTIFACT 标记、推送文本分片。
    async fn on_text(&mut self, text: &str, author: &str) {
        if self.thinking_open {
            self.close_thinking().await;
        }
        if author != "user" {
            self.assistant_text.push_str(text);
            self.agent_author = author.to_string();
        }
        // 剥 [[ARTIFACT:...]] 标记：模型若把产物标记抄进正文，
        // 这里兜底剥掉再推前端，避免界面出现这串内部信号误导用户。
        let cleaned = strip_artifact_markers(text);
        if !self.text_open {
            self.current_msg_id = uuid::Uuid::now_v7().to_string();
            self.send(SseEventMsg::TextMessageStart {
                message_id: self.current_msg_id.clone(),
            })
            .await;
            self.text_open = true;
        }
        self.send(SseEventMsg::TextMessageContent {
            message_id: self.current_msg_id.clone(),
            delta: cleaned,
        })
        .await;
    }

    /// 处理一个 Thinking part：推送思考流分片（仅非 user、非空）。
    async fn on_thinking(&mut self, thinking: &str, author: &str) {
        if author != "user" && !thinking.is_empty() {
            // thinking 流直接推送，不再累积做重复检测：
            // 退化治理已回归 cortex_agent 主循环的协议驱动 + thinking budget + max_iterations。
            if !self.thinking_open {
                self.current_thinking_id = uuid::Uuid::now_v7().to_string();
                self.send(SseEventMsg::ThinkingMessageStart {
                    message_id: self.current_thinking_id.clone(),
                })
                .await;
                self.thinking_open = true;
            }
            self.send(SseEventMsg::ThinkingMessageContent {
                message_id: self.current_thinking_id.clone(),
                delta: thinking.to_string(),
            })
            .await;
        }
    }

    /// 处理一个 FunctionCall part。返回 `true` 表示是被抑制的纯下载标记命令
    /// （调用方应 `continue` 跳过），`false` 表示已正常推送。
    async fn on_function_call(&mut self, name: &str, args: &Value, id: &Option<String>) -> bool {
        let call_id = id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        // 纯下载标记命令抑制：若 shell_command 的命令剥掉 [[ARTIFACT:...]] 及其 echo 后为空
        // （即整条只是 echo 标记），整条工具事件不发前端——它是脚本产物→文件卡片的内部信号，
        // 对用户无意义。标记的 call_id 记入 skip 集合，对应 FunctionResponse 也跳过。
        if name == "shell_command" && is_pure_artifact_command(args) {
            self.suppressed_call_ids
                .write()
                .expect("suppressed set lock")
                .insert(call_id.clone());
            tracing::info!("[SSE] 抑制纯下载标记命令（不推前端）: call_id={}", call_id);
            return true;
        }
        self.close_thinking().await;
        self.close_text().await;
        self.send(SseEventMsg::ToolCallStart {
            tool_call_id: call_id.clone(),
            tool_call_name: tool_display_name(name),
            server_name: mcp_server_name(name, self.mcp_slug_to_name),
        })
        .await;
        let args_str = match args {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        if !args_str.is_empty() {
            self.send(SseEventMsg::ToolCallArgs {
                tool_call_id: call_id.clone(),
                delta: args_str,
            })
            .await;
        }
        self.send(SseEventMsg::ToolCallEnd {
            tool_call_id: call_id.clone(),
        })
        .await;
        // 记为在途（等 RESULT 清除）；流结束时若仍残留 → 补发终态 RESULT（防卡 running）
        self.in_flight_tool_calls.insert(call_id, name.to_string());
        false
    }

    /// 处理一个 FunctionResponse part。返回 `true` 表示是被抑制命令的配对响应
    /// （调用方应 `continue`），`false` 表示已正常推送。screenshot 工具走对象存储兜底。
    async fn on_function_response(
        &mut self,
        name: &str,
        response: &Value,
        call_id: String,
    ) -> bool {
        // 收到 RESULT → 移出在途集合（被抑制命令从未入集，remove 是 no-op）
        self.in_flight_tool_calls.remove(&call_id);
        // 配对跳过被抑制的纯下载标记命令的响应
        if self
            .suppressed_call_ids
            .write()
            .expect("suppressed set lock")
            .remove(&call_id)
        {
            return true;
        }

        let result_str = if name.contains("screenshot") {
            tracing::info!(
                "[screenshot] SSE 兜底命中 screenshot 工具, object_store={}, resp_keys={:?}",
                self.state.object_store.is_some(),
                response
                    .as_object()
                    .map(|m| m.keys().collect::<Vec<_>>())
                    .unwrap_or_default()
            );
            // 工具层（TruncatingTool）或 ScreenshotToolWrapper 已注入 image_url 时直接透传：
            // 兼容顶层 image_url 与 output.image_url
            let already_has_image_url = response
                .get("image_url")
                .or_else(|| response.get("output").and_then(|o| o.get("image_url")))
                .is_some();
            if already_has_image_url {
                serde_json::to_string(response).unwrap_or_else(|_| response.to_string())
            } else {
                // 兜底：从 saved_path 或 base64 上传到对象存储
                let saved = match &self.state.object_store {
                    Some(os) => {
                        screenshot::save_screenshot_if_needed(
                            os,
                            self.thread_id,
                            response,
                            self.run_id,
                            &call_id,
                        )
                        .await
                    }
                    None => None,
                };
                match saved {
                    Some(filename) => {
                        let mut enriched = response.clone();
                        let saved_path = format!("screenshots/{}/{filename}", self.thread_id);
                        if let Some(obj) = enriched.as_object_mut() {
                            obj.insert(
                                "image_url".to_string(),
                                serde_json::Value::String(format!(
                                    "/api/screenshots/{}/{filename}",
                                    self.thread_id
                                )),
                            );
                            obj.insert(
                                "saved_path".to_string(),
                                serde_json::Value::String(saved_path),
                            );
                        } else {
                            enriched = serde_json::json!({
                                "data": response,
                                "image_url": format!("/api/screenshots/{}/{filename}", self.thread_id),
                                "saved_path": saved_path
                            });
                        }
                        serde_json::to_string(&enriched).unwrap_or_else(|_| response.to_string())
                    }
                    None => {
                        serde_json::to_string(response).unwrap_or_else(|_| response.to_string())
                    }
                }
            }
        } else {
            serde_json::to_string(response).unwrap_or_else(|_| response.to_string())
        };

        self.send(SseEventMsg::ToolCallResult {
            tool_call_id: call_id,
            tool_name: tool_display_name(name),
            content: result_str,
        })
        .await;
        false
    }
}

/// 创建 SSE 事件流 — 在独立 tokio 任务中运行 Agent 并将事件转换为 SSE 格式。
///
/// 核心逻辑：
/// 1. 发送 `RUN_STARTED` 事件
/// 2. 配置 Runner（session/artifact/memory 服务、compaction、cancellation）
/// 3. 消费 Runner 事件流，将 LLM 响应和工具调用转换为 SSE 事件（由 [`EventSink`] 分派）
/// 4. 处理工具确认（tool_confirmation）— 发送确认请求后暂停等待用户响应
/// 5. 流结束后手动持久化 AI 回复到 PostgreSQL（补偿 adk-rust 的持久化行为）
/// 6. 发送 `RUN_FINISHED` 事件
#[allow(clippy::too_many_arguments)]
pub(super) fn create_event_stream(
    state: Arc<AppState>,
    agent: Arc<dyn Agent>,
    budget_handle: Option<SharedBudget>,
    child_usage: Option<crate::agent::cortex::ChildUsageTotal>,
    thread_id: String,
    run_id: String,
    user_id: String,
    messages: Vec<InputMessage>,
    user_text: String,
    tool_decisions: Option<HashMap<String, String>>,
    model_id: Option<String>,
    tx: Sender<SseEvent>,
    rx: tokio::sync::mpsc::Receiver<SseEvent>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    // 会话级 tracing span：本任务内所有日志自动携带 session_id 字段，
    // 多会话并发时可按 session_id 过滤归属。
    let session_span = tracing::info_span!("session", session_id = %thread_id);
    tokio::spawn((async move {
        // run 生命周期守卫：本任务 panic 等异常退出时所有注销点都会被跳过，
        // 守卫在 unwind 析构中兜底「cancel + 清队列 + 注销」（幂等，正常收尾 no-op）
        let _run_guard = crate::infra::run_registry::ActiveRunGuard::new(
            state.run_registry.clone(),
            &thread_id,
            &run_id,
        );
        let start_ev = SseEventMsg::RunStarted {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        };
        let _ = tx
            .send(SseEvent::default().data(start_ev.to_sse_data()))
            .await;

        // 预取 MCP slug→server.name 映射，供工具卡展示来源
        // （一次 run 复用，避免每个工具事件都查库；按会话归属人隔离）
        let mcp_slug_to_name: HashMap<String, String> = match state.mcp_manager.as_ref() {
            Some(mgr) => mgr
                .list_servers(&user_id, false)
                .await
                .map(|servers| servers.into_iter().map(|s| (s.slug, s.name)).collect())
                .unwrap_or_default(),
            None => Default::default(),
        };

        let session_service = state.adk_session_service.clone();

        // cancel_token 由 handle_run_sse 提前创建并注册到全局表（cancel 接口按 thread_id 找到），
        // 这里直接复用，注入 runner_config（runner 事件边界 is_cancelled 轮询 + agent 工具 select! 双保险）。

        let mut confirmation_decisions = HashMap::new();
        if let Some(decisions) = tool_decisions {
            for (tool_name, decision) in decisions {
                let decision = match decision.to_lowercase().as_str() {
                    "approve" | "yes" | "true" => adk_rust::ToolConfirmationDecision::Approve,
                    _ => adk_rust::ToolConfirmationDecision::Deny,
                };
                confirmation_decisions.insert(tool_name, decision);
            }
        }
        let run_config = Some(adk_rust::RunConfig {
            streaming_mode: adk_rust::StreamingMode::SSE,
            tool_confirmation_decisions: confirmation_decisions,
            ..Default::default()
        });

        let runner_config = adk_rust::runner::RunnerConfig {
            app_name: "cortex-agent".to_string(),
            agent: agent.clone(),
            session_service: session_service.clone(),
            artifact_service: state.artifact_service.clone(),
            memory_service: state.memory_service.clone(),
            plugin_manager: None,
            run_config,
            // 上下文压缩统一由 CortexAgent 的窗口阈值压缩接管（对齐 codex：仅按窗口 90% 触发），
            // 不再使用 adk-rust 的按轮数（L1）/单轮 token（L2）压缩。
            compaction_config: None,
            context_cache_config: None,
            cache_capable: None,
            request_context: None,
            cancellation_token: Some(cancel_token.clone()),
            intra_compaction_config: None,
            intra_compaction_summarizer: None,
        };
        let runner = match adk_rust::runner::Runner::new(runner_config) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("创建 Runner 失败: {}", e);
                send_run_error(&tx, &thread_id, &run_id, e.to_string()).await;
                crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id).await;
                return;
            }
        };

        // 用户输入使用外层构造好的 user_text（已过 mention XML 注入）。
        // 历史消息由 adk-rust 的 Session 服务负责持久化与回放，不应在此重复拼接——
        // 早期实现曾把 messages 全量 join 塞给 LLM（含历史 assistant 回复），
        // 会让模型误以为对话已完结而空转，事件流零输出（本次 bug 根因）。
        let content = build_user_content(&user_text, &messages, &state, &thread_id).await;
        tracing::info!(
            "[RunSSE] 提交给 Runner 的 content: {} 字符（已注入 mention）",
            user_text.len()
        );

        let session_id = adk_rust::SessionId::try_from(thread_id.clone())
            .unwrap_or_else(|_| adk_rust::SessionId::generate());
        let mut event_stream = match runner
            .run(
                adk_rust::UserId::new_unchecked(&user_id),
                session_id,
                content,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Runner.run() 失败: {}", e);
                send_run_error(&tx, &thread_id, &run_id, e.to_string()).await;
                crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id).await;
                return;
            }
        };

        let mut sink = EventSink {
            tx: &tx,
            thread_id: &thread_id,
            run_id: &run_id,
            mcp_slug_to_name: &mcp_slug_to_name,
            state: &state,
            budget_handle: &budget_handle,
            child_usage: &child_usage,
            current_msg_id: String::new(),
            text_open: false,
            current_thinking_id: String::new(),
            thinking_open: false,
            assistant_text: String::new(),
            agent_author: String::new(),
            last_usage_snapshot: None,
            suppressed_call_ids: Arc::new(RwLock::new(HashSet::new())),
            in_flight_tool_calls: HashMap::new(),
        };

        while let Some(event_result) = event_stream.next().await {
            match event_result {
                Ok(event) => {
                    // 逐事件调试日志(降 debug:流式一回合数百条,info 会淹没
                    // 节点级日志;需要逐帧排查时开 debug)
                    tracing::debug!(
                        "[SSE] event author={} turn_complete={} interrupted={}",
                        event.author,
                        event.llm_response.turn_complete,
                        event.llm_response.interrupted,
                    );

                    if let Some(ref confirmation) = event.actions.tool_confirmation {
                        // 工具审批确认:节点级,保留 info
                        tracing::info!(
                            "[SSE] tool_confirmation: name={} call_id={:?} args={}",
                            confirmation.tool_name,
                            confirmation.function_call_id,
                            serde_json::to_string(&confirmation.args).unwrap_or_default(),
                        );
                        sink.on_confirmation(
                            &confirmation.tool_name,
                            &confirmation.function_call_id,
                            &confirmation.args,
                        )
                        .await;
                        crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id).await;
                        return;
                    }

                    sink.emit_usage(&event).await;

                    // 压缩检查点事件（L3 yield 的 actions.compaction，content=None）：
                    // flush 打开的消息块 + 推 CONTEXT_COMPACTED 通知前端「上下文已自动整理」。
                    // content=None 故不会进下面的正文分支，这里显式拦截发通知。
                    if event.actions.compaction.is_some() {
                        sink.on_compaction().await;
                        continue;
                    }

                    // mailbox 注入事件（多智能体 V2：子 agent FINAL_ANSWER/MESSAGE 信封，
                    // author="user" 的 user-role 内容）：仅作持久化（runner 落库），
                    // 不推前端气泡——inter-agent 通信对用户不可见（对齐 codex 走
                    // analysis channel）。runner 的真实用户输入事件不进本流，故按
                    // author=user 判定是精确的。
                    if event.author == "user" {
                        continue;
                    }

                    if let Some(content) = &event.llm_response.content {
                        for part in &content.parts {
                            match part {
                                adk_rust::Part::Text { text } => {
                                    // 逐 chunk 级(降 debug):流式期间每帧都有,
                                    // info 刷屏无信息量;STREAM TEXT CHUNK 同级保留内容采样
                                    tracing::debug!("[SSE] text part: {} chars", text.len());
                                    sink.on_text(text, &event.author).await;
                                }
                                adk_rust::Part::FunctionCall { name, args, id, .. } => {
                                    tracing::info!("[SSE] FunctionCall: name={} id={:?}", name, id);
                                    if sink.on_function_call(name, args, id).await {
                                        continue;
                                    }
                                }
                                adk_rust::Part::FunctionResponse {
                                    function_response,
                                    id,
                                    ..
                                } => {
                                    let call_id = id.clone().unwrap_or_default();
                                    tracing::info!(
                                        "[SSE] FunctionResponse: name={} call_id={} resp_len={}",
                                        function_response.name,
                                        call_id,
                                        serde_json::to_string(&function_response.response)
                                            .map(|s| s.len())
                                            .unwrap_or(0),
                                    );
                                    if sink
                                        .on_function_response(
                                            &function_response.name,
                                            &function_response.response,
                                            call_id,
                                        )
                                        .await
                                    {
                                        continue;
                                    }
                                }
                                adk_rust::Part::Thinking {
                                    thinking,
                                    signature,
                                } => {
                                    // 逐 chunk 级:降 debug(同 text part)
                                    tracing::debug!(
                                        "[SSE] thinking part: {} chars signature={}",
                                        thinking.chars().count(),
                                        signature.as_ref().map(|s| s.len()).unwrap_or(0)
                                    );
                                    sink.on_thinking(thinking, &event.author).await;
                                }
                                adk_rust::Part::EmbeddedResource { .. } => {
                                    tracing::debug!("[SSE] EmbeddedResource part skipped");
                                }
                                adk_rust::Part::InlineData {
                                    mime_type, data, ..
                                } => {
                                    // 罕见(模型回传内联数据),保留 info:排查"图片是否到模型"的关键观测
                                    tracing::info!(
                                        "[SSE] InlineData part: mime_type={} bytes={}",
                                        mime_type,
                                        data.len()
                                    );
                                }
                                adk_rust::Part::FileData {
                                    mime_type,
                                    file_uri,
                                    ..
                                } => {
                                    // 罕见,同上保留 info
                                    tracing::info!(
                                        "[SSE] FileData part: mime_type={} uri={}",
                                        mime_type,
                                        file_uri
                                    );
                                }
                                adk_rust::Part::ServerToolCall { server_tool_call } => {
                                    // 罕见(服务端工具),保留 info
                                    tracing::info!(
                                        "[SSE] ServerToolCall part: {}",
                                        serde_json::to_string(server_tool_call).unwrap_or_default()
                                    );
                                }
                                adk_rust::Part::ServerToolResponse {
                                    server_tool_response,
                                } => {
                                    tracing::info!(
                                        "[SSE] ServerToolResponse part: {}",
                                        serde_json::to_string(server_tool_response)
                                            .unwrap_or_default()
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("[SSE] 事件流错误: {}", e);
                    send_run_error(&tx, &thread_id, &run_id, e.to_string()).await;
                    crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id).await;
                    return;
                }
            }
        }

        sink.close_thinking().await;

        // 在途工具补终态：流结束时（用户取消 / runner 异常中断）若有工具已发 START 但未回流 RESULT，
        // 前端会永久停在「沙箱执行中」。这里 drain 残留 call_id，逐条补发一条 cancelled RESULT 翻成终态。
        // （正常完成时该 map 应为空；非空的正常完成属异常，同样补发以解卡。）
        let cancelled = cancel_token.is_cancelled();
        let leftover = std::mem::take(&mut sink.in_flight_tool_calls);
        if !leftover.is_empty() {
            tracing::info!(
                "[SSE] 流结束时仍有 {} 个在途工具未返回结果，补发终态 RESULT（cancelled={}）",
                leftover.len(),
                cancelled
            );
            for (call_id, tool_name) in leftover {
                let content = if cancelled {
                    r#"{"ok":false,"cancelled":true,"error":"运行已被中止"}"#.to_string()
                } else {
                    r#"{"ok":false,"error":"运行已结束（工具未返回结果）"}"#.to_string()
                };
                sink.send(SseEventMsg::ToolCallResult {
                    tool_call_id: call_id,
                    tool_name,
                    content,
                })
                .await;
            }
        }

        // 空响应告警：如果 agent 跑完了但一个 token 都没吐出来，
        // 极可能是端点协议错配（Anthropic vs OpenAI）、API Key 失效、
        // 或模型服务返回了非流式空响应。此前这种情况前端会看到"消息发出去没反应"，
        // 极难排查——这里显式打日志 + 发一条错误事件到前端。
        if sink.assistant_text.trim().is_empty() && !cancel_token.is_cancelled() {
            tracing::warn!(
                "[SSE] agent 完成但未输出任何内容（可能是端点/密钥配置错误、模型服务空响应或被网关拦截）\
                 model_id={:?} thread_id={}",
                model_id,
                thread_id
            );
            sink.send(SseEventMsg::RunError {
                message: "模型未返回任何内容。可能原因：\
                         1) base_url 端点协议错配（如 GLM 的 Anthropic 端点需改为 OpenAI 兼容端点 `/api/paas/v4`）；\
                         2) API Key 失效或欠费；\
                         3) 网关拦截。\
                         请检查后端日志与「模型供应商管理」配置。"
                    .to_string(),
            })
            .await;
        }

        tracing::info!("[SSE] 事件流结束（agent 运行完成）");

        // 用户取消时不持久化半截回复（避免 session 历史里出现一条 turn_complete 的残缺消息）
        // 落库前剥 [[ARTIFACT:...]] 标记：跨片标记此时已完整拼好，整体剥一次最干净，
        // 保证历史会话恢复也不再出现这串内部信号。
        let assistant_text = strip_artifact_markers(&sink.assistant_text);
        if !assistant_text.is_empty() && !cancel_token.is_cancelled() {
            let parts: Vec<adk_rust::Part> = vec![adk_rust::Part::Text {
                text: assistant_text.clone(),
            }];

            let mut event = adk_rust::Event::new(&run_id);
            event.author = if sink.agent_author.is_empty() {
                "agent".to_string()
            } else {
                sink.agent_author.clone()
            };
            let mut content = adk_rust::Content::new("model");
            content.parts = parts;
            event.llm_response.content = Some(content);
            event.llm_response.turn_complete = true;
            event.llm_response.partial = false;

            tracing::info!(
                "[SSE] 手动持久化 AI 回复到 PG: text={} chars",
                assistant_text.len()
            );
            if let Err(e) = state
                .adk_session_service
                .append_event(&thread_id, event)
                .await
            {
                tracing::warn!("[SSE] 手动持久化失败: {}", e);
            }
        }

        // run 收尾兜底推送(对齐 codex 'turn 收尾 send_token_count_event 一次'):
        // 流式中间帧一律不推后,不回 usage 的 provider(mimo)整个 run 无真实 usage
        // 帧——此处在 RunFinished 前用 budget 估算补发一次,前端 footer 有数、
        // 落库快照不空。真实 usage 的 provider 在 usage 帧已推、快照非 None,跳过。
        // 用户取消不推(与落库口径一致)。
        if !cancel_token.is_cancelled() && sink.last_usage_snapshot.is_none() {
            sink.emit_final_budget_usage().await;
        }

        // 落库会话级 token 用量快照（对齐 codex 会话级 token_info 持久化）：
        // 重进会话时前端据此立即恢复「已用 / 阈值」显示（见 chat.js restoreContextUsage）。
        // 用户取消时不落库（与不持久化半截回复一致）。
        if !cancel_token.is_cancelled()
            && let Some((total, threshold, _window_size)) = sink.last_usage_snapshot
        {
            if let Some(store) = &state.session_settings_store {
                if let Err(e) = store.set_token_usage(&thread_id, total, threshold).await {
                    tracing::warn!("[SSE] token 用量落库失败: {}", e);
                }
            }
        }

        // 注销活跃 run（run_id 匹配才注）——**唯一的常规注销点**：agent 侧 steer
        // `finish` 判定队列空时只标记 draining 不注销（assistant 正文此刻尚未落库），
        // 此处持久化完成后注销，保证后继 run 读历史时能看到本轮完整收尾；早退路径
        //（构建/确认/流错误）各自就地注销，panic 由 _run_guard 兜底。
        crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id)
            .await;

        sink.close_text().await;
        sink.send(SseEventMsg::RunFinished {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            reason: "complete".to_string(),
        })
        .await;

        // 沙箱快照上传(会话亲和容灾):本轮结束后异步上传,供节点故障切换时恢复
        if let Some(os) = state.object_store.clone() {
            let sandbox_dir = state.config.workspace_session_dir(&thread_id);
            if sandbox_dir.exists() {
                let sid = thread_id.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        crate::infra::sandbox::workspace_snapshot::upload(&os, &sid, &sandbox_dir).await
                    {
                        tracing::warn!("[sse] 上传沙箱快照失败(可忽略): {e}");
                    }
                });
            }
        }
    }).instrument(session_span));

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    stream.map(Ok)
}
