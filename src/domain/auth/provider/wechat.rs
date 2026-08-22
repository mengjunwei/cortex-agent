//! 微信（开放平台 / 网站应用）OAuth 适配器
//!
//! 微信扫码登录流程：
//! 1. 重定向到微信扫码授权页
//! 2. 用授权码换取 access_token + openid
//! 3. 用 access_token + openid 获取用户信息
//!
//! 官方文档：https://developers.weixin.qq.com/doc/oplatform/Website_App/WeChat_Login/WeChat_Login.html

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::config::AuthProviderConfig;
use crate::error::AppError;
use crate::security::crypto::AesCodec;

use super::super::models::ExternalIdentity;
use super::{OAuthProvider, make_key, resolve_secret};

const BASE_API: &str = "https://api.weixin.qq.com";
const AUTHORIZE_URL: &str = "https://open.weixin.qq.com/connect/qrconnect";

pub struct WeChatProvider {
    key: String,
    display_name: String,
    client_id: String,     // appid
    client_secret: String, // secret
    redirect_uri: String,
    http: Client,
}

impl WeChatProvider {
    pub fn new(cfg: &AuthProviderConfig, http: &Client, aes: &AesCodec) -> Result<Self, AppError> {
        let client_secret = resolve_secret(&cfg.client_secret_enc, aes)?;
        if cfg.client_id.is_empty() {
            return Err(AppError::ConfigError(
                "wechat provider 缺少 client_id".into(),
            ));
        }
        if client_secret.is_empty() {
            return Err(AppError::ConfigError(
                "wechat provider 缺少 client_secret_enc".into(),
            ));
        }
        if cfg.redirect_uri.is_empty() {
            return Err(AppError::ConfigError(
                "wechat provider 缺少 redirect_uri".into(),
            ));
        }
        Ok(Self {
            key: make_key("wechat", &cfg.name),
            display_name: cfg.name.clone(),
            client_id: cfg.client_id.clone(),
            client_secret,
            redirect_uri: cfg.redirect_uri.clone(),
            http: http.clone(),
        })
    }
}

#[async_trait]
impl OAuthProvider for WeChatProvider {
    fn key(&self) -> &str {
        &self.key
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn kind(&self) -> &str {
        "wechat"
    }

    async fn authorize_url(&self, state: &str) -> Result<String, AppError> {
        Ok(format!(
            "{url}?appid={appid}&redirect_uri={redirect}&response_type=code&scope=snsapi_login&state={state}",
            url = AUTHORIZE_URL,
            appid = urlencoding::encode(&self.client_id),
            redirect = urlencoding::encode(&self.redirect_uri),
            state = urlencoding::encode(state),
        ))
    }

    async fn exchange(&self, code: &str) -> Result<ExternalIdentity, AppError> {
        let token = self.fetch_access_token(code).await?;
        let user_json = self
            .fetch_user_info(&token.access_token, &token.openid)
            .await?;
        parse_user_info(&user_json)
    }
}

// ===== 微信 API 响应解析 =====

#[derive(Debug, Deserialize)]
struct AccessTokenResp {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    openid: String,
    #[serde(default)]
    errcode: i64,
    #[serde(default)]
    errmsg: String,
}

#[derive(Debug, Deserialize)]
struct UserInfoResp {
    #[serde(default)]
    openid: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    headimgurl: String,
    #[serde(default)]
    errcode: i64,
    #[serde(default)]
    errmsg: String,
}

impl WeChatProvider {
    async fn fetch_access_token(&self, code: &str) -> Result<AccessTokenResp, AppError> {
        let url = format!(
            "{base}/sns/oauth2/access_token?appid={appid}&secret={secret}&code={code}&grant_type=authorization_code",
            base = BASE_API,
            appid = urlencoding::encode(&self.client_id),
            secret = urlencoding::encode(&self.client_secret),
            code = urlencoding::encode(code),
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(map_http_err)?
            .error_for_status()
            .map_err(map_http_err)?;

        let text = resp.text().await.map_err(map_http_err)?;
        parse_access_token(&text)
    }

    async fn fetch_user_info(&self, access_token: &str, openid: &str) -> Result<String, AppError> {
        let url = format!(
            "{base}/sns/userinfo?access_token={token}&openid={openid}",
            base = BASE_API,
            token = urlencoding::encode(access_token),
            openid = urlencoding::encode(openid),
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(map_http_err)?
            .error_for_status()
            .map_err(map_http_err)?;

        resp.text().await.map_err(map_http_err)
    }
}

fn map_http_err<E: std::fmt::Display>(e: E) -> AppError {
    AppError::NetworkError(format!("微信 OAuth 请求失败: {e}"))
}

fn parse_access_token(body: &str) -> Result<AccessTokenResp, AppError> {
    let resp: AccessTokenResp = serde_json::from_str(body)
        .map_err(|e| AppError::BusinessError(format!("微信 access_token 响应解析失败: {e}")))?;
    if resp.errcode != 0 {
        return Err(AppError::BusinessError(format!(
            "微信 access_token 返回错误 errcode={}: {}",
            resp.errcode, resp.errmsg
        )));
    }
    if resp.access_token.is_empty() || resp.openid.is_empty() {
        return Err(AppError::BusinessError(
            "微信 access_token 响应中缺少 access_token 或 openid".into(),
        ));
    }
    Ok(resp)
}

fn parse_user_info(body: &str) -> Result<ExternalIdentity, AppError> {
    let resp: UserInfoResp = serde_json::from_str(body)
        .map_err(|e| AppError::BusinessError(format!("微信用户信息响应解析失败: {e}")))?;
    if resp.errcode != 0 {
        return Err(AppError::BusinessError(format!(
            "微信获取用户信息失败 errcode={}: {}",
            resp.errcode, resp.errmsg
        )));
    }
    if resp.openid.is_empty() {
        return Err(AppError::BusinessError("微信用户信息中缺少 openid".into()));
    }
    Ok(ExternalIdentity {
        provider: "wechat".into(),
        external_id: resp.openid,
        name: resp.nickname,
        avatar: resp.headimgurl,
        email: String::new(),
        raw_payload: body.to_string(),
    })
}

// ===== 单元测试 =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_access_token_success() {
        let json = r#"{
            "access_token": "ACCESS_TOKEN",
            "expires_in": 7200,
            "refresh_token": "REFRESH",
            "openid": "OPENID",
            "scope": "SCOPE",
            "unionid": "UNIONID"
        }"#;
        let resp = parse_access_token(json).unwrap();
        assert_eq!(resp.access_token, "ACCESS_TOKEN");
        assert_eq!(resp.openid, "OPENID");
    }

