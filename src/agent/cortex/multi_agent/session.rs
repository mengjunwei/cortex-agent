//! 子 agent 会话 —— 持久历史（fork + mailbox 注入 + turn 累积）。

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use adk_rust::serde_json::Value;
use adk_rust::{Content, Session, State};

struct ChildState(StdMutex<HashMap<String, Value>>);

impl State for ChildState {
    fn get(&self, key: &str) -> Option<Value> {
        self.0
            .lock()
            .expect("child state lock poisoned")
            .get(key)
            .cloned()
    }
    fn set(&mut self, key: String, value: Value) {
        self.0
            .lock()
            .expect("child state lock poisoned")
            .insert(key, value);
    }
    fn all(&self) -> HashMap<String, Value> {
        self.0.lock().expect("child state lock poisoned").clone()
    }
}

/// 子 agent 的会话：历史 = fork 历史 + 任务指令 + 每轮注入的 mailbox 消息 + turn 累积。
///
/// 子 agent 循环把每轮 conv 增量追加进 `history`，跨 turn 持久（这是 V2「持久会话」
/// 与 V1「一次性 run」的本质区别——followup_task 的上下文延续依赖它）。
pub(crate) struct ChildSession {
    id: String,
    app_name: String,
    user_id: String,
    state: ChildState,
    history: StdMutex<Vec<Content>>,
}

impl ChildSession {
    pub(super) fn new(id: String, app_name: String, user_id: String, initial: Vec<Content>) -> Self {
        Self {
            id,
            app_name,
            user_id,
            state: ChildState(StdMutex::new(HashMap::new())),
            history: StdMutex::new(initial),
        }
    }
    pub(super) fn push(&self, c: Content) {
        self.history.lock().expect("child history poisoned").push(c);
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
        self.history.lock().expect("child history poisoned").clone()
    }
}
