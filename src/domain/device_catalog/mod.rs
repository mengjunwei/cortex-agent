//! 设备目录缓存模块 — 从 PostgreSQL `system_builtin` schema 加载厂商和设备类型
//!
//! 提供内存缓存 + 定期刷新机制，避免每次查询都访问数据库：
//! - 启动时首次加载全量目录
//! - 后台每 5 分钟自动刷新
//! - 支持模糊匹配（用于用户输入歧义消解）
//! - DB 不可用时降级为空缓存，不阻塞服务启动

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::error::AppError;
use crate::infra::db::DbPool;

/// `system_builtin.device_brand` / `device_type` 查询结果行
#[derive(Debug, Clone, QueryableByName)]
struct CatalogRow {
    #[diesel(sql_type = sql_types::Text)]
    id: String,
    #[diesel(sql_type = sql_types::Text)]
    name_ch: String,
    #[diesel(sql_type = sql_types::Text)]
    name_en: String,
}

/// 厂商/设备型号记录 — 对应 `system_builtin.device_brand` 或 `system_builtin.device_type` 表
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// 数据库主键（字符串化的整数）
    pub id: String,
    /// 中文名称（如 "华为"、"路由器"）
    pub name_ch: String,
    /// 英文名称（如 "Huawei"、"router"）
    pub name_en: String,
}

impl From<CatalogRow> for CatalogEntry {
    fn from(row: CatalogRow) -> Self {
        CatalogEntry {
            id: row.id,
            name_ch: row.name_ch,
            name_en: row.name_en,
        }
    }
}

/// 设备目录内存缓存 — 定期从 PostgreSQL 刷新
///
/// 缓存两类数据：
/// - `brands`：所有支持的设备厂商列表
/// - `dev_types`：所有支持的设备类型列表
///
/// 使用 `tokio::sync::RwLock` 实现读写并发安全。
pub struct CatalogCache {
    brands: RwLock<Vec<CatalogEntry>>,
    dev_types: RwLock<Vec<CatalogEntry>>,
    pool: DbPool,
    last_refresh: RwLock<Instant>,
    /// 是否已完成首次刷新(首刷 info、周期刷新 debug 的分级标记)
    refresh_logged: std::sync::atomic::AtomicBool,
}

impl CatalogCache {
    /// 创建设备目录缓存并启动后台定期刷新
    ///
    /// 首次同步加载全量目录，随后启动一个 tokio 任务每 5 分钟刷新一次。
    /// 如果首次加载失败会返回错误（调用方可降级为 `new_empty`）。
    pub async fn new(pool: DbPool) -> Result<Arc<Self>, AppError> {
        let cache = Arc::new(Self {
            brands: RwLock::new(Vec::new()),
            dev_types: RwLock::new(Vec::new()),
            pool,
            last_refresh: RwLock::new(Instant::now()),
            refresh_logged: std::sync::atomic::AtomicBool::new(false),
        });

        // 首次加载(初始化观测点;分级标记在 refresh 内部翻转)
        cache.refresh().await?;

        // 启动定期刷新（每 5 分钟）
        let cache_clone = Arc::clone(&cache);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            interval.tick().await; // 跳过第一次（已在 new 中加载）
            loop {
                interval.tick().await;
                if let Err(e) = cache_clone.refresh().await {
                    tracing::warn!("[catalog] 定期刷新失败: {}", e);
                }
            }
        });

        tracing::info!("[catalog] CatalogCache 初始化成功");
        Ok(cache)
    }

    /// 降级模式：创建空缓存，不启动后台刷新（DB 不可用时使用）
    pub fn new_empty(pool: DbPool) -> Arc<Self> {
        Arc::new(Self {
            brands: RwLock::new(Vec::new()),
            dev_types: RwLock::new(Vec::new()),
            pool,
            last_refresh: RwLock::new(Instant::now()),
            refresh_logged: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// 从 PostgreSQL 刷新缓存 — 查询 `system_builtin.device_brand` 和 `device_type` 表
    async fn refresh(&self) -> Result<(), AppError> {
        let mut conn = self.pool.get().await?;

        // 查询厂商
        let brand_rows = diesel::sql_query(
            "SELECT id::text, name_ch, name_en FROM system_builtin.device_brand ORDER BY id",
        )
        .get_results::<CatalogRow>(&mut conn)
        .await?;

        let brands: Vec<CatalogEntry> = brand_rows.into_iter().map(CatalogEntry::from).collect();

        // 查询设备类型
        let type_rows = diesel::sql_query(
            "SELECT id::text, name_ch, name_en FROM system_builtin.device_type ORDER BY id",
        )
        .get_results::<CatalogRow>(&mut conn)
        .await?;

        let dev_types: Vec<CatalogEntry> = type_rows.into_iter().map(CatalogEntry::from).collect();

        let brand_count = brands.len();
        let type_count = dev_types.len();

        *self.brands.write().await = brands;
        *self.dev_types.write().await = dev_types;
        *self.last_refresh.write().await = Instant::now();

        // 首刷 info(初始化观测);周期刷新(每 5 分钟,内容基本恒定)降 debug
        use std::sync::atomic::Ordering;
        if self.refresh_logged.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                "[catalog] 缓存刷新完成: {} 厂商, {} 设备类型",
                brand_count,
                type_count
            );
        } else {
            tracing::info!(
                "[catalog] 首次缓存加载完成: {} 厂商, {} 设备类型",
                brand_count,
                type_count
            );
        }
        Ok(())
    }

    /// 模糊匹配厂商 — 返回匹配列表（用于歧义消解）
    ///
    /// 匹配规则：中文名包含或被包含、英文名（不区分大小写）包含或被包含
    pub async fn match_brand(&self, input: &str) -> Vec<CatalogEntry> {
        let brands = self.brands.read().await;
        let input_lower = input.to_lowercase();
        brands
            .iter()
            .filter(|b| {
                b.name_ch.contains(input)
                    || b.name_en.to_lowercase().contains(&input_lower)
                    || input_lower.contains(&b.name_en.to_lowercase())
                    || input.contains(&b.name_ch)
            })
            .cloned()
            .collect()
    }

    /// 模糊匹配设备类型 — 返回匹配列表
    ///
    /// 匹配规则同 [`match_brand`](Self::match_brand)
    pub async fn match_dev_type(&self, input: &str) -> Vec<CatalogEntry> {
        let types = self.dev_types.read().await;
        let input_lower = input.to_lowercase();
        types
            .iter()
            .filter(|t| {
                t.name_ch.contains(input)
                    || t.name_en.to_lowercase().contains(&input_lower)
                    || input_lower.contains(&t.name_en.to_lowercase())
                    || input.contains(&t.name_ch)
            })
            .cloned()
            .collect()
    }

    /// 生成 JSON 格式的完整目录（供 API 和工具使用）
    pub async fn to_json(&self) -> Value {
        let brands = self.brands.read().await;
        let dev_types = self.dev_types.read().await;
        json!({
            "brands": brands.iter().map(|b| json!({
                "id": b.id,
                "name_ch": b.name_ch,
                "name_en": b.name_en,
            })).collect::<Vec<_>>(),
            "dev_types": dev_types.iter().map(|t| json!({
                "id": t.id,
                "name_ch": t.name_ch,
                "name_en": t.name_en,
            })).collect::<Vec<_>>(),
        })
    }
}
