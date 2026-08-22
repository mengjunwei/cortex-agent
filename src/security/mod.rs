//! 应用密钥管理 —— AES 静态加密与 JWT 签名的统一密钥来源。
//!
//! 密钥内置代码（不再从配置文件读取），AES 与 JWT 共享同一密钥列表，支持轮换：
//! - [`APP_SECRETS`] 最后一项为「活动密钥」，用于加密新数据 / 签发新 token；
//! - 前面各项为「历史密钥」，仅用于解密历史密文 / 验证旧 token；
//! - 进程启动时 [`reencrypt::reencrypt_all`] 会把所有历史密文 re-wrap 到活动密钥
//!   （幂等，仅在密钥列表多于一项时实质生效）。
//!
//! ⚠️ **安全前提**：本仓库必须保持 PRIVATE。密钥内置源码后，仓库一旦公开，
//! 所有加密数据（模型 API Key、MCP 凭据、KB config、助手 env_vars）与 JWT 签名
//! 均等于泄露。轮换密钥时在 [`APP_SECRETS`] 末尾追加新值、重启即可。

pub mod crypto;
pub mod reencrypt;

/// 应用主密钥列表（AES-256-GCM 加密 + JWT HS256 签名共用）。
///
/// - **最后一项 = 活动密钥**：加密新数据、签发新 token 一律用它；
/// - **前面各项 = 历史密钥**：仅用于解密/验证轮换前产生的数据；
/// - **轮换**：在末尾追加一个全新密钥，重启后 boot 自动把全部历史密文 re-wrap 到新密钥。
///
/// 每项可以是 base64 编码的 32 字节（推荐，见 `security::crypto` 的密钥归一化），
/// 或任意长度口令（自动补齐/截断到 32 字节）。JWT 侧按 UTF-8 字节用作 HS256 secret
/// （要求 ≥32 字节；当前值 base64 解码恰好 32 字节，UTF-8 长度 44 字节，两侧均满足）。
///
/// ⚠️ 仓库必须 PRIVATE。
pub const APP_SECRETS: &[&str] = &[
    "2rpWkdT1L1WqRs5DHBqjJmv7JQkJqQeoM+QRyFhtYpI=", // v1（当前活动）
];

/// 当前活动密钥（[`APP_SECRETS`] 最后一项）。加密新数据 / 签发新 token 一律用它。
#[track_caller]
pub fn active_secret() -> &'static str {
    APP_SECRETS
        .last()
        .expect("APP_SECRETS 不能为空：至少需要配置一个活动密钥")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_non_empty_and_active_is_last() {
        assert!(!APP_SECRETS.is_empty());
        assert_eq!(active_secret(), APP_SECRETS[APP_SECRETS.len() - 1]);
    }

    #[test]
    fn active_secret_stable() {
        // 活动密钥应为固定值（防止误改）
        assert_eq!(
            active_secret(),
            "2rpWkdT1L1WqRs5DHBqjJmv7JQkJqQeoM+QRyFhtYpI="
        );
    }
}
