//! 飞书（Lark）OAuth 适配器
//!
//! 飞书网页登录流程（v1）：
//! 1. 重定向到飞书授权页
//! 2. 用授权码换取 `app_access_token`（需 app_id + app_secret）
//! 3. 用 app_access_token + 授权码换取用户信息
//!
//! 官方文档：https://open.feishu.cn/document/server-docs/authentication-management

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::config::AuthProviderConfig;
use crate::error::AppError;
use crate::model_provider::crypto::AesCodec;

use super::super::models::ExternalIdentity;
use super::{OAuthProvider, make_key, resolve_secret};

const BASE_API: &str = "https://open.feishu.cn";

pub struct FeishuProvider {
    key: String,
    display_name: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    http: Client,
}

impl FeishuProvider {
    pub fn new(cfg: &AuthProviderConfig, http: &Client, aes: &AesCodec) -> Result<Self, AppError> {
        let client_secret = resolve_secret(&cfg.client_secret_enc, aes)?;
        if cfg.client_id.is_empty() {
            return Err(AppError::ConfigError(
                "feishu provider 缺少 client_id".into(),
            ));
        }
        if client_secret.is_empty() {
            return Err(AppError::ConfigError(
                "feishu provider 缺少 client_secret_enc".into(),
            ));
        }
        if cfg.redirect_uri.is_empty() {
            return Err(AppError::ConfigError(
                "feishu provider 缺少 redirect_uri".into(),
            ));
        }
        Ok(Self {
            key: make_key("feishu", &cfg.name),
            display_name: cfg.name.clone(),
            client_id: cfg.client_id.clone(),
            client_secret,
            redirect_uri: cfg.redirect_uri.clone(),
            http: http.clone(),
        })
    }
}

