//! ChildAgentFactory —— V2 工具集背后的 spawn / 消息 / 中断 / 列表逻辑。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};

use adk_rust::serde_json::{json, Value};
use adk_rust::tokio::sync::watch;
use adk_rust::{Content, InvocationContext, Llm, Tool};
use tokio_util::sync::CancellationToken;

use super::super::builder::ModelResolver;
use super::super::role::{self, AgentRole, DEFAULT_ROLE_NAME};
use super::blueprint::{AgentBlueprint, BuildChildParams};
use super::child_loop::run_child_loop;
use super::envelope::{InterAgentMessageType, render_inter_agent_message};
use super::fork::{fork_history, ForkMode};
use super::mailbox::ParentMailbox;
use super::session::ChildSession;
use super::status::{ChildStatus, validate_spawn_depth};
use super::tree::{AgentTree, ChildHandle, MailboxItem};
use super::{ChildAgentEvent, ChildEventSink};
use crate::config::AgentsConfig;

/// spawn 请求参数（V2 schema 全集）。
pub(crate) struct SpawnRequest {
    pub task_name: String,
    pub message: String,
    pub agent_type: Option<String>,
    pub model_override: Option<Arc<dyn Llm>>,
    pub fork_mode: ForkMode,
    /// 父 conv 当前增量（preamble 之后），fork 用
    pub current_conv_tail: Vec<Content>,
}

pub(crate) struct ChildAgentFactory {
    /// 蓝图（wait_agent 读 tool_timeout 做外层钳制）
    pub(super) blueprint: AgentBlueprint,
    pub(super) tree: Arc<AgentTree>,
    parent_ctx: Arc<dyn InvocationContext>,
    cancel_token: CancellationToken,
    sink: Arc<dyn ChildEventSink>,
    depth: u32,
    max_depth: u32,
    /// model id → Llm 解析器（spawn 的 model 参数用；对齐 codex
    /// apply_requested_spawn_agent_model_overrides——args 优先、
    /// default_subagent_model 兜底、再回落继承父模型）。
    /// None=不支持覆盖（聊天模式等无 store 场景）。
    model_resolver: Option<ModelResolver>,
    /// 本 agent 的 canonical path（root 的 spawn 上下文 = "/root"）
    self_path: String,
    /// spawn_agent 工具渲染 agent_type 候选描述用
    pub(super) agents_cfg: AgentsConfig,
    /// 父会话历史（fork FullHistory/LastNTurns 用）
    /// 父会话历史（factory 构建时读一次——与主循环 conv 基线 history_len 同一时刻快照，
    /// 保证 fork 拼接「持久历史 + 本 run 增量」不重叠。不能懒加载：首次 spawn 时读到的
    /// history 已含本 run 已持久化事件，会与 conv 快照双份）。
    parent_history: Vec<Content>,
    /// 主循环 conv 增量快照（spawn fork 输入；每轮由主循环刷新）
    pub(super) conv_snapshot: Arc<StdMutex<Vec<Content>>>,
    /// 本 agent 收到子 agent FINAL_ANSWER / MESSAGE 的邮箱。root 的 mailbox 由主循环
    /// drain 注入 conv；子 agent 的 mailbox 由其会话循环 drain（见 run_child_loop）。
    pub(super) self_mailbox: Option<Arc<ParentMailbox>>,
}

impl ChildAgentFactory {
    #[allow(clippy::too_many_arguments)] // 子 agent 工厂构建参数，聚合 struct 反而割裂
    pub(crate) fn new(
        blueprint: AgentBlueprint,
        tree: Arc<AgentTree>,
        parent_ctx: Arc<dyn InvocationContext>,
        cancel_token: CancellationToken,
        sink: Arc<dyn ChildEventSink>,
        depth: u32,
        max_depth: u32,
        self_path: String,
        agents_cfg: AgentsConfig,
        conv_snapshot: Arc<StdMutex<Vec<Content>>>,
        self_mailbox: Option<Arc<ParentMailbox>>,
        model_resolver: Option<ModelResolver>,
    ) -> Self {
        let parent_history = parent_ctx.session().conversation_history();
        Self {
            blueprint,
            tree,
            parent_ctx,
            cancel_token,
            sink,
            depth,
            max_depth,
            self_path,
            agents_cfg,
            parent_history,
            conv_snapshot,
            self_mailbox,
            model_resolver,
        }
    }

