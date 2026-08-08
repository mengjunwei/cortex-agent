//! 多智能体动态并行——`spawn_agent` / `wait_agent`（对齐 codex `multi_agents`）。
//!
//! 模型在运行时自主 fork 子 agent，让「有界、可独立运行」的子任务在后台并行推进，
//! 主 agent 不阻塞、继续干别的活，最后用 `wait_agent` 收齐结果。把「调研A → 调研B →
//! 调研C」式的串行多轮，压成 spawn + 一轮 wait，显著减少交互轮次。
//!
//! 子 agent 与父同构（同 model / 工具 / preamble / context 治理），通过
//! [`ChildInvocationContext`] 包装父 ctx：工具执行复用父 ctx（共享文件系统 / artifacts /
//! 记忆），而对话历史从 task prompt 起步。spawn 深度受 `max_spawn_depth` 限制，防失控递归。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use adk_rust::async_trait;
use adk_rust::serde_json::{json, Value};
use adk_rust::tokio::sync::watch;
use adk_rust::{
    Agent, Artifacts, CallbackContext, Content, EventStream, GenerateContentConfig,
    InvocationContext, Llm, Memory, Part, ReadonlyContext, Result, RunConfig, Session,
    SharedState, State, Tool, ToolContext, Toolset,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::builder::CortexAgentBuilder;
use super::hook::CompactionHook;
use crate::config::ContextConfig;
use crate::domain::permissions::PermissionPolicy;

pub(crate) const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";
pub(crate) const WAIT_AGENT_TOOL_NAME: &str = "wait_agent";
const WAIT_DEFAULT_TIMEOUT_SECS: u64 = 300;

// ============================================================================
// 子 agent 事件出口（① 可视化：子 agent 活动转发到主 SSE 流）
// ============================================================================
//
// cortex 是单 SSE 流架构（一个 run 一个连接），不像 codex 那样 per-thread 订阅。
// 因此子 agent 的事件通过本 trait 转发到主 SSE 流，前端按 task_name 聚合渲染。
// agent 层只定义抽象，server 层实现（转 SseEvent），避免 agent→server 反向依赖。

/// 子 agent 活动事件。
#[derive(Clone, Debug)]
pub enum ChildAgentEvent {
    /// 子 agent 开始运行。
    Started { task_name: String },
    /// 子 agent 的一段文本输出（增量 delta）。
    Text { task_name: String, delta: String },
    /// 子 agent 发起一次工具调用。
    ToolCall {
        task_name: String,
        tool_call_id: String,
        name: String,
        args: String,
    },
    /// 子 agent 的工具调用返回结果。
    ToolResult {
        task_name: String,
        tool_call_id: String,
        name: String,
        content: String,
    },
    /// 子 agent 运行结束（ok=true 成功；result=最终文本或错误说明）。
    Finished {
        task_name: String,
        ok: bool,
        result: String,
    },
}

/// 子 agent 事件出口。server 层实现转 SSE；默认 [`NoopChildEventSink`] 丢弃。
pub trait ChildEventSink: Send + Sync {
    fn emit(&self, event: ChildAgentEvent);
}

/// 空 sink：非 SSE 场景 / 测试用，丢弃所有事件。
pub struct NoopChildEventSink;
impl ChildEventSink for NoopChildEventSink {
    fn emit(&self, _event: ChildAgentEvent) {}
}

// ============================================================================
// 子 agent 状态
// ============================================================================

/// 子 agent 的运行状态。watch channel 的载荷。
#[derive(Clone, Debug)]
pub(crate) enum ChildStatus {
    Running,
    Completed(String),
    Failed(String),
}

impl ChildStatus {
    fn is_done(&self) -> bool {
        !matches!(self, ChildStatus::Running)
    }
    fn label(&self) -> &'static str {
        match self {
            ChildStatus::Running => "running",
            ChildStatus::Completed(_) => "completed",
            ChildStatus::Failed(_) => "failed",
        }
    }
    fn result_text(&self) -> String {
        match self {
            ChildStatus::Running => "[still running]".to_string(),
            ChildStatus::Completed(s) => s.clone(),
            ChildStatus::Failed(s) => format!("[failed: {s}]"),
        }
    }
}

