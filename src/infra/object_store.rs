//! 对象存储基础设施 — 基于 opendal 封装 S3 兼容(RustFS / MinIO / AWS S3)客户端。
//!
//! 归属基础设施层(架构 §2.4),通过 `AppDeps` 注入;业务层经领域服务间接调用(§9 #6)。
//! 错误统一映射 `AppError::ObjectStoreError`(§9 #8),禁 `anyhow` / `unwrap`。
//!
//! 共享 key 命名见 spec §7:
//! - `screenshots/{session_id}/{filename}`
//! - `uploads/{user_id}/{filename}`
//! - `artifacts/{app}/{user}/{session}/{file}/v{version}`
//! - `workspaces/{session_id}/snapshot.tar.zst`

use std::time::Duration;

use bytes::Bytes;
use opendal::Operator;
use opendal::layers::RetryLayer;
use opendal::services::S3;

use crate::config::ObjectStorageConfig;
use crate::error::AppError;

/// S3 兼容对象存储客户端(截图 / 上传图 / artifact / 沙箱快照共用)。
#[derive(Clone)]
pub struct ObjectStore {
    op: Operator,
    /// presigned URL 默认有效期(来自配置 `presign_ttl_secs`)
    presign_ttl: Duration,
}

impl ObjectStore {
    /// 按 `[object_storage]` 配置构造客户端并完成连通性自检。
    ///
    /// 调用方负责在外层判定 `enabled`;本函数被调用即视为启用,会校验必填项并连接。
    pub async fn new(cfg: &ObjectStorageConfig) -> Result<Self, AppError> {
        for (name, val) in [
            ("endpoint", cfg.endpoint.as_str()),
            ("bucket", cfg.bucket.as_str()),
            ("access_key", cfg.access_key.as_str()),
            ("secret_key", cfg.secret_key.as_str()),
        ] {
            if val.trim().is_empty() {
                return Err(AppError::ObjectStoreError(format!(
                    "对象存储配置缺项:{name}(在 [object_storage] 段补齐)"
                )));
            }
        }

        let mut builder = S3::default()
            .bucket(&cfg.bucket)
            .endpoint(&cfg.endpoint)
            .region(&cfg.region)
            .access_key_id(&cfg.access_key)
            .secret_access_key(&cfg.secret_key);
        // path_style=true(RustFS/MinIO)→ 不启用虚拟主机风格;AWS S3 设 false 走虚拟主机。
        if !cfg.path_style {
            builder = builder.enable_virtual_host_style();
        }

        // opendal 0.58：layer 直接返回 Operator，不再需要 finish()
        let op = Operator::new(builder)
            .map_err(|e| AppError::ObjectStoreError(format!("opendal operator 构建失败:{e}")))?
            .layer(RetryLayer::new());

        // 连通性自检:确认 bucket 可访问,启动期暴露配置错误而非运行时才报。
        if let Err(e) = op.check().await {
            return Err(AppError::ObjectStoreError(format!(
                "对象存储连通性自检失败(endpoint={}, bucket={}):{e}",
                cfg.endpoint, cfg.bucket
            )));
        }

        tracing::info!(
            "[infra] object store 就绪:endpoint={} bucket={} path_style={}",
            cfg.endpoint,
            cfg.bucket,
            cfg.path_style
        );

        Ok(Self {
            op,
            presign_ttl: Duration::from_secs(cfg.presign_ttl_secs),
        })
    }

    /// 默认 presigned GET 有效期(来自配置)。
    pub fn default_presign_ttl(&self) -> Duration {
        self.presign_ttl
    }

    /// 写入对象(覆盖)。
    pub async fn put(&self, key: &str, data: Bytes) -> Result<(), AppError> {
        self.op
            .write(key, data)
            .await
            // opendal 0.58：write 返回 Metadata，这里只关心成功与否
            .map(|_| ())
            .map_err(|e| AppError::ObjectStoreError(format!("put 对象失败 key='{key}':{e}")))
    }

