use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;

use super::AppState;
use super::{auth, knowledge_instances};

#[derive(Debug, Deserialize)]
pub(super) struct KbProxyImageParams {
    /// 知识库实例 id（取该实例的 SECRET_KEY 做签名）
    pub(super) i: String,
    /// 原始图片 URL
    pub(super) u: String,
}

/// 从 `https://host/...` / `http://host/...` 中取 host 部分（含端口）。
fn url_host(u: &str) -> Option<&str> {
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))?;
    rest.split('/').next().filter(|h| !h.is_empty())
}

/// 取 host 的父域：点数 ≥ 2 时剥去最左一段子域（如 `dify-api.crc.com.cn` → `crc.com.cn`），
/// 否则原样返回（`dify.com` 这类 2 段域名视为整体）。
///
/// 用于判定 Dify 的 API 域与文件域是否同站——同一业务常把 API 与文件分别放在不同子域
/// （如 `dify-api.crc.com.cn` 调接口、`dify-upload.crc.com.cn` 取文件）。
fn parent_domain(host: &str) -> &str {
    // 去掉端口（host:443 → host），避免一侧带端口导致父域比较误判
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let dots = host.bytes().filter(|&b| b == b'.').count();
    if dots >= 2 {
        host.split_once('.').map(|(_, rest)| rest).unwrap_or(host)
    } else {
        host
    }
}

/// 内置可信 Dify 域名白名单（多组，每组 = 一个 Dify 部署：文件预览域 + 接口域）。
/// 命中任一组即按 Dify 规则代理（[`dify_signed_url`] HMAC 签名）。
/// 新增业务线/部署：复制一组填上该部署的域名即可；不在白名单但与实例 `base_url`
/// 同父域的也会自动放行（兜底）。
const DIFY_FILE_HOST_WHITELIST: &[&[&str]] = &[
    // 部署 1：crc —— 接口 dify-api.crc.com.cn / 文件 dify-upload.crc.com.cn / dify.crc.com.cn
    &[
        "dify-upload.crc.com.cn",
        "dify-api.crc.com.cn",
        "dify.crc.com.cn",
    ],
    // 部署 2（示例占位，按实际补；文件域、接口域都列上）：
    // &["dify-upload.xxx.com", "dify-api.xxx.com"],
];

/// 从 `.../files/{file_id}/file-preview[?...]` 中提取 file_id（取 `/files/` 之后、
/// 下一个 `/` 之前的一段）。无法解析返回 None（非 Dify 文件预览 URL）。
fn dify_file_id(url: &str) -> Option<String> {
    let after = url.split("/files/").nth(1)?;
    let id = after.split('/').next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// 为 Dify 文件预览 URL 生成 HMAC-SHA256 签名 URL。
///
/// Dify 的 `/files/{id}/file-preview` **不**走 dataset api_key 鉴权（带 key 也只返回
/// 0 字节——底层权限模型决定），而是用服务端 `SECRET_KEY` 对
/// `file-preview|{file_id}|{timestamp}|{nonce}` 做 HMAC-SHA256，再把
/// `timestamp`/`nonce`/`sign` 作为 query 附上。签名 URL 直接 GET 即可（无需
/// Authorization 头），有效期由 Dify 端 `FILES_ACCESS_TIMEOUT` 控制。
///
/// 返回重建后的规范 URL `{scheme}://{host}/files/{file_id}/file-preview?...`，忽略原
/// URL 的 query。返回 None 表示无法解析 file_id。
fn dify_signed_url(url: &str, secret_key: &str) -> Option<String> {
    let file_id = dify_file_id(url)?;
    let host = url_host(url)?;
    let scheme = if url.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let msg = format!("file-preview|{file_id}|{ts}|{nonce}");

    use base64::Engine;
    // hmac 0.13：new_from_slice 在 KeyInit trait 上（经 hmac 再导出），需显式引入
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret_key.as_bytes()).ok()?;
    mac.update(msg.as_bytes());
    // 带填充的 urlsafe base64（与 Dify 实测签名一致：32 字节 → 44 字符，末尾含 =）
    let sign = base64::engine::general_purpose::URL_SAFE.encode(mac.finalize().into_bytes());

    Some(format!(
        "{scheme}://{host}/files/{file_id}/file-preview?timestamp={ts}&nonce={nonce}&sign={sign}"
    ))
}