    #[test]
    fn parse_access_token_invalid_code() {
        let json = r#"{"errcode":40029,"errmsg":"invalid code"}"#;
        let err = parse_access_token(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_access_token_missing_openid() {
        let json = r#"{"access_token":"tok","errcode":0}"#;
        let err = parse_access_token(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_user_info_success() {
        let json = r#"{
            "openid": "OPENID",
            "nickname": "微信用户",
            "sex": 1,
            "headimgurl": "https://thirdwx.qlogo.cn/mmopen/xxx/132",
            "privilege": []
        }"#;
        let ext = parse_user_info(json).unwrap();
        assert_eq!(ext.provider, "wechat");
        assert_eq!(ext.external_id, "OPENID");
        assert_eq!(ext.name, "微信用户");
        assert_eq!(ext.email, "");
        assert!(ext.avatar.contains("qlogo.cn"));
    }

    #[test]
    fn parse_user_info_error_code() {
        let json = r#"{"errcode":48001,"errmsg":"api unauthorized"}"#;
        let err = parse_user_info(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[test]
    fn parse_user_info_missing_openid() {
        let json = r#"{"nickname":"无openid","errcode":0}"#;
        let err = parse_user_info(json).unwrap_err();
        assert!(matches!(err, AppError::BusinessError(_)));
    }

    #[tokio::test]
    async fn authorize_url_construction() {
        let aes = AesCodec::from_passphrase("test");
        let cfg = AuthProviderConfig {
            kind: "wechat".into(),
            name: "微信".into(),
            client_id: "wx123".into(),
            client_secret_enc: "secret".into(),
            redirect_uri: "https://my.app/cb".into(),
            issuer: String::new(),
            authorize_url: String::new(),
            token_url: String::new(),
            userinfo_url: String::new(),
            scope: "openid".into(),
        };
        let provider = WeChatProvider::new(&cfg, &Client::new(), &aes).unwrap();
        let url = provider.authorize_url("abc").await.unwrap();
        assert!(url.contains("appid=wx123"));
        assert!(url.contains("scope=snsapi_login"));
        assert!(url.contains("state=abc"));
        assert!(url.contains("response_type=code"));
    }
}