/// 计算 child 深度并校验是否超过上限（防失控递归）。纯函数，便于测试。
fn validate_spawn_depth(depth: u32, max_depth: u32) -> std::result::Result<u32, String> {
    let child_depth = depth.saturating_add(1);
    if child_depth > max_depth {
        Err(format!(
            "Spawn depth limit ({}) reached. Solve the task yourself instead of spawning.",
            max_depth
        ))
    } else {
        Ok(child_depth)
    }
}

// ============================================================================
// 子 agent 注册表（spawn 写、wait 读）
// ============================================================================

/// 持有后台子 agent 的 JoinHandle，drop 时 abort。
/// （② 生命周期：主 run 结束 → factory/registry drop → 所有子 handle drop → 子 task abort）
struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

struct ChildEntry {
    rx: watch::Receiver<ChildStatus>,
    _handle: AbortOnDrop,
}

pub(crate) struct ChildAgentRegistry {
    children: StdMutex<HashMap<String, ChildEntry>>,
}

impl ChildAgentRegistry {
    pub(crate) fn new() -> Self {
        Self {
            children: StdMutex::new(HashMap::new()),
        }
    }
    fn register(
        &self,
        task_name: &str,
        rx: watch::Receiver<ChildStatus>,
        handle: tokio::task::JoinHandle<()>,
    ) {
        self.children
            .lock()
            .expect("registry lock poisoned")
            .insert(
                task_name.to_string(),
                ChildEntry {
                    rx,
                    _handle: AbortOnDrop(Some(handle)),
                },
            );
    }
    fn get_rx(&self, task_name: &str) -> Option<watch::Receiver<ChildStatus>> {
        self.children
            .lock()
            .expect("registry lock poisoned")
            .get(task_name)
            .map(|e| e.rx.clone())
    }
    fn list_all(&self) -> Vec<String> {
        self.children
            .lock()
            .expect("registry lock poisoned")
            .keys()
            .cloned()
            .collect()
    }
    /// 移除一个子 agent entry（释放 JoinHandle，允许 task_name 复用）。
    fn remove(&self, task_name: &str) {
        self.children
            .lock()
            .expect("registry lock poisoned")
            .remove(task_name);
    }
    /// 取某 task 当前状态（用于判断是否已完成、可复用）。
    fn status_of(&self, task_name: &str) -> Option<ChildStatus> {
        self.children
            .lock()
            .expect("registry lock poisoned")
            .get(task_name)
            .map(|e| e.rx.borrow().clone())
    }
    /// 当前仍在运行（Running）的子 agent 数（③ 并发上限判定用）。
    fn count_running(&self) -> usize {
        self.children
            .lock()
            .expect("registry lock poisoned")
            .values()
            .filter(|e| !e.rx.borrow().is_done())
            .count()
    }
}

// ============================================================================
// ChildSession + ChildState —— 子 agent 的「会话」，历史从 task prompt 起步
// ============================================================================

struct ChildState(StdMutex<HashMap<String, Value>>);

impl State for ChildState {
    fn get(&self, key: &str) -> Option<Value> {
        self.0.lock().expect("child state lock poisoned").get(key).cloned()
    }
    fn set(&mut self, key: String, value: Value) {
        self.0
            .lock()
            .expect("child state lock poisoned")
            .insert(key, value);
    }
    fn all(&self) -> HashMap<String, Value> {
        self.0
            .lock()
            .expect("child state lock poisoned")
            .clone()
    }
}

struct ChildSession {
    id: String,
    app_name: String,
    user_id: String,
    state: ChildState,
    /// 子 agent 的初始「历史」= 任务指令（user role）。
    task_content: Content,
}

impl ChildSession {
    fn new(id: String, app_name: String, user_id: String, task_content: Content) -> Self {
        Self {
            id,
            app_name,
            user_id,
            state: ChildState(StdMutex::new(HashMap::new())),
            task_content,
        }
    }
}

impl Session for ChildSession {
    fn id(&self) -> &str {
        &self.id
    }
    fn app_name(&self) -> &str {
        &self.app_name
    }
    fn user_id(&self) -> &str {
        &self.user_id
    }
    fn state(&self) -> &dyn State {
        &self.state
    }
    fn conversation_history(&self) -> Vec<Content> {
        // 子 agent 从任务指令起步，不继承父的完整对话（避免污染 + 省 token）。
        vec![self.task_content.clone()]
    }
}

// ============================================================================
// ChildInvocationContext —— 包装父 ctx，换上子 agent + 子 session
// ============================================================================

