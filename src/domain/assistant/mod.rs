//! 助手 bounded context（`docs/architecture.md` §2.3）。
//!
//! 管理「自定义/内置助手模板」的领域模型、持久化与分享逻辑。
//!
//! 子模块：
//! - [`enums`]：`AssistantKind` / `AgentType` / `Visibility`（SMALLINT 存储）
//! - [`models`]：`Assistant` 领域模型 + `AssistantRow` DB 行 + `AssistantPublicCard` 脱敏视图
//! - [`store`]：`AssistantStore` 数据访问（diesel-async）
//! - `share`：分享口令生成（M8 引入）

pub mod enums;
pub mod models;
pub mod store;

pub use enums::{AgentType, AssistantKind, Visibility};
pub use models::{Assistant, AssistantPublicCard, AssistantRow, CustomAssistantInput};
pub use store::AssistantStore;
