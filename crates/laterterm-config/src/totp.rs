//! TOTP 动态码（RFC 6238）：纯算法与解析，不依赖运行环境。
//!
//! 密钥本身由 laterterm-secrets 存入系统密钥环，本模块只处理：
//! - base32 解码（RFC 4648，容忍小写、空格与缺省填充）；
//! - HMAC-SHA1/256/512 计数器摘要 + RFC 4226 动态截断；
//! - `otpauth://totp/...` 链接解析（各类验证器导出的通用格式）。
//!
//! 密码学原语不自造：HMAC/SHA 走 RustCrypto（hmac、sha1、sha2）。

use anyhow::{Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Sha256, Sha512};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_SECRET_LEN: usize = 512;
const MAX_NAME_LEN: usize = 80;

/// TOTP 摘要算法。绝大多数服务用 SHA1；SHA256/512 供少数严格服务。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TotpAlgorithm {
    #[default]
    Sha1,
    Sha256,
    Sha512,
}

impl TotpAlgorithm {
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "SHA1" | "SHA-1" => Ok(Self::Sha1),
            "SHA256" | "SHA-256" => Ok(Self::Sha256),
            "SHA512" | "SHA-512" => Ok(Self::Sha512),
            other => bail!("不支持的 TOTP 算法: {other}"),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
            Self::Sha512 => "SHA512",
        }
    }
}

/// TOTP 参数（算法 / 位数 / 周期）。默认即业界事实标准：SHA1 · 6 位 · 30 秒。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TotpConfig {
    pub algorithm: TotpAlgorithm,
    pub digits: u32,
    pub period: u64,
}

impl Default for TotpConfig {
    fn default() -> Self {
        Self {
            algorithm: TotpAlgorithm::Sha1,
            digits: 6,
            period: 30,
        }
    }
}

impl TotpConfig {
    /// 校验参数范围，防止坏配置产生空码或疯狂刷新。
    pub fn validate(&self) -> Result<()> {
        if !(6..=8).contains(&self.digits) {
            bail!("动态码位数只支持 6 或 8");
        }
        if !(1..=3600).contains(&self.period) {
            bail!("动态码周期需在 1~3600 秒之间");
        }
        Ok(())
    }
}

/// 一次生成结果：验证码 + 距过期剩余时间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TotpCode {
    pub code: String,
    pub valid_for: Duration,
}

/// 设置侧的 TOTP 条目元数据；密钥不入 TOML，只存系统密钥环。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TotpEntry {
    pub name: String,
    pub algorithm: TotpAlgorithm,
    pub digits: u32,
    pub period: u64,
}

impl Default for TotpEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            algorithm: TotpAlgorithm::Sha1,
            digits: 6,
            period: 30,
        }
    }
}

impl TotpEntry {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        TotpConfig {
            algorithm: self.algorithm,
            digits: self.digits,
            period: self.period,
        }
        .validate()
    }
}

/// 解析后的密钥材料：归一化 base32 + 参数。只在本模块内部流转，不落盘。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSecret {
    /// 归一化后的大写 base32（无空白、无填充）。
    pub secret: String,
    pub label: Option<String>,
    pub config: TotpConfig,
}

/// 解析用户输入：既接受裸 base32 密钥（用默认参数），也接受 otpauth:// 链接。
pub fn parse_secret_input(input: &str) -> Result<ParsedSecret> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("密钥不能为空");
    }
    if trimmed.len() > MAX_SECRET_LEN * 2 {
        bail!("密钥过长");
    }
    if trimmed.to_ascii_lowercase().starts_with("otpauth://") {
        parse_otpauth(trimmed)
    } else {
        Ok(ParsedSecret {
            secret: normalize_base32(trimmed)?,
            label: None,
            config: TotpConfig::default(),
        })
    }
}

/// 归一化并校验 base32（RFC 4648 字母表 A-Z2-7）：大写化、去空白/连字符、去填充。
pub fn normalize_base32(input: &str) -> Result<String> {
    let cleaned: String = input
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-')
        .map(|ch| ch.to_ascii_uppercase())
        .take_while(|ch| *ch != '=')
        .collect();
    if cleaned.is_empty() {
        bail!("密钥不能为空");
    }
    if cleaned.len() > MAX_SECRET_LEN {
        bail!("密钥过长");
    }
    if !cleaned
        .chars()
        .all(|ch| ch.is_ascii_uppercase() || ('2'..='7').contains(&ch))
    {
        bail!("密钥含 base32 非法字符（应为 A-Z 与 2-7）");
    }
    Ok(cleaned)
}

fn decode_base32(normalized: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut output = Vec::with_capacity(normalized.len() * 5 / 8);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in normalized.bytes() {
        let value = ALPHABET.iter().position(|&c| c == ch).unwrap_or(0) as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
        }
    }
    output
}

