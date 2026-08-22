//! 通用 OIDC（OpenID Connect）适配器
//!
//! 支持任何符合 OIDC 标准的身份提供商（Keycloak、Authentik、Okta、自建 SSO 等）。
//!
//! 流程：
//! 1. （可选）从 `{issuer}/.well-known/openid-configuration` 自动发现端点
//! 2. 重定向到 authorization_endpoint
//! 3. 用授权码在 token_endpoint 换取 access_token
//! 4. 从 userinfo_endpoint 获取用户信息
//!
//! 如果配置中手动指定了 `authorize_url` / `token_url` / `userinfo_url`，则跳过 discovery。

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::AuthProviderConfig;
use crate::error::AppError;
use crate::security::crypto::AesCodec;

use super::super::models::ExternalIdentity;
use super::{OAuthProvider, make_key, resolve_secret};

pub struct OidcProvider {
    key: String,
    display_name: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    scope: String,
    issuer: String,
    authorize_url_override: String,
    token_url_override: String,
    userinfo_url_override: String,
    http: Client,
    discovery: Arc<Mutex<Option<DiscoveryDoc>>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DiscoveryDoc {
    #[serde(default)]
    authorization_endpoint: String,
    #[serde(default)]
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct TokenResp {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    token_type: String,
}

#[derive(Debug, Default, Deserialize)]
struct UserInfoResp {
    #[serde(default)]
    sub: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    picture: String,
    #[serde(default)]
    email: String,
}

impl OidcProvider {
    pub fn new(cfg: &AuthProviderConfig, http: &Client, aes: &AesCodec) -> Result<Self, AppError> {
        let client_secret = resolve_secret(&cfg.client_secret_enc, aes)?;
        if cfg.client_id.is_empty() {
            return Err(AppError::ConfigError("oidc provider 缺少 client_id".into()));
        }
        if cfg.redirect_uri.is_empty() {
            return Err(AppError::ConfigError(
                "oidc provider 缺少 redirect_uri".into(),
            ));
        }
        let has_all_overrides = !cfg.authorize_url.is_empty()
            && !cfg.token_url.is_empty()
            && !cfg.userinfo_url.is_empty();
        if cfg.issuer.is_empty() && !has_all_overrides {
            return Err(AppError::ConfigError(
                "oidc provider 必须配置 issuer（用于 discovery）或手动指定全部端点 URL".into(),
            ));
        }
        Ok(Self {
            key: make_key("oidc", &cfg.name),
            display_name: cfg.name.clone(),
            client_id: cfg.client_id.clone(),
            client_secret,
            redirect_uri: cfg.redirect_uri.clone(),
            scope: cfg.scope.clone(),
            issuer: cfg.issuer.clone(),
            authorize_url_override: cfg.authorize_url.clone(),
            token_url_override: cfg.token_url.clone(),
            userinfo_url_override: cfg.userinfo_url.clone(),
            http: http.clone(),
            discovery: Arc::new(Mutex::new(None)),
        })
    }

    async fn ensure_discovery(&self) -> Result<DiscoveryDoc, AppError> {
        {
            let guard = self.discovery.lock().await;
            if let Some(doc) = guard.as_ref() {
                return Ok(doc.clone());
            }
        }

        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(map_http_err)?
            .error_for_status()
            .map_err(map_http_err)?;

        let doc: DiscoveryDoc = resp
            .json()
            .await
            .map_err(|e| AppError::BusinessError(format!("OIDC discovery 文档解析失败: {e}")))?;

        let mut guard = self.discovery.lock().await;
        *guard = Some(doc.clone());
        Ok(doc)
    }

    async fn authorize_endpoint(&self) -> Result<String, AppError> {
        if !self.authorize_url_override.is_empty() {
            return Ok(self.authorize_url_override.clone());
        }
        Ok(self.ensure_discovery().await?.authorization_endpoint)
    }

    async fn token_endpoint(&self) -> Result<String, AppError> {
        if !self.token_url_override.is_empty() {
            return Ok(self.token_url_override.clone());
        }
        Ok(self.ensure_discovery().await?.token_endpoint)
    }

    async fn userinfo_endpoint(&self) -> Result<String, AppError> {
        if !self.userinfo_url_override.is_empty() {
            return Ok(self.userinfo_url_override.clone());
        }
        Ok(self.ensure_discovery().await?.userinfo_endpoint)
    }
}

#[async_trait]
impl OAuthProvider for OidcProvider {
    fn key(&self) -> &str {
        &self.key
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn kind(&self) -> &str {
        "oidc"
    }

    async fn authorize_url(&self, state: &str) -> Result<String, AppError> {
        let endpoint = self.authorize_endpoint().await?;
        if endpoint.is_empty() {
            return Err(AppError::BusinessError(
                "OIDC authorize_endpoint 为空（请检查 discovery 或手动配置）".into(),
            ));
        }
        Ok(format!(
            "{endpoint}?client_id={client_id}&redirect_uri={redirect}&response_type=code&scope={scope}&state={state}",
            client_id = urlencoding::encode(&self.client_id),
            redirect = urlencoding::encode(&self.redirect_uri),
            scope = urlencoding::encode(&self.scope),
            state = urlencoding::encode(state),
        ))
    }

    async fn exchange(&self, code: &str) -> Result<ExternalIdentity, AppError> {
        let token_url = self.token_endpoint().await?;
        let token = self.fetch_token(&token_url, code).await?;

        let userinfo_url = self.userinfo_endpoint().await?;
        let user_json = self
            .fetch_userinfo(&userinfo_url, &token.access_token, &token.token_type)
            .await?;
        parse_user_info(&user_json)
    }
}

impl OidcProvider {
    async fn fetch_token(&self, url: &str, code: &str) -> Result<TokenResp, AppError> {
        let resp = self
            .http
            .post(url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("redirect_uri", &self.redirect_uri),
            ])
            .send()
            .await
            .map_err(map_http_err)?
            .error_for_status()
            .map_err(map_http_err)?;

        let text = resp.text().await.map_err(map_http_err)?;
        let token: TokenResp = serde_json::from_str(&text)
            .map_err(|e| AppError::BusinessError(format!("OIDC token 响应解析失败: {e}")))?;
        if token.access_token.is_empty() {
            return Err(AppError::BusinessError(format!(
                "OIDC token 响应中缺少 access_token: {text}"
            )));
        }
        Ok(token)
    }

    async fn fetch_userinfo(
        &self,
        url: &str,
        access_token: &str,
        token_type: &str,
    ) -> Result<String, AppError> {
        let bearer = if token_type.is_empty() {
            format!("Bearer {access_token}")
        } else {
            format!("{} {access_token}", token_type.trim())
        };
        let resp = self
            .http
            .get(url)
            .header("Authorization", &bearer)
            .send()
            .await
            .map_err(map_http_err)?
            .error_for_status()
            .map_err(map_http_err)?;

        resp.text().await.map_err(map_http_err)
    }
}

fn map_http_err<E: std::fmt::Display>(e: E) -> AppError {
    AppError::NetworkError(format!("OIDC 请求失败: {e}"))
}

fn parse_user_info(body: &str) -> Result<ExternalIdentity, AppError> {
    let resp: UserInfoResp = serde_json::from_str(body)
        .map_err(|e| AppError::BusinessError(format!("OIDC userinfo 响应解析失败: {e}")))?;
    if resp.sub.is_empty() {
        return Err(AppError::BusinessError(
            "OIDC userinfo 中缺少 sub 字段".into(),
        ));
    }
    Ok(ExternalIdentity {
        provider: "oidc".into(),
        external_id: resp.sub,
        name: resp.name,
        avatar: resp.picture,
        email: resp.email,
        raw_payload: body.to_string(),
    })
}

// ===== 单元测试 =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_info_success() {
        let json = r#"{
            "sub": "1234567890",
            "name": "John Doe",
            "picture": "https://example.com/john.png",
            "email": "john@example.com",
            "email_verified": true
        }"#;
        let ext = parse_user_info(json).unwrap();
        assert_eq!(ext.provider, "oidc");
        assert_eq!(ext.external_id, "1234567890");
        assert_eq!(ext.name, "John Doe");
        assert_eq!(ext.email, "john@example.com");
    }

    #[test]
    fn parse_user_info_missing_sub() {
        let json = r#"{"name":"No Sub"}"#;
        let err = parse_user_info(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_user_info_bad_json() {
        let err = parse_user_info("{{{").unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_user_info_minimal() {
        let json = r#"{"sub":"user-42"}"#;
        let ext = parse_user_info(json).unwrap();
        assert_eq!(ext.external_id, "user-42");
        assert_eq!(ext.name, "");
    }

    fn make_cfg(issuer: &str, authorize: &str, token: &str, userinfo: &str) -> AuthProviderConfig {
        AuthProviderConfig {
            kind: "oidc".into(),
            name: "公司SSO".into(),
            client_id: "client-1".into(),
            client_secret_enc: "secret".into(),
            redirect_uri: "https://my.app/cb".into(),
            issuer: issuer.into(),
            authorize_url: authorize.into(),
            token_url: token.into(),
            userinfo_url: userinfo.into(),
            scope: "openid profile email".into(),
        }
    }

    #[test]
    fn new_requires_issuer_or_all_overrides() {
        let aes = AesCodec::from_passphrase("test");
        let http = Client::new();

        // 全空 → 报错
        let cfg = make_cfg("", "", "", "");
        assert!(OidcProvider::new(&cfg, &http, &aes).is_err());

        // 仅 issuer → OK
        let cfg = make_cfg("https://sso.example.com", "", "", "");
        assert!(OidcProvider::new(&cfg, &http, &aes).is_ok());

        // 全部 override → OK（不需要 issuer）
        let cfg = make_cfg(
            "",
            "https://sso.example.com/auth",
            "https://sso.example.com/token",
            "https://sso.example.com/userinfo",
        );
        assert!(OidcProvider::new(&cfg, &http, &aes).is_ok());

        // 部分 override（缺 userinfo）→ 因 issuer 也为空 → 报错
        let cfg = make_cfg(
            "",
            "https://sso.example.com/auth",
            "https://sso.example.com/token",
            "",
        );
        assert!(OidcProvider::new(&cfg, &http, &aes).is_err());
    }

    #[tokio::test]
    async fn authorize_url_with_override() {
        let aes = AesCodec::from_passphrase("test");
        let http = Client::new();
        let cfg = make_cfg(
            "",
            "https://sso/auth",
            "https://sso/token",
            "https://sso/userinfo",
        );
        let provider = OidcProvider::new(&cfg, &http, &aes).unwrap();

        let url = provider.authorize_url("state123").await.unwrap();
        assert!(url.starts_with("https://sso/auth?"));
        assert!(url.contains("client_id=client-1"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("state=state123"));
    }

    #[tokio::test]
    async fn discovery_caching_fetches_once() {
        // 用 wiremock 模拟 discovery 端点
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let discovery_body = serde_json::json!({
            "authorization_endpoint": format!("{}/auth", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "userinfo_endpoint": format!("{}/userinfo", server.uri()),
        });

        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery_body))
            // 最多被调用 1 次（验证 discovery 被缓存）
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let aes = AesCodec::from_passphrase("test");
        let http = Client::new();
        let cfg = make_cfg(&server.uri(), "", "", "");
        let provider = OidcProvider::new(&cfg, &http, &aes).unwrap();

        // 第一次触发 discovery
        let url1 = provider.authorize_url("s1").await.unwrap();
        assert!(url1.contains("/auth?"));
        // 第二次应命中缓存，不再次请求 discovery
        let url2 = provider.authorize_url("s2").await.unwrap();
        assert!(url2.contains("/auth?"));
    }
}
