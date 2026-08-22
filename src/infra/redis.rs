use std::time::Duration;

use bb8::Pool as RedisPool;
use bb8_redis::RedisConnectionManager;

/// Redis 连接池类型别名
pub type SharedRedisPool = RedisPool<RedisConnectionManager>;

/// 初始化 Redis 连接池
///
/// 连接失败时返回错误（不 panic），由调用方决定是否降级。
///
/// 设置 2 秒连接超时，避免 Redis 不可达时每次获取连接都卡 30 秒 TCP 超时。
pub async fn init_redis(url: String) -> anyhow::Result<SharedRedisPool> {
    let manager = RedisConnectionManager::new(url)
        .map_err(|e| anyhow::anyhow!("Failed to create Redis connection manager: {e}"))?;
    let pool = RedisPool::builder()
        .max_size(8)
        .connection_timeout(Duration::from_secs(2))
        .build(manager)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create Redis connection pool: {e}"))?;
    Ok(pool)
}
