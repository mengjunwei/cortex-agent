//! 只读数据库查询工具集：nyetdb v0.3.1 移植版（`DB_IMPL` 仅接受 `nyet`，
//! 显式设其他值报配置错误）。
//!
//! [`nyet`]：sqlparser AST 只读验证（fail closed）、Unicode 控制字符剥离、
//! MySQL 可执行注释检测、EXPLAIN 护栏、PII 双网（查询前 net A + 结果溯源
//! net B）、每查一连接 + `BEGIN READ ONLY`、JSON 信封输出（`{"v":1,"ok":...}`，
//! 错误码契约 NYET / CONNECTION_FAILED / DB_ERROR / TIMEOUT / CONFIG_INVALID）。
//!
//! 实现共用 [`config`] 的 DB_* env 解析，MCP 工具面四个入口：
//! db_query / db_schema / db_sample / db_explain。
//!
//! # 退出码约定
//!
//! DB_* 配置无效或启动自检失败 → 进程以 **exit code 2** 退出（stderr 说明原因），
//! cortex 的 MCP 探活立即转红；下次探测/使用时 McpManager 重新拉起进程，自愈。

pub mod config;
pub mod nyet;

pub use config::DbEnv;

use std::sync::Arc;

use nyet::NyetDb;

/// 统一工具门面：server.rs 只见这四个入口 + 启动自检。
///
/// Arc：ToolServer 需要 Clone（rmcp 注册面），而 NyetDb 的 Policy 无 Clone；
/// 方法全是 &self，Arc 共享无副作用（引擎本就每查一连接）。
#[derive(Clone)]
pub struct DbTools(Arc<NyetDb>);

impl DbTools {
    /// 构建 + 启动自检（nyet 全流水线探查）。Err（中文，操作者可见）→ main exit 2。
    pub async fn start(env: DbEnv) -> Result<DbTools, String> {
        let db = NyetDb::new(&env)?;
        db.probe().await?;
        Ok(DbTools(Arc::new(db)))
    }

    /// db_query：单条只读 SQL（JSON 信封输出）。
    pub async fn query(&self, sql: &str, limit: Option<u64>) -> String {
        self.0.query(sql, limit).await
    }

    /// db_schema：无 table → 表清单；有 table → 单表明细。
    pub async fn schema(&self, table: Option<&str>) -> String {
        self.0.schema(table).await
    }

    /// db_sample：随机抽 N 行。
    pub async fn sample(&self, table: &str, limit: Option<u64>) -> String {
        self.0.sample(table, limit).await
    }

    /// db_explain：计划与代价预估（不执行语句本身）。
    pub async fn explain(&self, sql: &str) -> String {
        self.0.explain(sql).await
    }
}