    /// 解析 spawn 的模型覆盖（对齐 codex 优先级：args.model >
    /// agents.default_subagent_model > 继承父模型）。
    /// - args 显式指定的 model 解析失败 → 报错（模型传错了要能纠正）；
    /// - default_subagent_model 是服务端兜底配置，解析失败（id 被删/改名）→
    ///   告警并回落继承父模型，不让一个坏配置哑掉整个多智能体功能。
    pub(super) fn resolve_model_override(
        &self,
        requested: Option<&str>,
    ) -> std::result::Result<Option<Arc<dyn Llm>>, String> {
        // args 显式指定
        if let Some(id) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            let resolved = self.model_resolver.as_ref().and_then(|f| f(id));
            return match resolved {
                Some(m) => Ok(Some(m)),
                None => Err(format!("Unknown model `{id}` for spawn_agent.")),
            };
        }
        // 服务端默认兜底：失败只告警回落（codex 的 default_subagent_model 同为兜底非硬依赖）
        if let Some(id) = self
            .agents_cfg
            .default_subagent_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let resolved = self.model_resolver.as_ref().and_then(|f| f(id));
            return match resolved {
                Some(m) => Ok(Some(m)),
                None => {
                    tracing::warn!(
                        "[multi_agent] agents.default_subagent_model=`{id}` 解析失败，回落继承父模型"
                    );
                    Ok(None)
                }
            };
        }
        Ok(None)
    }

    /// 生成 V2 全套工具（spawn/send_message/followup_task/wait/interrupt/list）。
    pub(crate) fn toolset(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        vec![
            self.spawn_handle(),
            Arc::new(super::tools::SendMessageTool::new(Arc::clone(self), false)) as Arc<dyn Tool>,
            Arc::new(super::tools::SendMessageTool::new(Arc::clone(self), true)) as Arc<dyn Tool>,
            self.wait_handle(),
            Arc::new(super::tools::InterruptAgentTool::new(Arc::clone(self))) as Arc<dyn Tool>,
            Arc::new(super::tools::ListAgentsTool::new(Arc::clone(self))) as Arc<dyn Tool>,
        ]
    }

    fn spawn_handle(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(super::tools::SpawnAgentTool::new(Arc::clone(self)))
    }
    fn wait_handle(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(super::tools::WaitAgentTool::new(Arc::clone(self)))
    }

    /// canonical path 计算：`{self_path}/{task_name}`。
    fn canonical(&self, task_name: &str) -> String {
        format!("{}/{}", self.self_path.trim_end_matches('/'), task_name)
    }

    /// 解析 target（相对名 / canonical 绝对路径），返回 canonical path。
    /// 对齐 codex resolve_agent_target：`/` 开头 = 绝对；否则相对 caller 路径拼接。
    pub(crate) fn resolve_target(&self, target: &str) -> String {
        let t = target.trim();
        if t.starts_with('/') {
            t.to_string()
        } else {
            self.canonical(t)
        }
    }

    /// 解析角色（未知角色报错，消息对齐 codex）。
    fn resolve_role(&self, agent_type: Option<&str>) -> std::result::Result<AgentRole, String> {
        let name = agent_type
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_ROLE_NAME);
        role::resolve_role(name, &self.agents_cfg.roles)
            .ok_or_else(|| format!("unknown agent_type '{name}'"))
    }

    /// fork 一个子 agent 并启动持久会话循环。
    pub(crate) async fn spawn(
        &self,
        req: SpawnRequest,
    ) -> std::result::Result<(String, Option<String>), String> {
        let child_depth = validate_spawn_depth(self.depth, self.max_depth)?;
        // task_name 语法校验（对齐 codex AgentPath 段规则：[a-z0-9_]，保留字拒绝）
        validate_task_name(&req.task_name)?;
        let canonical = self.canonical(&req.task_name);
        // 并发容量检查在原子注册处（try_register_with_capacity 锁内 check+占位，
        // 对齐 codex registry CAS——裸预检与注册分离有跨 agent 并发 spawn 的 TOCTOU）。
        // 冲突处理：运行中 → 拒绝；终态 → 复位 interrupt、清 inbox、投新任务、唤醒旧循环复用
        // （对齐 codex agent path 重复拒绝；终态复活路径同 followup_task，不新增占用）。
        if let Some(existing) = self.tree.get(&canonical) {
            let current = existing.status.borrow().clone();
            if !current.is_terminal() {
                return Err(format!("agent path `{canonical}` already exists"));
            }
            // 复活容量检查（复活后 Running 重新占位；不检查可借反复复活突破并发上限）
            if let Err(limit) = self.tree.check_revive_capacity() {
                return Err(format!(
                    "agent thread limit reached (limit {limit}). Call wait_agent to collect finished ones before spawning more, or do the task yourself."
                ));
            }
            *existing.interrupt.lock().expect("interrupt lock poisoned") = CancellationToken::new();
            existing.inbox.lock().expect("child inbox poisoned").clear();
            let _ = existing.status.send_replace(ChildStatus::PendingInit);
            let initial_task = render_inter_agent_message(
                InterAgentMessageType::NewTask,
                &canonical,
                &self.self_path,
                &format!(
                    "[You are agent `{canonical}`. Your parent agent assigned you this task.]\n\n{}",
                    req.message
                ),
            );
            existing.push_mailbox(MailboxItem {
                rendered: initial_task,
                trigger_turn: true,
            });
            self.tree.notify_activity();
            // 复活也通知前端（否则面板停在 completed，新一轮活动无起点标记）
            self.sink.emit(ChildAgentEvent::Started {
                task_name: canonical.clone(),
            });
            existing.wake.notify_one();
            let nickname = existing.nickname.clone();
            return Ok((canonical, nickname));
        }

        // 角色 + 模型覆盖解析
        let agent_role = self.resolve_role(req.agent_type.as_deref())?;
        let child_cancel = self.cancel_token.child_token();
        // 容量预检（对齐 codex ensure_execution_capacity 的快检 + registry CAS 硬闸双层：
        // 预检放昵称预留前——容量拒绝时不白占池内昵称；原子注册兜底 TOCTOU）
        if let Err(limit) = self.tree.check_revive_capacity() {
            return Err(format!(
                "agent thread limit reached (limit {limit}). Call wait_agent to collect finished ones before spawning more, or do the task yourself."
            ));
        }
        // 先建 handle（agent 构建需要 self_inbox；树的注册在其后）
        let nickname = self.tree.reserve_nickname(&agent_role);
        let (status_tx, _status_rx) = watch::channel(ChildStatus::PendingInit);
        let child = Arc::new(ChildHandle {
            path: canonical.clone(),
            nickname: nickname.clone(),
            status: status_tx,
            inbox: StdMutex::new(VecDeque::new()),
            wake: tokio::sync::Notify::new(),
            interrupt: StdMutex::new(CancellationToken::new()),
            injected_log: StdMutex::new(Vec::new()),
        });
        let child_agent = self.blueprint.build_child(BuildChildParams {
            task_name: &req.task_name,
            depth: child_depth,
            cancel: child_cancel.clone(),
            model_override: req.model_override.clone(),
            instruction_override: agent_role.instruction.clone(),
            canonical_path: &canonical,
            handle: &child,
        })?;

        // fork 历史进 session initial；任务指令作为 NEW_TASK mailbox 消息投递
        // （对齐 codex SpawnInitialInput::InterAgentCommunication：初始任务走 mailbox，
        // 与 followup_task 同一条消费路径，避免「首任务在 session、后续任务在 inbox」双轨）。
        let forked = fork_history(&self.parent_history, req.fork_mode, &req.current_conv_tail);

        let parent_sess = self.parent_ctx.session();
        let session = ChildSession::new(
            parent_sess.id().to_string(),
            parent_sess.app_name().to_string(),
            parent_sess.user_id().to_string(),
            forked,
        );

        if let Err(limit) = self
            .tree
            .try_register_with_capacity(&canonical, child.clone())
        {
            return Err(format!(
                "agent thread limit reached (limit {limit}). Call wait_agent to collect finished ones before spawning more, or do the task yourself."
            ));
        }

        // 通知前端子 agent 已启动（聚合键 = canonical path，全树唯一防撞名混流）。
        self.sink.emit(ChildAgentEvent::Started {
            task_name: canonical.clone(),
        });

        // 持久会话循环：跑 turn → 收 mailbox → 有 trigger 则再跑。
        let tree = self.tree.clone();
        let sink = self.sink.clone();
        let task_name = req.task_name.clone();
        let parent_path = self.self_path.clone();
        let parent_mailbox = self.self_mailbox.clone();
        let blueprint_cancel = child_cancel.clone();
        let child_for_task = child.clone();
        let canonical_for_log = canonical.clone();
        // 初始任务：NEW_TASK 信封（canonical 身份 + 任务指令），投 inbox 并唤醒循环。
        let initial_task = render_inter_agent_message(
            InterAgentMessageType::NewTask,
            &canonical,
            &self.self_path,
            &format!(
                "[You are agent `{canonical}`. Your parent agent assigned you this task.]\n\n{}",
                req.message
            ),
        );
        child.push_mailbox(MailboxItem {
            rendered: initial_task,
            trigger_turn: true,
        });
        self.tree.notify_activity();
        child.wake.notify_one();

        let handle = tokio::spawn(async move {
            run_child_loop(
                child_agent,
                session,
                child_for_task,
                tree,
                sink,
                &canonical_for_log,
                &parent_path,
                parent_mailbox,
                blueprint_cancel,
            )
            .await;
            tracing::info!(
                "[multi_agent] 子 agent '{task_name}' ({canonical_for_log}) 会话循环退出"
            );
        });
        // 终止路径：主 run 结束 → registry/factory drop → cancel_token 级联取消子循环；
        // 单个 turn 可被 interrupt_agent 取消。显式 drop 分离任务（tokio 语义：drop
        // JoinHandle 即 detached，由运行时管理）——不用 `let _ =`，JoinHandle 是
        // Future，`let _ =` 会触发 let_underscore_future 且语义含混。
        std::mem::drop(handle);
        Ok((canonical, nickname))
    }

    /// send_message / followup_task 共用投递（对齐 codex handle_message_string_tool）。
    pub(crate) fn send_message(
        &self,
        target: &str,
        message: &str,
        trigger_turn: bool,
    ) -> std::result::Result<(), String> {
        let msg = message.trim();
        if msg.is_empty() {
            return Err("Empty message can't be sent to an agent".to_string());
        }
        let canonical = self.resolve_target(target);
        // root 特判先于 tree.get（root 不在 children map）；followup 不能 target root
        // （对齐 codex "Follow-up tasks can't target the root agent"）。
        if canonical == "/root" {
            if trigger_turn {
                return Err("Follow-up tasks can't target the root agent".to_string());
            }
            let rendered = render_inter_agent_message(
                InterAgentMessageType::Message,
                &canonical,
                &self.self_path,
                msg,
            );
            return self.tree.deliver(
                &canonical,
                MailboxItem {
                    rendered,
                    trigger_turn,
                },
            );
        }
        let child = self
            .tree
            .get(&canonical)
            .ok_or_else(|| format!("live agent path `{canonical}` not found"))?;
        let rendered = render_inter_agent_message(
            if trigger_turn {
                InterAgentMessageType::NewTask
            } else {
                InterAgentMessageType::Message
            },
            &canonical,
            &self.self_path,
            msg,
        );
        self.tree.deliver(
            &canonical,
            MailboxItem {
                rendered,
                trigger_turn,
            },
        )?;
        if trigger_turn {
            // followup：终态 agent 复活（对齐 codex trigger_turn 可重启 idle 线程；
            // 复活重新占运行位，过容量检查——对齐 ensure_execution_capacity_for_turn_start）
            if child.status.borrow().is_terminal() {
                if let Err(limit) = self.tree.check_revive_capacity() {
                    return Err(format!(
                        "agent thread limit reached (limit {limit}). Call wait_agent to collect finished ones before giving this agent another task."
                    ));
                }
                *child.interrupt.lock().expect("interrupt lock poisoned") =
                    CancellationToken::new();
                let _ = child.status.send_replace(ChildStatus::PendingInit);
            }
            child.wake.notify_one();
        }
        Ok(())
    }

    /// 中断子 agent 当前 turn（对齐 codex interrupt_agent：agent 仍可接新任务）。
    pub(crate) fn interrupt(&self, target: &str) -> std::result::Result<Value, String> {
        let canonical = self.resolve_target(target);
        // root 特判（对齐 codex "root is not a spawned agent"，先于 not-found 判定）
        if canonical == "/root" {
            return Err("root is not a spawned agent".to_string());
        }
        let child = self
            .tree
            .get(&canonical)
            .ok_or_else(|| format!("agent with id {canonical} not found"))?;
        // 不能中断自己
        if canonical == self.self_path {
            return Err(
                "an agent cannot interrupt itself; return your result and let the parent interrupt you if needed"
                    .to_string(),
            );
        }
        let previous = child.status.borrow().status_value();
        // 对齐 codex「interrupt 只作用于进行中的 turn」：仅 Running 才 cancel token。
        // 非 Running（PendingInit/终态）打 interrupt 是 no-op——若对 PendingInit cancel，
        // 该 agent 的初始任务会被永久搁浅（followup 见非终态不换 token，循环醒后
        // is_cancelled→continue 烧掉 permit 再挂起，砖死并占住并发额度）。
        if !matches!(&*child.status.borrow(), ChildStatus::Running) {
            self.tree.notify_activity();
            return Ok(json!({ "previous_status": previous }));
        }
        child
            .interrupt
            .lock()
            .expect("interrupt lock poisoned")
            .cancel();
        let _ = child.status.send_replace(ChildStatus::Interrupted);
        self.tree.notify_activity();
        Ok(json!({ "previous_status": previous }))
    }

    /// list_agents（对齐 codex：/root 排第一位 + canonical path 字典序 + 状态值）。
    pub(crate) fn list_agents(&self) -> Value {
        let paths = self.tree.list_paths();
        let mut agents: Vec<Value> = Vec::with_capacity(paths.len() + 1);
        // root 排第一（对齐 codex register_session_root + list 的 root-first 语义）：
        // root 存活期间（本 agent 树存在）恒可见，给子 agent「可以向 /root 发消息」
        // 的发现渠道。
        agents.push(json!({
            "agent_name": "/root",
            "agent_status": "running",
        }));
        for p in &paths {
            if let Some(child) = self.tree.get(p) {
                let status = child.status.borrow().status_value();
                agents.push(json!({
                    "agent_name": p,
                    "agent_status": status,
                }));
            }
        }
        json!({ "agents": agents })
    }
}

/// task_name 语法校验（对齐 codex AgentPath 段规则）。
fn validate_task_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("agent_name must not be empty".to_string());
    }
    if name == "root" {
        return Err("agent_name `root` is reserved".to_string());
    }
    if name.contains('/') {
        return Err("agent_name must not contain `/`".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(
            "agent_name must use only lowercase letters, digits, and underscores".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_name_validation() {
        assert!(validate_task_name("explore_auth").is_ok());
        assert!(validate_task_name("t1").is_ok());
        assert!(validate_task_name("").is_err());
        assert!(validate_task_name("root").is_err());
        assert!(validate_task_name("a/b").is_err());
        assert!(validate_task_name("Upper").is_err());
        assert!(validate_task_name("a-b").is_err());
    }
}