/// 生成指定时刻的动态码（RFC 6238：counter = unix 时间 / 周期）。
pub fn generate_code(secret: &str, config: &TotpConfig, at: SystemTime) -> Result<TotpCode> {
    config.validate()?;
    let normalized = normalize_base32(secret)?;
    let elapsed = at
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("系统时间早于 Unix 纪元，无法生成动态码"))?;
    let counter = elapsed.as_secs() / config.period;
    let valid_for = Duration::from_secs(config.period - elapsed.as_secs() % config.period);
    let digest = hmac_digest(&decode_base32(&normalized), counter, config.algorithm);
    let code = dynamic_truncate(&digest, config.digits);
    Ok(TotpCode { code, valid_for })
}

fn hmac_digest(secret: &[u8], counter: u64, algorithm: TotpAlgorithm) -> Vec<u8> {
    // new_from_slice 对任意长度密钥都不会失败（hmac 内部按块处理密钥）。
    match algorithm {
        TotpAlgorithm::Sha1 => {
            let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC 密钥长度不受限制");
            mac.update(&counter.to_be_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        TotpAlgorithm::Sha256 => {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC 密钥长度不受限制");
            mac.update(&counter.to_be_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        TotpAlgorithm::Sha512 => {
            let mut mac = Hmac::<Sha512>::new_from_slice(secret).expect("HMAC 密钥长度不受限制");
            mac.update(&counter.to_be_bytes());
            mac.finalize().into_bytes().to_vec()
        }
    }
}

/// RFC 4226 动态截断：取摘要末字节低 4 位为偏移，读 4 字节并去掉最高位。
fn dynamic_truncate(digest: &[u8], digits: u32) -> String {
    let offset = (*digest.last().expect("HMAC 摘要非空") & 0x0f) as usize;
    let binary = ((digest[offset] & 0x7f) as u32) << 24
        | (digest[offset + 1] as u32) << 16
        | (digest[offset + 2] as u32) << 8
        | digest[offset + 3] as u32;
    let modulus = 10u32.pow(digits);
    format!("{:0width$}", binary % modulus, width = digits as usize)
}

/// 解析 `otpauth://totp/<label>?secret=..&algorithm=..&digits=..&period=..`。
/// 未知参数忽略；algorithm/digits/period 缺省时取默认配置。
fn parse_otpauth(uri: &str) -> Result<ParsedSecret> {
    let rest = uri.strip_prefix("otpauth://").expect("调用方已确认前缀");
    let (type_path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    let type_path = type_path.trim_end_matches('/');
    let Some(issuer_type) = type_path.split('/').next() else {
        bail!("otpauth 链接缺少类型段");
    };
    if !issuer_type.eq_ignore_ascii_case("totp") {
        bail!("只支持 totp 类型的 otpauth 链接（收到 {issuer_type}）");
    }
    let label = type_path
        .split_once('/')
        .map(|(_, label)| percent_decode(label));

    let mut config = TotpConfig::default();
    if let Some(query) = query {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "secret" => {}
                "algorithm" => config.algorithm = TotpAlgorithm::parse(value)?,
                "digits" => {
                    config.digits = value
                        .trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("otpauth digits 参数非法: {value}"))?;
                }
                "period" => {
                    config.period = value
                        .trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("otpauth period 参数非法: {value}"))?;
                }
                _ => {}
            }
        }
    }
    let secret = query
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                key.trim()
                    .eq_ignore_ascii_case("secret")
                    .then(|| value.to_string())
            })
        })
        .ok_or_else(|| anyhow::anyhow!("otpauth 链接缺少 secret 参数"))?;
    config.validate()?;
    Ok(ParsedSecret {
        secret: normalize_base32(&percent_decode(&secret))?,
        label,
        config,
    })
}