    /// 读取对象全部字节。
    pub async fn get(&self, key: &str) -> Result<Bytes, AppError> {
        self.op
            .read(key)
            .await
            .map(|buf| buf.to_bytes())
            .map_err(|e| AppError::ObjectStoreError(format!("get 对象失败 key='{key}':{e}")))
    }

    /// 删除单个对象;对象不存在视为成功(S3 语义)。
    #[allow(dead_code)] // 预留给三期 ArtifactService 实现;当前业务用 delete_prefix
    pub async fn delete(&self, key: &str) -> Result<(), AppError> {
        self.op
            .delete(key)
            .await
            .map_err(|e| AppError::ObjectStoreError(format!("delete 对象失败 key='{key}':{e}")))
    }

    /// 递归删除某前缀下所有对象。
    pub async fn delete_prefix(&self, prefix: &str) -> Result<(), AppError> {
        // opendal 0.58：remove_all 弃用，改 delete_with + recursive(true)
        self.op
            .delete_with(prefix)
            .recursive(true)
            .await
            .map_err(|e| {
                AppError::ObjectStoreError(format!("delete_prefix 失败 prefix='{prefix}':{e}"))
            })
    }

    /// 列出某前缀下所有对象 key(非递归地返回带前缀的全路径)。
    #[allow(dead_code)] // 预留给三期 ArtifactService 实现
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>, AppError> {
        self.op
            .list(prefix)
            .await
            .map(|entries| entries.into_iter().map(|e| e.path().to_string()).collect())
            .map_err(|e| AppError::ObjectStoreError(format!("list 失败 prefix='{prefix}':{e}")))
    }

    /// 对象是否存在。
    pub async fn exists(&self, key: &str) -> Result<bool, AppError> {
        self.op
            .exists(key)
            .await
            .map_err(|e| AppError::ObjectStoreError(format!("exists 失败 key='{key}':{e}")))
    }

    /// 生成 presigned GET URL;`ttl` 为有效期。模型 / 前端凭此直链拉取,无需 cortex 中转。
    pub async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, AppError> {
        self.op
            .presign_read(key, ttl)
            .await
            .map(|req| req.uri().to_string())
            .map_err(|e| AppError::ObjectStoreError(format!("presign_get 失败 key='{key}':{e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> ObjectStorageConfig {
        ObjectStorageConfig {
            enabled: true,
            endpoint: "http://localhost:9000".into(),
            region: "us-east-1".into(),
            bucket: "cortex".into(),
            access_key: "cortex".into(),
            secret_key: "cortex12345".into(),
            path_style: true,
            presign_ttl_secs: 604800,
        }
    }

    /// 验证 ObjectStore 连本地 RustFS 的 put/get/delete/presign 全链路。
    /// 前置:docker run rustfs(localhost:9000)+ 已建 bucket `cortex`(mc mb rustfs/cortex)。
    /// 无 RustFS 时跳过(不 fail),避免污染常规 cargo test。
    #[tokio::test]
    async fn rustfs_roundtrip() {
        let os = match ObjectStore::new(&test_cfg()).await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("跳过:ObjectStore 初始化失败(RustFS 未起或 bucket 不存在):{e}");
                return;
            }
        };
        let key = "test/objectstore-roundtrip.bin";
        os.put(key, Bytes::from_static(b"hello-rustfs"))
            .await
            .expect("put 对象失败");
        let got = os.get(key).await.expect("get 对象失败");
        assert_eq!(got.as_ref(), b"hello-rustfs");
        let url = os
            .presign_get(key, Duration::from_secs(60))
            .await
            .expect("presign 失败");
        assert!(
            url.starts_with("http://localhost:9000/"),
            "presigned url 异常: {url}"
        );
        os.delete(key).await.expect("delete 失败");
        assert!(os.get(key).await.is_err(), "删除后应取不到");
    }
}
