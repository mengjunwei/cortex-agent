//! 初始化辅助函数 — 从 build_app_deps 中提取的子系统初始化逻辑

use std::sync::Arc;

use crate::config::AppConfig;

pub(super) async fn init_session_service(
    cfg: &AppConfig,
) -> anyhow::Result<Arc<dyn adk_rust::session::SessionService>> {
    tracing::info!("[infra] 初始化 PostgreSQL Session 服务...");
    tracing::debug!(
        "[infra] Session 数据库 URL: postgres://***@{}:{}/{}",
        cfg.db.host,
        cfg.db.port,
        cfg.db.db
    );

    let pg_service = adk_rust::session::PostgresSessionService::new(&cfg.db.url()).await?;
    tracing::info!("[infra] PostgreSQL Session 服务创建成功，开始迁移...");

    match tokio::time::timeout(std::time::Duration::from_secs(30), pg_service.migrate()).await {
        Ok(Ok(_)) => {
            tracing::info!("[infra] PostgreSQL Session 迁移完成");
        }
        Ok(Err(e)) => {
            tracing::warn!("[infra] PostgreSQL Session 迁移失败({})，继续启动", e);
        }
        Err(_) => {
            tracing::warn!("[infra] PostgreSQL Session 迁移超时(>1s)，继续启动");
        }
    }

    Ok(Arc::new(pg_service))
}

pub(super) async fn init_memory_service(
    cfg: &AppConfig,
) -> anyhow::Result<Arc<dyn adk_rust::Memory>> {
    let redis_config = adk_rust::memory::redis::RedisMemoryConfig {
        url: cfg.redis.url(),
        ttl: None,
    };
    let redis_service = adk_rust::memory::redis::RedisMemoryService::new(redis_config).await?;
    let adapter = adk_rust::memory::MemoryServiceAdapter::new(
        Arc::new(redis_service),
        "cortex-agent",
        "user",
    );
    Ok(Arc::new(adapter))
}

/// 初始化认证（SSO）服务
///
/// 装配链路：
/// 1. `UserStore` — 建表 `users` / `user_identities`
/// 2. `AesCodec` — 解密 provider 的 `client_secret`
/// 3. `reqwest::Client` — OAuth 出站 HTTP
/// 4. `ProviderRegistry` — 遍历 `[[auth.providers]]` 全部实例化
/// 5. `JwtService` — HS256
/// 6. `AuthService` — 编排以上组件 + Redis 黑名单
pub(super) async fn init_auth_service(
    cfg: &AppConfig,
    pool: crate::infra::db::DbPool,
    redis: Option<crate::infra::redis::SharedRedisPool>,
) -> anyhow::Result<Arc<crate::domain::auth::AuthService>> {
    use crate::domain::auth::{ApiTokenStore, AuthService, JwtService, ProviderRegistry, UserStore};

    let users = UserStore::new(pool.clone()).await?;
    let api_tokens = ApiTokenStore::new(pool).await?;

    let aes = crate::security::crypto::AesCodec::from_secrets();

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let registry = ProviderRegistry::from_config(&cfg.auth.providers, http, &aes)?;
    let jwt = JwtService::from_secrets(cfg.auth.token_ttl_secs)?;

    let svc = AuthService::new(
        users,
        api_tokens,
        Arc::new(registry),
        Arc::new(jwt),
        redis,
        cfg.auth.token_ttl_secs,
        cfg.auth.cookie_name.clone(),
    );

    Ok(Arc::new(svc))
}

/// Upsert MCP 种子服务器到 DB（按 slug 匹配，存在则更新 endpoint/args，不存在则创建）
pub(super) async fn upsert_mcp_seed(
    mgr: &crate::domain::mcp::McpManager,
    seed: &crate::config::McpSeedConfig,
) -> anyhow::Result<()> {
    use crate::infra::store_base::Store;
    use diesel_async::RunQueryDsl;

    let store = mgr.store();
    let mut conn = store.get_conn().await?;

    let id = uuid::Uuid::now_v7().to_string();
    let now = chrono::Utc::now();

    diesel::sql_query(
        r#"INSERT INTO mcp_servers (id, name, slug, transport, endpoint, args, env_enc, env_mask, headers_enc, headers_mask, status, tool_timeout_secs, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, '', '{}', '', '{}', 1, $8, $7, $7)
           ON CONFLICT (slug) DO UPDATE SET
             name = EXCLUDED.name,
             endpoint = EXCLUDED.endpoint,
             args = EXCLUDED.args,
             transport = EXCLUDED.transport,
             tool_timeout_secs = EXCLUDED.tool_timeout_secs,
             updated_at = EXCLUDED.updated_at"#,
    )
    .bind::<diesel::sql_types::VarChar, _>(&id)
    .bind::<diesel::sql_types::VarChar, _>(&seed.name)
    .bind::<diesel::sql_types::VarChar, _>(&seed.slug)
    .bind::<diesel::sql_types::SmallInt, _>(seed.transport)
    .bind::<diesel::sql_types::VarChar, _>(&seed.endpoint)
    .bind::<diesel::sql_types::Text, _>(&seed.args)
    .bind::<diesel::sql_types::Timestamptz, _>(now)
    .bind::<diesel::sql_types::Int4, _>(seed.tool_timeout_secs.unwrap_or(60) as i32)
    .execute(&mut *conn)
    .await?;

    tracing::info!(
        "[infra] MCP 种子 upsert 成功: slug={}, name={}",
        seed.slug,
        seed.name
    );
    Ok(())
}
