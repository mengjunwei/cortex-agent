//! OAuth 提供商抽象层
//!
//! - [`OAuthProvider`] trait 统一所有第三方身份提供商的接口
//! - [`ProviderRegistry`] 在启动时从配置数组实例化全部 provider 并注册
//! - 新增 provider 只需：实现 trait + 在 registry match 中加一行 + 配置一段

pub mod feishu;
pub mod oidc;
pub mod wechat;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;

use crate::config::AuthProviderConfig;
use crate::error::AppError;
use crate::security::crypto::AesCodec;

use super::models::ExternalIdentity;

/// OAuth 提供商统一接口
///
/// 每个具体实现（Feishu / WeChat / OIDC）封装自身的协议差异，
/// 对上层只暴露 `authorize_url` + `exchange` 两个核心操作。
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// 实例唯一键（格式 `kind:name`），用于路由 `/api/auth/callback/{key}`
    fn key(&self) -> &str;

    /// 前端展示名称
    fn display_name(&self) -> &str;

    /// 类型标识：`feishu` / `wechat` / `oidc`
    fn kind(&self) -> &str;

    /// 生成跳转到第三方的授权 URL（含 state 防 CSRF）
    async fn authorize_url(&self, state: &str) -> Result<String, AppError>;

    /// 用回调 code 换取统一身份信息
    async fn exchange(&self, code: &str) -> Result<ExternalIdentity, AppError>;
}

/// 前端展示用的 provider 摘要
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub key: String,
    pub kind: String,
    pub name: String,
}

/// 提供商注册表 — 启动时从配置构造，运行时按 key 查找
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn OAuthProvider>>,
    /// 保持配置声明顺序，供 list 使用
    order: Vec<String>,
}

impl ProviderRegistry {
    /// 从配置数组构造注册表。
    ///
    /// 容错策略：单个 provider 配置错误（字段缺失 / 未知 kind / 键重复）只跳过该 provider
    /// 并记录警告，不中断整个认证服务初始化。这样可避免「一个 SSO provider 配置不全
    /// 导致本地用户名密码登录也一并不可用」的脆弱行为——认证服务应尽可能可用。
    pub fn from_config(
        configs: &[AuthProviderConfig],
        http: Client,
        aes: &AesCodec,
    ) -> Result<Self, AppError> {
        let mut providers: HashMap<String, Arc<dyn OAuthProvider>> = HashMap::new();
        let mut order = Vec::new();

        for cfg in configs {
            // 构造单个 provider：失败则跳过并警告，不影响其他 provider 与本地登录
            let provider: Arc<dyn OAuthProvider> = match cfg.kind.as_str() {
                "feishu" => match feishu::FeishuProvider::new(cfg, &http, aes) {
                    Ok(p) => Arc::new(p),
                    Err(e) => {
                        tracing::warn!(
                            "[Auth] 跳过 feishu provider「{}」：{}（本地登录与其他 provider 不受影响）",
                            cfg.name,
                            e
                        );
                        continue;
                    }
                },
                "wechat" => match wechat::WeChatProvider::new(cfg, &http, aes) {
                    Ok(p) => Arc::new(p),
                    Err(e) => {
                        tracing::warn!(
                            "[Auth] 跳过 wechat provider「{}」：{}（本地登录与其他 provider 不受影响）",
                            cfg.name,
                            e
                        );
                        continue;
                    }
                },
                "oidc" => match oidc::OidcProvider::new(cfg, &http, aes) {
                    Ok(p) => Arc::new(p),
                    Err(e) => {
                        tracing::warn!(
                            "[Auth] 跳过 oidc provider「{}」：{}（本地登录与其他 provider 不受影响）",
                            cfg.name,
                            e
                        );
                        continue;
                    }
                },
                other => {
                    tracing::warn!(
                        "[Auth] 跳过未知类型的 provider「{}」（kind='{other}'，仅支持 feishu/wechat/oidc）",
                        cfg.name
                    );
                    continue;
                }
            };

            let key = provider.key().to_string();
            if providers.contains_key(&key) {
                tracing::warn!(
                    "[Auth] 跳过重复的 provider「{}」（键 '{key}' 已存在，kind + name 组合必须唯一）",
                    cfg.name
                );
                continue;
            }
            order.push(key.clone());
            providers.insert(key, provider);
        }

        tracing::info!("[Auth] 已注册 {} 个身份提供商", providers.len());
        Ok(Self { providers, order })
    }

    /// 按 key 查找 provider
    pub fn get(&self, key: &str) -> Option<Arc<dyn OAuthProvider>> {
        self.providers.get(key).cloned()
    }

    /// 列出全部 provider（按配置顺序）
    pub fn list(&self) -> Vec<ProviderInfo> {
        self.order
            .iter()
            .filter_map(|k| self.providers.get(k))
            .map(|p| ProviderInfo {
                key: p.key().to_string(),
                kind: p.kind().to_string(),
                name: p.display_name().to_string(),
            })
            .collect()
    }

    /// 是否注册了至少一个 provider
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// 解析 `client_secret_enc`：`enc:` 前缀走 AesCodec 解密，无前缀视为明文（开发模式）
pub(crate) fn resolve_secret(enc_or_plain: &str, aes: &AesCodec) -> Result<String, AppError> {
    if let Some(ciphertext) = enc_or_plain.strip_prefix("enc:") {
        aes.decrypt(ciphertext)
            .map_err(|e| AppError::ConfigError(format!("client_secret 解密失败: {e}")))
    } else {
        Ok(enc_or_plain.to_string())
    }
}

/// 构造实例键
///
/// 使用 `-` 作为分隔符（而非 `:`），因为该键会出现在回调 URL 的路径段中
/// （如 `/api/auth/callback/{key}`）。冒号虽在 URL path 中合法，但部分 IdP
/// （如飞书）在重定向 URL 白名单匹配时对冒号处理不一致，可能导致校验失败。
pub(crate) fn make_key(kind: &str, name: &str) -> String {
    format!("{kind}-{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_secret_plain_passthrough() {
        let aes = AesCodec::from_passphrase("test");
        assert_eq!(resolve_secret("my-secret", &aes).unwrap(), "my-secret");
    }

    #[test]
    fn resolve_secret_enc_roundtrip() {
        let aes = AesCodec::from_passphrase("test");
        let encrypted = format!("enc:{}", aes.encrypt("hidden-value").unwrap());
        assert_eq!(resolve_secret(&encrypted, &aes).unwrap(), "hidden-value");
    }

    #[test]
    fn resolve_secret_enc_bad_ciphertext_errors() {
        let aes = AesCodec::from_passphrase("test");
        let result = resolve_secret("enc:garbage!!!", &aes);
        assert!(result.is_err());
    }

    #[test]
    fn make_key_format() {
        assert_eq!(make_key("feishu", "飞书"), "feishu-飞书");
    }

    #[test]
    fn registry_empty_when_no_configs() {
        let aes = AesCodec::from_passphrase("test");
        let http = Client::new();
        let reg = ProviderRegistry::from_config(&[], http, &aes).unwrap();
        assert!(reg.is_empty());
        assert!(reg.list().is_empty());
    }
}