pub(crate) struct ChildInvocationContext {
    parent: Arc<dyn InvocationContext>,
    child_agent: Arc<dyn Agent>,
    session: ChildSession,
    ended: AtomicBool,
}

impl ChildInvocationContext {
    fn new(
        parent: Arc<dyn InvocationContext>,
        child_agent: Arc<dyn Agent>,
        session: ChildSession,
    ) -> Self {
        Self {
            parent,
            child_agent,
            session,
            ended: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ReadonlyContext for ChildInvocationContext {
    fn invocation_id(&self) -> &str {
        self.parent.invocation_id()
    }
    fn agent_name(&self) -> &str {
        // agent_name 随子 agent（影响 history 过滤 / 事件归属）。
        self.child_agent.name()
    }
    fn user_id(&self) -> &str {
        self.parent.user_id()
    }
    fn app_name(&self) -> &str {
        self.parent.app_name()
    }
    fn session_id(&self) -> &str {
        self.parent.session_id()
    }
    fn branch(&self) -> &str {
        self.parent.branch()
    }
    fn user_content(&self) -> &Content {
        &self.session.task_content
    }
}

#[async_trait]
impl CallbackContext for ChildInvocationContext {
    fn artifacts(&self) -> Option<Arc<dyn Artifacts>> {
        self.parent.artifacts()
    }
    fn shared_state(&self) -> Option<Arc<SharedState>> {
        self.parent.shared_state()
    }
}

#[async_trait]
impl InvocationContext for ChildInvocationContext {
    fn agent(&self) -> Arc<dyn Agent> {
        self.child_agent.clone()
    }
    fn memory(&self) -> Option<Arc<dyn Memory>> {
        self.parent.memory()
    }
    fn session(&self) -> &dyn Session {
        &self.session
    }
    fn run_config(&self) -> &RunConfig {
        self.parent.run_config()
    }
    fn end_invocation(&self) {
        self.ended.store(true, Ordering::SeqCst);
    }
    fn ended(&self) -> bool {
        // 子 agent 既响应自身 end，也响应父 invocation 结束（父停则子停）。
        self.ended.load(Ordering::SeqCst) || self.parent.ended()
    }
}

// ============================================================================
// AgentBlueprint —— 从父 agent 克隆构建参数，用于 fork 同构子 agent
// ============================================================================

pub(crate) struct AgentBlueprint {
    description: String,
    model: Arc<dyn Llm>,
    instruction: Option<String>,
    memory_block: Option<String>,
    skill_catalog: Option<String>,
    skill_bodies: Option<String>,
    config: Option<GenerateContentConfig>,
    tools: Vec<Arc<dyn Tool>>,
    toolsets: Vec<Arc<dyn Toolset>>,
    max_iterations: u32,
    llm_timeout: Duration,
    tool_timeout: Duration,
    policy: PermissionPolicy,
    context_window: Option<usize>,
    compact_model: Option<Arc<dyn Llm>>,
    context_config: ContextConfig,
    hooks: Vec<Arc<dyn CompactionHook>>,
    sink: Arc<dyn ChildEventSink>,
}

impl AgentBlueprint {
    /// 用蓝图构建一个同构子 agent（name = task_name，spawn 深度 +1）。
    fn build_child(
        &self,
        task_name: &str,
        depth: u32,
        cancel: CancellationToken,
    ) -> std::result::Result<Arc<dyn Agent>, String> {
        let mut b = CortexAgentBuilder::new(task_name)
            .description(&self.description)
            .model(self.model.clone())
            .policy(self.policy)
            .cancel_token(cancel)
            .max_iterations(self.max_iterations)
            .llm_timeout(self.llm_timeout)
            .tool_timeout(self.tool_timeout)
            .context_config(self.context_config.clone())
            .spawn_depth(depth);
        if let Some(c) = &self.config {
            b = b.generate_content_config(c.clone());
        }
        for t in &self.tools {
            b = b.tool(t.clone());
        }
        for ts in &self.toolsets {
            b = b.toolset(ts.clone());
        }
        if let Some(i) = &self.instruction {
            b = b.instruction(i.clone());
        }
        if let Some(m) = &self.memory_block {
            b = b.memory_block(m.clone());
        }
        if let Some(c) = &self.skill_catalog {
            b = b.skill_catalog(c.clone());
        }
        if let Some(s) = &self.skill_bodies {
            b = b.skill_bodies(s.clone());
        }
        if let Some(w) = self.context_window {
            b = b.context_window(w);
        }
        if let Some(cm) = &self.compact_model {
            b = b.compact_model(cm.clone());
        }
        for h in &self.hooks {
            b = b.compaction_hook(h.clone());
        }
        // 子 agent 继承同一 sink：孙 agent 事件也能转发到主 SSE 流。
        b = b.child_event_sink(self.sink.clone());
        let agent = b.build().map_err(|e| e.to_string())?;
        Ok(Arc::new(agent) as Arc<dyn Agent>)
    }
}

impl super::CortexAgent {
    /// 从当前 agent 的配置克隆一份蓝图（供 fork 子 agent）。
    pub(super) fn child_blueprint(&self) -> AgentBlueprint {
        AgentBlueprint {
            description: self.description.clone(),
            model: self.model.clone(),
            instruction: self.instruction.clone(),
            memory_block: self.memory_block.clone(),
            skill_catalog: self.skill_catalog.clone(),
            skill_bodies: self.skill_bodies.clone(),
            config: self.config.clone(),
            tools: self.tools.clone(),
            toolsets: self.toolsets.clone(),
            max_iterations: self.max_iterations,
            llm_timeout: self.llm_timeout,
            tool_timeout: self.tool_timeout,
            policy: self.policy,
            context_window: self.context_window,
            compact_model: self.compact_model.clone(),
            context_config: self.context_config.clone(),
            hooks: self.hooks.clone(),
            sink: self.child_event_sink.clone(),
        }
    }
}

// ============================================================================
// ChildAgentFactory —— spawn_agent 工具背后的「fork + 后台运行」逻辑
// ============================================================================

pub(crate) struct ChildAgentFactory {
    blueprint: AgentBlueprint,
    registry: Arc<ChildAgentRegistry>,
    parent_ctx: Arc<dyn InvocationContext>,
    cancel_token: CancellationToken,
    sink: Arc<dyn ChildEventSink>,
    depth: u32,
    max_depth: u32,
    max_concurrent: usize,
}

impl ChildAgentFactory {
    pub(crate) fn new(
        blueprint: AgentBlueprint,
        registry: Arc<ChildAgentRegistry>,
        parent_ctx: Arc<dyn InvocationContext>,
        cancel_token: CancellationToken,
        sink: Arc<dyn ChildEventSink>,
        depth: u32,
        max_depth: u32,
        max_concurrent: usize,
    ) -> Self {
        Self {
            blueprint,
            registry,
            parent_ctx,
            cancel_token,
            sink,
            depth,
            max_depth,
            max_concurrent,
        }
    }

    pub(crate) fn spawn_handle(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(SpawnAgentTool::new(Arc::clone(self)))
    }
    pub(crate) fn wait_handle(&self, registry: Arc<ChildAgentRegistry>) -> Arc<dyn Tool> {
        Arc::new(WaitAgentTool::new(registry))
    }

    /// fork 一个子 agent 并在后台独立运行。
    async fn spawn(&self, task_name: &str, message: &str) -> std::result::Result<(), String> {
        let child_depth = validate_spawn_depth(self.depth, self.max_depth)?;
        // task_name 冲突处理：运行中→拒绝；已完成→清理后复用（避免 task_name 永久占用）。
        if let Some(status) = self.registry.status_of(task_name) {
            if !status.is_done() {
                return Err(format!(
                    "An agent named '{task_name}' is still running. Call wait_agent to collect its result first."
                ));
            }
            self.registry.remove(task_name);
        }
        // ③ 并发上限（对齐 codex AgentExecutionLimiter）：超过则拒绝，让模型先 wait 或自己做。
        if self.max_concurrent > 0 && self.registry.count_running() >= self.max_concurrent {
            return Err(format!(
                "Too many agents running in parallel (limit {}). Call wait_agent to collect finished ones before spawning more, or do the task yourself.",
                self.max_concurrent
            ));
        }

        let child_cancel = self.cancel_token.child_token();
        let child_agent = self
            .blueprint
            .build_child(task_name, child_depth, child_cancel.clone())?;

        // 子 session：继承父会话身份，历史从任务指令起步。
        let parent_sess = self.parent_ctx.session();
        let task_content = Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: message.to_string(),
            }],
        };
        let session = ChildSession::new(
            parent_sess.id().to_string(),
            parent_sess.app_name().to_string(),
            parent_sess.user_id().to_string(),
            task_content,
        );
        let child_ctx = Arc::new(ChildInvocationContext::new(
            self.parent_ctx.clone(),
            child_agent.clone(),
            session,
        ));

        let (tx, rx) = watch::channel(ChildStatus::Running);
        // ① 通知前端子 agent 已启动。
        self.sink
            .emit(ChildAgentEvent::Started {
                task_name: task_name.to_string(),
            });

        let sink = self.sink.clone();
        let task_name_owned = task_name.to_string();
        let cancel_for_check = child_cancel.clone();
        // 后台驱动子 agent；JoinHandle 交 registry 持有（② RAII：主结束 → registry drop → abort）。
        let handle = tokio::spawn(async move {
            let status = run_child_to_status(
                child_agent,
                child_ctx,
                sink.clone(),
                &task_name_owned,
                &cancel_for_check,
            )
            .await;
            // 通知 wait_agent（watch）。
            let _ = tx.send(status.clone());
            // ① 通知前端子 agent 结束。
            let (ok, result) = match &status {
                ChildStatus::Completed(s) => (true, s.clone()),
                ChildStatus::Failed(s) => (false, s.clone()),
                ChildStatus::Running => (false, "terminated".to_string()),
            };
            sink.emit(ChildAgentEvent::Finished {
                task_name: task_name_owned.clone(),
                ok,
                result,
            });
            tracing::info!("[multi_agent] 子 agent '{task_name_owned}' 后台运行结束");
        });
        self.registry.register(task_name, rx, handle);
        Ok(())
    }
}