/// 代理 Dify 知识库文档中的图片。
///
/// 背景：Dify 文档（由 Dify 解析 docx 等生成）里的图片是 Dify 文件域的 URL
/// （`https://<host>/files/<uuid>/file-preview`），浏览器直连返回 400。该接口**不**走
/// dataset api_key 鉴权（底层权限模型决定带 key 也只回 0 字节），而是用服务端
/// `SECRET_KEY` 做 HMAC-SHA256 签名（见 [`dify_signed_url`]）。这里按**当前会话绑定的
/// 实例 id** 解密该实例的 `SECRET_KEY`，签名后服务端拉取回传图片。
///
/// - `i` = 知识库实例 id（前端取当前会话助手绑定的 `kb_instance_id`）；
/// - `u` = 原始图片 URL（仅允许白名单域名或与该实例 Dify 同一父域的主机）。
///   命中后用该实例 `SECRET_KEY` 生成签名 URL 拉取；未配置 `SECRET_KEY` 则 302 回原
///   URL（图片无法显示，需在知识库录入页补填 SECRET_KEY）。
///
/// GET /api/kb/proxy-image?i=<instance_id>&u=<url>
pub(super) async fn handle_kb_proxy_image(
    State(state): State<Arc<AppState>>,
    auth::OptionalAuthUser(opt_user): auth::OptionalAuthUser,
    Query(params): Query<KbProxyImageParams>,
) -> axum::response::Response {
    use crate::domain::knowledge::backend::ProviderKind;
    use crate::domain::knowledge::backend::schema;

    // 与其他知识库读接口一致：未登录按匿名 "user"（私有实例仍受 require_readable 约束）
    let user_id = opt_user
        .as_ref()
        .map(|u| u.user_id.clone())
        .unwrap_or_else(|| "user".to_string());
    let is_admin = opt_user.as_ref().map(|u| u.is_admin).unwrap_or(false);

    let url = params.u.trim();
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return (StatusCode::BAD_REQUEST, "无效的图片 URL").into_response();
    }
    let Some(img_host) = url_host(url) else {
        return (StatusCode::BAD_REQUEST, "无效的图片 URL").into_response();
    };

    // 取实例（读权限校验）
    let inst =
        match knowledge_instances::require_readable(&state, &params.i, &user_id, is_admin).await {
            Ok(i) => i,
            Err(_) => return (StatusCode::NOT_FOUND, "知识库不可用").into_response(),
        };
    if ProviderKind::from_i16(inst.provider_kind) != Some(ProviderKind::Dify) {
        return (StatusCode::BAD_REQUEST, "该知识库类型不支持图片代理").into_response();
    }

    // 解密实例 config → base_url + secret_key（mask=false → secret_key 为明文）
    let cfg_plain = schema::decrypt_secret_fields(
        ProviderKind::Dify,
        &inst.config_value(),
        state.knowledge_manager.codec(),
        false,
    );
    let base_host = cfg_plain
        .get("base_url")
        .and_then(|v| v.as_str())
        .and_then(url_host);
    // 仅代理可信 Dify 文件域（白名单任一组 或 与 base_url 同父域）。
    let allowed = DIFY_FILE_HOST_WHITELIST
        .iter()
        .any(|group| group.contains(&img_host))
        || base_host.is_some_and(|bh| parent_domain(bh) == parent_domain(img_host));
    if !allowed {
        // 非可信域（如普通公网图片）不报错，直接 302 回原 URL，由浏览器自行加载。
        return axum::response::Redirect::to(url).into_response();
    }

    // 用该实例 SECRET_KEY 对 file-preview 做 HMAC 签名（签名 URL 自带鉴权，无需 Authorization）。
    // 未配置 SECRET_KEY → 无法签名，302 回原 URL 退化为图片不可用（需在录入页补填）。
    let Some(secret_key) = cfg_plain
        .get("secret_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        tracing::warn!(
            "[kb_proxy_image] 实例 {} 未配置 SECRET_KEY，文档图片无法签名",
            params.i
        );
        return axum::response::Redirect::to(url).into_response();
    };
    let Some(img_url) = dify_signed_url(url, secret_key) else {
        // 非标准 /files/{id}/file-preview 路径：直接回退原 URL
        return axum::response::Redirect::to(url).into_response();
    };

    let fetch = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client
            .get(&img_url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("上游返回 {status}"));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((ct, bytes))
    };
    match tokio::time::timeout(Duration::from_secs(25), fetch).await {
        Ok(Ok((ct, bytes))) => {
            if bytes.len() > 30 * 1024 * 1024 {
                return (StatusCode::PAYLOAD_TOO_LARGE, "图片过大").into_response();
            }
            // 放行图片与通用二进制流（部分 CDN 对图片回 octet-stream），
            // 仅拒绝 text/html 等明显非图片内容，避免被用来嵌套页面。
            if !ct.starts_with("image/") && ct != "application/octet-stream" {
                return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "非图片内容").into_response();
            }
            // 显式构建响应，避免对 header 数组 derive 的 trait 依赖
            let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_str(&ct).unwrap_or_else(|_| {
                    axum::http::HeaderValue::from_static("application/octet-stream")
                }),
            );
            resp
        }
        Ok(Err(e)) => {
            tracing::warn!("[kb_proxy_image] 拉取失败: url={} err={}", url, e);
            (StatusCode::BAD_GATEWAY, "图片获取失败").into_response()
        }
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "图片获取超时").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 知识库图片代理：API 域与文件域分属不同子域时，按父域判同站。
    #[test]
    fn kb_proxy_host_matching() {
        let base = url_host("https://dify-api.crc.com.cn/v1").unwrap();
        let img = url_host("https://dify-upload.crc.com.cn/files/abc/file-preview").unwrap();
        assert_eq!(base, "dify-api.crc.com.cn");
        assert_eq!(img, "dify-upload.crc.com.cn");
        assert_eq!(parent_domain(base), "crc.com.cn");
        assert_eq!(parent_domain(img), "crc.com.cn");

        assert_eq!(parent_domain("dify-upload.crc.com.cn"), "crc.com.cn");

        assert_ne!(
            parent_domain("dify-api.crc.com.cn"),
            parent_domain("cdn.example.com")
        );

        assert_eq!(parent_domain("dify-api.crc.com.cn:443"), "crc.com.cn");
        assert_eq!(parent_domain("dify.com"), "dify.com");
        assert_eq!(parent_domain("upload.dify.com"), "dify.com");
    }

    /// Dify 文件预览 URL 的 HMAC-SHA256 签名。
    #[test]
    fn kb_proxy_dify_signed_url() {
        let key = "test-secret-key";
        let original = "https://dify-upload.crc.com.cn/files/875fcbb0-1f3d-4bfe-ade4-818f7bbdefe9/file-preview";

        assert_eq!(
            dify_file_id(original),
            Some("875fcbb0-1f3d-4bfe-ade4-818f7bbdefe9".into())
        );
        assert_eq!(
            dify_file_id("https://x/files/abc/file-preview?x=1"),
            Some("abc".into())
        );

        let signed = dify_signed_url(original, key).unwrap();
        assert!(signed.starts_with(
            "https://dify-upload.crc.com.cn/files/875fcbb0-1f3d-4bfe-ade4-818f7bbdefe9/file-preview?timestamp="
        ));
        assert!(signed.contains("&nonce="));
        assert!(signed.contains("&sign="));
        let sign = signed.rsplit("sign=").next().unwrap();
        assert_eq!(sign.len(), 44);
        assert!(sign.ends_with('='));

        let s2 = dify_signed_url(original, key).unwrap();
        assert_ne!(signed, s2);

        assert!(dify_signed_url("https://x/foo/bar", key).is_none());
        assert!(dify_signed_url("not-a-url", key).is_none());

        assert!(
            url_host("https://dify-upload.crc.com.cn/files/abc/file-preview")
                .map(|h| DIFY_FILE_HOST_WHITELIST
                    .iter()
                    .any(|g| g.contains(&h)))
                .unwrap_or(false)
        );
    }
}
