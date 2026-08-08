//! AES-256-GCM 加解密：用于模型供应商 API Key 的静态加密存储
//!
//! - 密文格式：`base64( nonce(12B) || ciphertext+tag )`
//! - 解密时按前 12 字节切分出 nonce
//! - 密钥来源：`config.toml [security].aes_key` 或环境变量 `MODEL_AES_KEY`

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

/// AES-256-GCM 编解码器
pub struct AesCodec {
    cipher: Aes256Gcm,
}

impl AesCodec {
    /// 从配置口令构造。优先按 base64 解码 32 字节；否则按 UTF-8 字节补齐/截断到 32 字节。
    /// 留空时随机生成临时密钥（仅供首次体验，重启后无法解密历史数据）。
    pub fn from_passphrase(raw: &str) -> Self {
        let key = normalize_key(raw);
        let cipher = Aes256Gcm::new(&key.into());
        Self { cipher }
    }

    /// 加密明文，返回 base64 字符串（含 nonce）
    pub fn encrypt(&self, plaintext: &str) -> anyhow::Result<String> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("AES 加密失败: {}", e))?;
        let mut buf = nonce.to_vec();
        buf.extend_from_slice(&ciphertext);
        Ok(b64().encode(&buf))
    }

    /// 解密 base64 字符串（含 nonce），返回明文
    pub fn decrypt(&self, encoded: &str) -> anyhow::Result<String> {
        let bytes = b64()
            .decode(encoded.trim())
            .map_err(|e| anyhow::anyhow!("Base64 解码失败: {}", e))?;
        if bytes.len() < 12 {
            anyhow::bail!("密文长度不足，无法解密");
        }
        let nonce = Nonce::from_slice(&bytes[..12]);
        let ciphertext = &bytes[12..];
        let plaintext = self
            .cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("AES 解密失败（密钥可能已变更）: {}", e))?;
        String::from_utf8(plaintext).map_err(|e| anyhow::anyhow!("UTF-8 解码失败: {}", e))
    }
}

/// 从 `[security].aes_key`（或环境变量 `MODEL_AES_KEY`）构造 [`AesCodec`]。
///
/// 密钥为空时随机生成临时密钥并打 warn（`tag` 标识调用方，便于日志定位）。
/// 统一 model_provider / mcp 等多处的 AES 初始化样板。
pub fn codec_from_security(security: &crate::config::SecurityConfig, tag: &str) -> AesCodec {
    let aes_raw = std::env::var("MODEL_AES_KEY").unwrap_or_else(|_| security.aes_key.clone());
    if aes_raw.trim().is_empty() {
        tracing::warn!(
            "[{tag}] 未配置 [security].aes_key，已生成临时 AES 密钥。\
             重启后历史加密数据将无法解密，生产环境请务必固定密钥。"
        );
    }
    AesCodec::from_passphrase(&aes_raw)
}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// 将任意输入归一化为 32 字节 AES-256 密钥
fn normalize_key(raw: &str) -> [u8; 32] {
    let trimmed = raw.trim();

    // 1) 尝试 base64 解码到 32 字节
    if !trimmed.is_empty() {
        if let Ok(decoded) = b64().decode(trimmed) {
            if decoded.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&decoded);
                return k;
            }
        }
    }

    // 2) 回退：UTF-8 字节补齐/截断到 32 字节
    let mut key = [0u8; 32];
    let bytes = trimmed.as_bytes();
    let len = bytes.len().min(32);
    key[..len].copy_from_slice(&bytes[..len]);
    key
}