/// 最小百分号解码（otpauth label/secret 场景）；非法序列原样保留。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(value) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            )
            .ok()
        {
            output.push(value);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// 条目名与命令模板同名规则：非空、无控制字符、长度受限。
pub fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("名称不能为空");
    }
    if name.chars().count() > MAX_NAME_LEN {
        bail!("名称过长");
    }
    if name.contains('\0') || name.contains('\r') || name.contains('\n') {
        bail!("名称不能包含 NUL 或换行");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn base32_decodes_rfc4648_vectors() {
        // RFC 4648 测试向量（去填充、容忍小写与空格）。
        let cases = [
            ("MY======", "f"),
            ("mz xq====", "fo"),
            ("MZXW6===", "foo"),
            ("MZXW6YQ=", "foob"),
            ("MZXW6YTB", "fooba"),
            ("MZXW6YTBOI======", "foobar"),
        ];
        for (input, expected) in cases {
            let normalized = normalize_base32(input).unwrap();
            let decoded = decode_base32(&normalized);
            assert_eq!(decoded, expected.as_bytes(), "输入: {input}");
        }
        assert!(normalize_base32("ABC1").is_err());
        assert!(normalize_base32("AB!C").is_err());
    }

    #[test]
    fn rfc6238_test_vectors() {
        // RFC 6238 附录 B 标准向量：密钥为 ASCII "1234567890" 重复到各算法要求长度。
        // 20 字节 "12345678901234567890" 的 base32：
        let sha1_key = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        // 32 字节 "12345678901234567890123456789012" 的 base32：
        let sha256_key = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZA====";
        // 64 字节 "1234567890"×6+"1234" 的 base32（RFC 6238 SHA512 用例）：
        let sha512_key = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNA=";

        let cases = [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ];
        for (time, expected) in cases {
            let code = generate_code(
                sha1_key,
                &TotpConfig {
                    algorithm: TotpAlgorithm::Sha1,
                    digits: 8,
                    period: 30,
                },
                at(time),
            )
            .unwrap();
            assert_eq!(code.code, expected, "SHA1 T={time}");
        }

        let cases = [
            (59u64, "46119246"),
            (1111111109, "68084774"),
            (1111111111, "67062674"),
            (1234567890, "91819424"),
            (2000000000, "90698825"),
            (20000000000, "77737706"),
        ];
        for (time, expected) in cases {
            let code = generate_code(
                sha256_key,
                &TotpConfig {
                    algorithm: TotpAlgorithm::Sha256,
                    digits: 8,
                    period: 30,
                },
                at(time),
            )
            .unwrap();
            assert_eq!(code.code, expected, "SHA256 T={time}");
        }

        let cases = [
            (59u64, "90693936"),
            (1111111109, "25091201"),
            (1111111111, "99943326"),
            (1234567890, "93441116"),
            (2000000000, "38618901"),
            (20000000000, "47863826"),
        ];
        for (time, expected) in cases {
            let code = generate_code(
                sha512_key,
                &TotpConfig {
                    algorithm: TotpAlgorithm::Sha512,
                    digits: 8,
                    period: 30,
                },
                at(time),
            )
            .unwrap();
            assert_eq!(code.code, expected, "SHA512 T={time}");
        }
    }

    #[test]
    fn six_digit_code_is_prefix_of_eight_digit() {
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        let six = generate_code(
            secret,
            &TotpConfig {
                digits: 6,
                ..TotpConfig::default()
            },
            at(59),
        )
        .unwrap();
        assert_eq!(six.code, "287082");
        assert_eq!(six.valid_for, Duration::from_secs(30 - 29));
    }

    #[test]
    fn parses_otpauth_uri_with_params() {
        let parsed = parse_secret_input(
            "otpauth://totp/Example:alice@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Example&algorithm=SHA256&digits=8&period=60",
        )
        .unwrap();
        assert_eq!(parsed.secret, "JBSWY3DPEHPK3PXP");
        assert_eq!(parsed.label.as_deref(), Some("Example:alice@example.com"));
        assert_eq!(
            parsed.config,
            TotpConfig {
                algorithm: TotpAlgorithm::Sha256,
                digits: 8,
                period: 60,
            }
        );
    }

    #[test]
    fn otpauth_defaults_and_errors() {
        let parsed = parse_secret_input("otpauth://totp/jump?secret=jbswy3dpehpk3pxp").unwrap();
        assert_eq!(parsed.config, TotpConfig::default());
        assert!(parse_secret_input("otpauth://hotp/x?secret=JBSWY3DPEHPK3PXP").is_err());
        assert!(
            parse_secret_input("otpauth://totp/x?algorithm=SHA3&secret=JBSWY3DPEHPK3PXP").is_err()
        );
        assert!(parse_secret_input("otpauth://totp/x?digits=5&secret=JBSWY3DPEHPK3PXP").is_err());
        assert!(parse_secret_input("otpauth://totp/x?digits=9").is_err());
    }

    #[test]
    fn rejects_empty_and_malformed_secrets() {
        assert!(parse_secret_input("   ").is_err());
        assert!(parse_secret_input("ABC1").is_err());
    }

    #[test]
    fn percent_decode_handles_plain_and_encoded() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn entry_validation_and_settings_crud() {
        let mut settings = crate::Settings::default();
        let entry = TotpEntry {
            name: "跳板机".into(),
            ..Default::default()
        };
        entry.validate().unwrap();
        settings.upsert_totp(entry.clone()).unwrap();
        settings
            .upsert_totp(TotpEntry {
                digits: 8,
                ..entry.clone()
            })
            .unwrap();
        assert_eq!(settings.totp.len(), 1);
        assert_eq!(settings.totp[0].digits, 8);
        assert!(settings.remove_totp("跳板机"));
        assert!(!settings.remove_totp("跳板机"));

        assert!(
            TotpEntry {
                name: "  ".into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            TotpEntry {
                period: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn totp_roundtrips_through_settings_file() {
        let mut settings = crate::Settings::default();
        settings
            .upsert_totp(TotpEntry {
                name: "jump".into(),
                algorithm: TotpAlgorithm::Sha256,
                digits: 8,
                period: 60,
            })
            .unwrap();
        let raw = toml::to_string(&settings).unwrap();
        assert!(!raw.contains("secret"), "密钥不允许进 TOML");
        let parsed: crate::Settings = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.totp, settings.totp);
        // 旧配置文件没有 totp 段也能读出空列表。
        let legacy: crate::Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.totp.is_empty());
    }
}
