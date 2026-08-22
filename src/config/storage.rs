//! 助手与对象存储配置段 — `[assistant]` / `[object_storage]`

use serde::Deserialize;

/// 助手配置 — 对应 `config.toml` 的 `[assistant]` 段（当前无配置项，保留占位以便后续扩展）
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AssistantConfig {}

/// 对象存储配置(`[object_storage]` 段)— S3 兼容,接 RustFS / MinIO / AWS S3
///
/// 用于截图 / 上传图 / artifact / 沙箱快照的共享存储(6+ 节点负载均衡场景)。
/// 详见 `docs/superpowers/specs/2026-08-04-object-storage-ha-design.md`。
#[derive(Debug, Clone, Deserialize)]
pub struct ObjectStorageConfig {
    /// 是否启用对象存储(默认 true)。生产多节点必须启用并配齐连接参数。
    #[serde(default = "default_os_enabled")]
    pub enabled: bool,
    /// S3 endpoint,如 `http://rustfs:9000`
    #[serde(default)]
    pub endpoint: String,
    /// region(默认 us-east-1)
    #[serde(default = "default_os_region")]
    pub region: String,
    /// bucket 名
    #[serde(default)]
    pub bucket: String,
    /// access key(敏感,不入日志)
    #[serde(default)]
    pub access_key: String,
    /// secret key(敏感,不入日志)
    #[serde(default)]
    pub secret_key: String,
    /// path-style 访问(RustFS/MinIO 用 true;AWS S3 虚拟主机风格用 false),默认 true
    #[serde(default = "default_os_path_style")]
    pub path_style: bool,
    /// presigned URL 有效期(秒),默认 7 天
    #[serde(default = "default_os_presign_ttl")]
    pub presign_ttl_secs: u64,
}

impl Default for ObjectStorageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: String::new(),
            region: default_os_region(),
            bucket: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            path_style: true,
            presign_ttl_secs: default_os_presign_ttl(),
        }
    }
}

fn default_os_enabled() -> bool {
    true
}
fn default_os_region() -> String {
    "us-east-1".to_string()
}
fn default_os_path_style() -> bool {
    true
}
fn default_os_presign_ttl() -> u64 {
    7 * 24 * 3600
}