/// 驱动子 agent run()，drain 其事件流，转发活动到 sink，提取最终文本作为结果。
/// 若期间被取消（cancel_token），报 Failed("cancelled") 而非 Completed。
async fn run_child_to_status(
    agent: Arc<dyn Agent>,
    ctx: Arc<dyn InvocationContext>,
    sink: Arc<dyn ChildEventSink>,
    task_name: &str,
    cancel: &CancellationToken,
) -> ChildStatus {
    match agent.run(ctx).await {
        Ok(stream) => {
            let status = drain_child_stream(stream, sink, task_name).await;
            if cancel.is_cancelled() {
                ChildStatus::Failed("cancelled".to_string())
            } else {
                status
            }
        }
        Err(e) => ChildStatus::Failed(format!("agent run failed: {e}")),
    }
}

/// 消费子 agent 的事件流：把工具调用/文本/工具结果转发到 sink（① 可视化），
/// 同时记录最后一个文本 turn 的输出作为最终答案。
async fn drain_child_stream(
    mut stream: EventStream,
    sink: Arc<dyn ChildEventSink>,
    task_name: &str,
) -> ChildStatus {
    let mut current = String::new();
    let mut final_answer = String::new();
    while let Some(ev_result) = stream.next().await {
        let ev = match ev_result {
            Ok(ev) => ev,
            Err(e) => return ChildStatus::Failed(format!("stream error: {e}")),
        };
        if let Some(content) = ev.llm_response.content.as_ref() {
            for p in &content.parts {
                match p {
                    Part::Text { text } => {
                        if !text.is_empty() {
                            current.push_str(text);
                            sink.emit(ChildAgentEvent::Text {
                                task_name: task_name.to_string(),
                                delta: text.clone(),
                            });
                        }
                    }
                    Part::FunctionCall { name, args, id, .. } => {
                        sink.emit(ChildAgentEvent::ToolCall {
                            task_name: task_name.to_string(),
                            tool_call_id: id.clone().unwrap_or_default(),
                            name: name.clone(),
                            args: args.to_string(),
                        });
                    }
                    Part::FunctionResponse {
                        function_response,
                        id,
                        ..
                    } => {
                        sink.emit(ChildAgentEvent::ToolResult {
                            task_name: task_name.to_string(),
                            tool_call_id: id.clone().unwrap_or_default(),
                            name: function_response.name.clone(),
                            content: function_response.response.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }
        // 每个 turn 结束时，把该 turn 累积的文本存为候选答案；最后一个文本 turn 即最终答复。
        if ev.llm_response.turn_complete || ev.llm_response.finish_reason.is_some() {
            if !current.trim().is_empty() {
                final_answer = std::mem::take(&mut current);
            } else {
                current.clear();
            }
        }
    }
    if final_answer.trim().is_empty() {
        ChildStatus::Completed("(agent produced no final text)".to_string())
    } else {
        ChildStatus::Completed(final_answer)
    }
}

// ============================================================================
// spawn_agent 工具
// ============================================================================

const SPAWN_DESC: &str = "Spawn a new sub-agent that runs a concrete, bounded subtask INDEPENDENTLY in the background, while you continue other useful work. Use this when a subtask is self-contained and parallelizable (e.g. investigating a separate module, exploring an alternative approach). Each spawned agent starts fresh (no shared conversation) and has the same tools as you. IMPORTANT: spawned agents run in parallel without coordinating writes — make each one work on INDEPENDENT files/resources; do NOT have two agents edit the same file. Do NOT use spawn for tasks that depend tightly on your current context or for trivial one-step work — do those yourself. Returns immediately; call wait_agent with the task_name to collect its result when ready. Keep task_name short and unique (e.g. 'explore_auth').";

pub(crate) struct SpawnAgentTool {
    factory: Arc<ChildAgentFactory>,
}

impl SpawnAgentTool {
    pub(crate) fn new(factory: Arc<ChildAgentFactory>) -> Self {
        Self { factory }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        SPAWN_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        SPAWN_DESC
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Short unique name for the spawned agent. Refer to it by this name in wait_agent."
                },
                "message": {
                    "type": "string",
                    "description": "The full task instruction for the spawned agent. Must be concrete and self-contained — the sub-agent starts fresh and does not inherit your conversation."
                }
            },
            "required": ["task_name", "message"]
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let task_name = args
            .get("task_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if task_name.is_empty() || message.is_empty() {
            return Ok(json!({ "error": "Both 'task_name' and 'message' are required." }));
        }
        match self.factory.spawn(&task_name, &message).await {
            Ok(()) => Ok(json!({
                "status": "spawned",
                "task_name": task_name,
                "message": format!(
                    "Agent '{task_name}' is now working on the task in the background. Continue with other useful work, then call wait_agent with task_names=[\"{task_name}\"] to collect its result."
                )
            })),
            Err(e) => Ok(json!({ "error": e })),
        }
    }
}

// ============================================================================
// wait_agent 工具
// ============================================================================

const WAIT_DESC: &str = "Wait for one or more previously spawned agents (spawn_agent) to finish and collect their results. Pass their task_names. Returns each agent's status (completed/failed/timeout/not_found) and its final answer. If task_names is omitted, waits for all currently spawned agents. Use this after spawning to gather parallel results before synthesizing.";

pub(crate) struct WaitAgentTool {
    registry: Arc<ChildAgentRegistry>,
}

impl WaitAgentTool {
    pub(crate) fn new(registry: Arc<ChildAgentRegistry>) -> Self {
        Self { registry }
    }

    async fn wait_for(&self, names: Vec<String>, timeout_secs: u64) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        let mut results = serde_json::Map::new();
        let mut still_running = Vec::new();
        for name in &names {
            match self.registry.get_rx(name) {
                Some(mut rx) => {
                    let current = rx.borrow().clone();
                    if current.is_done() {
                        results.insert(
                            name.clone(),
                            json!({ "status": current.label(), "result": current.result_text() }),
                        );
                        continue;
                    }
                    match tokio::time::timeout_at(deadline, rx.wait_for(|s| s.is_done())).await {
                        Ok(Ok(ref_status)) => {
                            let st = ref_status.clone();
                            results.insert(
                                name.clone(),
                                json!({ "status": st.label(), "result": st.result_text() }),
                            );
                        }
                        Ok(Err(_)) => {
                            // watch sender drop（子 agent task 意外终止且未发最终状态）
                            results.insert(
                                name.clone(),
                                json!({ "status": "failed", "result": "[agent ended without reporting]" }),
                            );
                        }
                        Err(_) => {
                            results.insert(
                                name.clone(),
                                json!({ "status": "timeout", "result": "[still running after wait timeout; call wait_agent again later]" }),
                            );
                            still_running.push(name.clone());
                        }
                    }
                }
                None => {
                    results.insert(
                        name.clone(),
                        json!({ "status": "not_found", "result": "No spawned agent with this task_name. Spawn it first with spawn_agent." }),
                    );
                }
            }
        }
        Ok(json!({
            "results": Value::Object(results),
            "still_running": still_running,
        }))
    }
}

#[async_trait]
impl Tool for WaitAgentTool {
    fn name(&self) -> &str {
        WAIT_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        WAIT_DESC
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "task_names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Names of spawned agents to wait for. Omit to wait for all currently spawned agents."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Maximum seconds to wait. Default 300. If an agent is still running after this, it is reported as timeout and keeps running."
                }
            }
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let names: Vec<String> = args
            .get("task_names")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(WAIT_DEFAULT_TIMEOUT_SECS);
        let names = if names.is_empty() {
            let all = self.registry.list_all();
            if all.is_empty() {
                return Ok(json!({
                    "results": {},
                    "message": "No spawned agents to wait for. Use spawn_agent first."
                }));
            }
            all
        } else {
            names
        };
        self.wait_for(names, timeout_secs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_status_done_and_labels() {
        assert!(!ChildStatus::Running.is_done());
        assert!(ChildStatus::Completed("ok".into()).is_done());
        assert!(ChildStatus::Failed("boom".into()).is_done());
        assert_eq!(ChildStatus::Running.label(), "running");
        assert_eq!(ChildStatus::Completed("x".into()).label(), "completed");
        assert_eq!(ChildStatus::Failed("y".into()).label(), "failed");
    }

    #[test]
    fn child_status_result_text() {
        assert_eq!(ChildStatus::Completed("answer".into()).result_text(), "answer");
        assert_eq!(ChildStatus::Failed("oops".into()).result_text(), "[failed: oops]");
        assert_eq!(ChildStatus::Running.result_text(), "[still running]");
    }

    #[test]
    fn spawn_depth_boundary() {
        // 顶层 depth=0, max=3 → child=1，允许。
        assert_eq!(validate_spawn_depth(0, 3).unwrap(), 1);
        // depth=2, max=3 → child=3，允许（恰好等于上限）。
        assert_eq!(validate_spawn_depth(2, 3).unwrap(), 3);
        // depth=3, max=3 → child=4 超限，拒绝。
        assert!(validate_spawn_depth(3, 3).is_err());
        // max=0 → 完全禁用 spawn（child=1 > 0）。
        assert!(validate_spawn_depth(0, 0).is_err());
        // 饱和：极大 depth 不会溢出，且必然拒绝。
        assert!(validate_spawn_depth(u32::MAX, 3).is_err());
    }

    #[test]
    fn child_session_history_is_just_the_task() {
        let task = Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: "investigate the auth module".to_string(),
            }],
        };
        let sess = ChildSession::new(
            "s1".to_string(),
            "app".to_string(),
            "u1".to_string(),
            task.clone(),
        );
        // 历史 = 仅任务指令，不继承父对话。
        let hist = sess.conversation_history();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].role, "user");
        // 身份字段透传。
        assert_eq!(sess.id(), "s1");
        assert_eq!(sess.app_name(), "app");
        assert_eq!(sess.user_id(), "u1");
    }

    #[test]
    fn child_state_get_set_all() {
        let task = Content {
            role: "user".to_string(),
            parts: vec![Part::Text { text: "t".to_string() }],
        };
        let sess = ChildSession::new("s".into(), "a".into(), "u".into(), task);
        let state = sess.state();
        assert!(state.get("missing").is_none());
        // set 需 &mut，但 state() 返回 &dyn State；通过 all() 验证初始为空。
        assert!(state.all().is_empty());
    }

    #[tokio::test]
    async fn registry_register_has_get_list() {
        let registry = ChildAgentRegistry::new();
        assert!(registry.status_of("a1").is_none());
        let (_tx, rx) = watch::channel(ChildStatus::Running);
        // register 需 JoinHandle（RAII：drop 时 abort 子 task）；用空 task 提供一个。
        let handle = tokio::spawn(async {});
        registry.register("a1", rx, handle);
        assert!(registry.status_of("a1").is_some());
        assert_eq!(registry.list_all(), vec!["a1".to_string()]);
        // get_rx 返回的 receiver 能读到当前状态。
        let rx2 = registry.get_rx("a1").expect("registered");
        assert!(matches!(rx2.borrow().clone(), ChildStatus::Running));
        assert!(registry.get_rx("nope").is_none());
        // ③ count_running：只统计仍 Running 的子 agent。
        assert_eq!(registry.count_running(), 1);
        let (_tx2, rx2b) = watch::channel(ChildStatus::Completed("ok".into()));
        let handle2 = tokio::spawn(async {});
        registry.register("a2", rx2b, handle2);
        assert_eq!(registry.count_running(), 1); // a2 已完成，不计入
    }
}