#[async_trait]
impl OAuthProvider for FeishuProvider {
    fn key(&self) -> &str {
        &self.key
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn kind(&self) -> &str {
        "feishu"
    }

    async fn authorize_url(&self, state: &str) -> Result<String, AppError> {
        Ok(format!(
            "{base}/open-apis/authen/v1/authorize?client_id={client_id}&response_type=code&redirect_uri={redirect}&state={state}",
            base = BASE_API,
            client_id = urlencoding::encode(&self.client_id),
            redirect = urlencoding::encode(&self.redirect_uri),
            state = urlencoding::encode(state),
        ))
    }

    async fn exchange(&self, code: &str) -> Result<ExternalIdentity, AppError> {
        let app_access_token = self.fetch_app_access_token().await?;
        let user_json = self.fetch_user_info(&app_access_token, code).await?;
        parse_user_info(&user_json)
    }
}

// ===== 飞书 API 响应解析 =====

/// app_access_token 响应
#[derive(Debug, Deserialize)]
struct AppAccessTokenResp {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    app_access_token: Option<String>,
}

/// authen/v1/access_token 响应（用户信息在 data 字段内）
#[derive(Debug, Deserialize)]
struct UserInfoResp {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<UserInfoData>,
}

#[derive(Debug, Default, Deserialize)]
struct UserInfoData {
    #[serde(default)]
    name: String,
    #[serde(default)]
    avatar_url: String,
    #[serde(default)]
    open_id: String,
    #[serde(default)]
    email: String,
}

impl FeishuProvider {
    async fn fetch_app_access_token(&self) -> Result<String, AppError> {
        let url = format!(
            "{base}/open-apis/auth/v3/app_access_token/internal",
            base = BASE_API
        );
        let body = serde_json::json!({
            "app_id": self.client_id,
            "app_secret": self.client_secret,
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(map_http_err)?
            .error_for_status()
            .map_err(map_http_err)?;

        let text = resp.text().await.map_err(map_http_err)?;
        parse_app_access_token(&text)
    }

    async fn fetch_user_info(
        &self,
        app_access_token: &str,
        code: &str,
    ) -> Result<String, AppError> {
        let url = format!("{base}/open-apis/authen/v1/access_token", base = BASE_API);
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
        });
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {app_access_token}"))
            .json(&body)
            .send()
            .await
            .map_err(map_http_err)?
            .error_for_status()
            .map_err(map_http_err)?;

        resp.text().await.map_err(map_http_err)
    }
}

fn map_http_err<E: std::fmt::Display>(e: E) -> AppError {
    AppError::NetworkError(format!("飞书 OAuth 请求失败: {e}"))
}

fn parse_app_access_token(body: &str) -> Result<String, AppError> {
    let resp: AppAccessTokenResp = serde_json::from_str(body)
        .map_err(|e| AppError::BusinessError(format!("飞书 app_access_token 响应解析失败: {e}")))?;
    if resp.code != 0 {
        return Err(AppError::BusinessError(format!(
            "飞书 app_access_token 返回错误 code={}: {}",
            resp.code, resp.msg
        )));
    }
    resp.app_access_token
        .ok_or_else(|| AppError::BusinessError("飞书 app_access_token 响应中缺少 token".into()))
}

fn parse_user_info(body: &str) -> Result<ExternalIdentity, AppError> {
    let resp: UserInfoResp = serde_json::from_str(body)
        .map_err(|e| AppError::BusinessError(format!("飞书用户信息响应解析失败: {e}")))?;
    if resp.code != 0 {
        return Err(AppError::BusinessError(format!(
            "飞书获取用户信息失败 code={}: {}",
            resp.code, resp.msg
        )));
    }
    let data = resp.data.unwrap_or_default();
    if data.open_id.is_empty() {
        return Err(AppError::BusinessError("飞书用户信息中缺少 open_id".into()));
    }
    Ok(ExternalIdentity {
        provider: "feishu".into(),
        external_id: data.open_id,
        name: data.name,
        avatar: data.avatar_url,
        email: data.email,
        raw_payload: body.to_string(),
    })
}

// ===== 单元测试 =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_app_access_token_success() {
        let json = r#"{"code":0,"msg":"ok","app_access_token":"t-abc123","expire":7200}"#;
        assert_eq!(parse_app_access_token(json).unwrap(), "t-abc123");
    }

    #[test]
    fn parse_app_access_token_error_code() {
        let json = r#"{"code":99991663,"msg":"app_secret invalid"}"#;
        let err = parse_app_access_token(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_app_access_token_missing_token() {
        let json = r#"{"code":0,"msg":"ok"}"#;
        let err = parse_app_access_token(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_app_access_token_bad_json() {
        let err = parse_app_access_token("not json").unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_user_info_success() {
        let json = r#"{
            "code": 0,
            "msg": "success",
            "data": {
                "name": "张三",
                "avatar_url": "https://img.example.com/avatar.png",
                "open_id": "ou_abcdefg",
                "email": "zhangsan@example.com"
            }
        }"#;
        let ext = parse_user_info(json).unwrap();
        assert_eq!(ext.provider, "feishu");
        assert_eq!(ext.external_id, "ou_abcdefg");
        assert_eq!(ext.name, "张三");
        assert_eq!(ext.email, "zhangsan@example.com");
        assert!(!ext.raw_payload.is_empty());
    }

    #[test]
    fn parse_user_info_missing_open_id() {
        let json = r#"{"code":0,"data":{"name":"无open_id的人"}}"#;
        let err = parse_user_info(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_user_info_error_code() {
        let json = r#"{"code":99991668,"msg":"invalid code"}"#;
        let err = parse_user_info(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_user_info_partial_data() {
        let json = r#"{"code":0,"data":{"open_id":"ou_x"}}"#;
        let ext = parse_user_info(json).unwrap();
        assert_eq!(ext.external_id, "ou_x");
        assert_eq!(ext.name, "");
        assert_eq!(ext.email, "");
    }

    #[tokio::test]
    async fn authorize_url_construction() {
        let aes = AesCodec::from_passphrase("test");
        let cfg = AuthProviderConfig {
            kind: "feishu".into(),
            name: "飞书".into(),
            client_id: "cli_123".into(),
            client_secret_enc: "secret".into(),
            redirect_uri: "https://my.app/cb".into(),
            issuer: String::new(),
            authorize_url: String::new(),
            token_url: String::new(),
            userinfo_url: String::new(),
            scope: "openid".into(),
        };
        let provider = FeishuProvider::new(&cfg, &Client::new(), &aes).unwrap();
        let url = provider.authorize_url("xyz").await.unwrap();
        assert!(url.contains("client_id=cli_123"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fmy.app%2Fcb"));
        assert!(url.contains("state=xyz"));
    }
}
