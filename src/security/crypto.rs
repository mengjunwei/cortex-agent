//! AES-256-GCM 加解密：用于模型供应商 API Key、MCP 凭据、KB config、助手 env_vars 的静态加密。
//!
//! - 密文格式：`base64( nonce(12B) || ciphertext+tag )`
//! - 解密时按前 12 字节切分出 nonce
//! - 密钥来源：内置 [`crate::security::APP_SECRETS`]（见 [`AesCodec::from_secrets`]）。
//!   支持多密钥轮换——加密用活动密钥（最后一项），解密遍历全部密钥（兼容历史密文）。
//!   [`AesCodec::from_passphrase`] 仅供测试使用。

use aes_gcm::aead::{Aead, Generate, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

/// AES-256-GCM 编解码器（支持多密钥轮换）。
///
/// `ciphers` 按密钥列表构造，**最后一项为活动密钥**（加密用），其余为历史密钥（仅解密）。
/// 至少含一个密钥。AES-256-GCM 无状态，同密钥的多个实例互通。
pub struct AesCodec {
    ciphers: Vec<Aes256Gcm>,
}

impl AesCodec {
    /// 从内置密钥列表 [`crate::security::APP_SECRETS`] 构造。
    ///
    /// 最后一项为活动密钥（加密/签发用），前面为历史密钥（仅解密/验证）。生产路径一律用它。
    pub fn from_secrets() -> Self {
        let ciphers = crate::security::APP_SECRETS
            .iter()
            .map(|raw| Aes256Gcm::new(&normalize_key(raw).into()))
            .collect::<Vec<_>>();
        assert!(
            !ciphers.is_empty(),
            "APP_SECRETS 不能为空：至少需要一个活动密钥"
        );
        Self { ciphers }
    }

    /// 单密钥构造（历史接口，仅供测试与迁移期使用）。
    pub fn from_passphrase(raw: &str) -> Self {
        Self {
            ciphers: vec![Aes256Gcm::new(&normalize_key(raw).into())],
        }
    }

    /// 加密明文，返回 base64 字符串（含 nonce）。始终用活动密钥（最后一项）。
    pub fn encrypt(&self, plaintext: &str) -> anyhow::Result<String> {
        let active = self.ciphers.last().expect("AesCodec 至少含一个密钥");
        // aead 0.6 起 generate_nonce 移到 Generate trait；rand_core 的 OsRng 未开 feature，
        // 用免 RNG 参数的 try_generate()（内部走 getrandom 系统熵源）
        let nonce =
            Nonce::try_generate().map_err(|e| anyhow::anyhow!("生成随机 nonce 失败: {e}"))?;
        let ciphertext = active
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("AES 加密失败: {}", e))?;
        let mut buf = nonce.to_vec();
        buf.extend_from_slice(&ciphertext);
        Ok(b64().encode(&buf))
    }

    /// 解密 base64 字符串（含 nonce）。**从活动密钥往前遍历**，首个成功返回；
    /// 全部失败则返回错误。支持用历史密钥解旧密文（轮换兼容）。
    pub fn decrypt(&self, encoded: &str) -> anyhow::Result<String> {
        let bytes = b64()
            .decode(encoded.trim())
            .map_err(|e| anyhow::anyhow!("Base64 解码失败: {}", e))?;
        if bytes.len() < 12 {
            anyhow::bail!("密文长度不足，无法解密");
        }
        let nonce: &aes_gcm::aead::Nonce<Aes256Gcm> = (&bytes[..12])
            .try_into()
            .map_err(|e| anyhow::anyhow!("Nonce 切片长度异常: {e}"))?;
        let ciphertext = &bytes[12..];
        // 从活动密钥（末尾）往前试：优先命中最新密钥，兼容历史密钥密文
        let mut last_err: Option<aes_gcm::aead::Error> = None;
        for cipher in self.ciphers.iter().rev() {
            match cipher.decrypt(nonce, ciphertext) {
                Ok(plain) => {
                    return String::from_utf8(plain)
                        .map_err(|e| anyhow::anyhow!("UTF-8 解码失败: {}", e));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(anyhow::anyhow!(
            "AES 解密失败（所有密钥均不匹配，密钥可能已变更）: {}",
            last_err.map(|_| "decrypt error").unwrap_or_default()
        ))
    }

    /// 仅用活动密钥（最后一项）尝试解密。供 re-encrypt 判断密文是否已是最新：
    /// 成功 = 已是活动密钥加密（可跳过重加密）；失败 = 旧密钥或损坏（需进一步处理）。
    pub fn decrypt_active(&self, encoded: &str) -> anyhow::Result<String> {
        let bytes = b64()
            .decode(encoded.trim())
            .map_err(|e| anyhow::anyhow!("Base64 解码失败: {}", e))?;
        if bytes.len() < 12 {
            anyhow::bail!("密文长度不足，无法解密");
        }
        let nonce: &aes_gcm::aead::Nonce<Aes256Gcm> = (&bytes[..12])
            .try_into()
            .map_err(|e| anyhow::anyhow!("Nonce 切片长度异常: {e}"))?;
        let ciphertext = &bytes[12..];
        let active = self.ciphers.last().expect("AesCodec 至少含一个密钥");
        let plain = active
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("活动密钥解密失败: {}", e))?;
        String::from_utf8(plain).map_err(|e| anyhow::anyhow!("UTF-8 解码失败: {}", e))
    }

    /// 是否仅含一个密钥（无历史密钥）。re-encrypt 仅在多密钥时需生效。
    pub fn is_single_key(&self) -> bool {
        self.ciphers.len() == 1
    }
}

#[cfg(test)]
impl AesCodec {
    /// 测试专用：构造 `[legacy, active]` 双密钥 codec，模拟密钥轮换场景
    /// （`active` 为末项=活动密钥，`legacy` 仅解密）。供 reencrypt 单测验证历史密文 re-wrap。
    pub(crate) fn for_rotation_test(legacy: &str, active: &str) -> Self {
        Self {
            ciphers: vec![
                Aes256Gcm::new(&normalize_key(legacy).into()),
                Aes256Gcm::new(&normalize_key(active).into()),
            ],
        }
    }
}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// 将任意输入归一化为 32 字节 AES-256 密钥。
///
/// 注意：传入空串时返回**全零密钥**（确定值，非随机）。生产环境密钥已内置
/// [`crate::security::APP_SECRETS`]，不会走空串路径；此分支仅为防御性兜底。
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

    // 2) 回退：UTF-8 字节补齐/截断到 32 字节（空串 → 全零）
    let mut key = [0u8; 32];
    let bytes = trimmed.as_bytes();
    let len = bytes.len().min(32);
    key[..len].copy_from_slice(&bytes[..len]);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE_A: &str = "2rpWkdT1L1WqRs5DHBqjJmv7JQkJqQeoM+QRyFhtYpI=";
    const PHRASE_B: &str = "another-32-byte-secret-key-for-test!"; // 不同密钥

    #[test]
    fn encrypt_decrypt_roundtrip_single_key() {
        let codec = AesCodec::from_passphrase(PHRASE_A);
        let enc = codec.encrypt("hello 世界").unwrap();
        assert_ne!(enc, "hello 世界");
        assert_eq!(codec.decrypt(&enc).unwrap(), "hello 世界");
    }

    #[test]
    fn from_secrets_uses_builtin_active_key() {
        let codec = AesCodec::from_secrets();
        let enc = codec.encrypt("secret").unwrap();
        assert_eq!(codec.decrypt_active(&enc).unwrap(), "secret");
        assert_eq!(codec.decrypt(&enc).unwrap(), "secret");
    }

    #[test]
    fn multi_key_decrypts_legacy_ciphertext() {
        // 旧密钥加密的密文
        let legacy = AesCodec::from_passphrase(PHRASE_A);
        let enc = legacy.encrypt("legacy-data").unwrap();

        // 多密钥 codec：[旧, 新]，活动 = 新
        let rot = AesCodec {
            ciphers: vec![
                Aes256Gcm::new(&normalize_key(PHRASE_A).into()),
                Aes256Gcm::new(&normalize_key(PHRASE_B).into()),
            ],
        };
        // decrypt_active（新密钥）应失败（旧密文）
        assert!(rot.decrypt_active(&enc).is_err());
        // decrypt（遍历）应成功（命中旧密钥）
        assert_eq!(rot.decrypt(&enc).unwrap(), "legacy-data");
    }

    #[test]
    fn multi_key_encrypt_uses_active_only() {
        let rot = AesCodec {
            ciphers: vec![
                Aes256Gcm::new(&normalize_key(PHRASE_A).into()),
                Aes256Gcm::new(&normalize_key(PHRASE_B).into()),
            ],
        };
        let enc = rot.encrypt("new-data").unwrap();
        // 活动密钥（新）能解
        assert_eq!(rot.decrypt_active(&enc).unwrap(), "new-data");
        // 仅旧密钥的 codec 解不开（证明加密用的是新密钥）
        let legacy_only = AesCodec::from_passphrase(PHRASE_A);
        assert!(legacy_only.decrypt(&enc).is_err());
    }

    #[test]
    fn decrypt_garbage_returns_err() {
        let codec = AesCodec::from_secrets();
        assert!(codec.decrypt("not-valid-base64-!!!").is_err());
        assert!(codec.decrypt("dG9v").is_err()); // 解码后不足 12 字节
    }

    #[test]
    fn is_single_key_reflects_count() {
        assert!(AesCodec::from_passphrase(PHRASE_A).is_single_key());
        // from_secrets 当前内置 1 个密钥 → 单密钥
        assert!(AesCodec::from_secrets().is_single_key());
    }
}
