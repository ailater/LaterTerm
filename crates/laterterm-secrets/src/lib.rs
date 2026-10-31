//! laterterm-secrets — 系统密钥环封装。
//!
//! 密钥（TOTP 种子、未来的 SSH 凭据）只存系统密钥环，绝不写进 settings.toml。
//! Linux 经 keyring v4 默认层走 Secret Service（gnome-keyring/kwallet）。
//!
//! 抽象出 [`SecretStore`] trait 是为了测试与未来可能的兜底实现（如加密文件）；
//! 生产路径固定用 [`KeyringStore`]，拿不到密钥环时如实报错，不做明文降级。

use anyhow::{Context, Result, anyhow};
use keyring::Entry;
use std::sync::Arc;

/// 密钥存取契约。`account` 是服务内的条目名（如 `totp/跳板机`）。
pub trait SecretStore: Send + Sync {
    /// 读取密钥；条目不存在返回 `Ok(None)`，其余错误向上抛。
    fn get(&self, account: &str) -> Result<Option<String>>;
    /// 写入或覆盖密钥。
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    /// 删除密钥；条目不存在视为成功（幂等）。
    fn delete(&self, account: &str) -> Result<()>;
}

/// 系统密钥环实现。service 固定为应用名，条目按 account 隔离。
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new() -> Self {
        Self {
            service: "laterterm".into(),
        }
    }

    fn entry(&self, account: &str) -> Result<Entry> {
        Entry::new(&self.service, account)
            .map_err(|error| anyhow!("无法访问系统密钥环（{error}）；请确认桌面密钥环服务已解锁"))
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>> {
        let entry = self.entry(account)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(anyhow!("读取密钥失败: {error}")),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        let entry = self.entry(account)?;
        entry
            .set_password(secret)
            .with_context(|| format!("写入密钥失败: {account}"))
    }

    fn delete(&self, account: &str) -> Result<()> {
        let entry = self.entry(account)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(anyhow!("删除密钥失败: {error}")),
        }
    }
}

/// TOTP 条目在密钥环中的账户名。名称里不允许 '/'，避免与命名空间混淆。
pub fn totp_account(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("名称不能为空");
    }
    if name.contains('/') || name.chars().any(char::is_control) {
        anyhow::bail!("名称不能包含 / 或控制字符");
    }
    Ok(format!("totp/{name}"))
}

/// 便捷构造：进程级共享的系统密钥环。
pub fn default_store() -> Arc<dyn SecretStore> {
    Arc::new(KeyringStore::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用的内存实现：也验证 trait 形状足够承载真实实现。
    struct MemoryStore {
        entries: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    }

    impl MemoryStore {
        fn new() -> Self {
            Self {
                entries: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            }
        }
    }

    impl SecretStore for MemoryStore {
        fn get(&self, account: &str) -> Result<Option<String>> {
            Ok(self.entries.lock().unwrap().get(account).cloned())
        }
        fn set(&self, account: &str, secret: &str) -> Result<()> {
            self.entries
                .lock()
                .unwrap()
                .insert(account.to_string(), secret.to_string());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<()> {
            self.entries.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryStore::new();
        assert_eq!(store.get("totp/a").unwrap(), None);
        store.set("totp/a", "JBSW").unwrap();
        assert_eq!(store.get("totp/a").unwrap().as_deref(), Some("JBSW"));
        store.delete("totp/a").unwrap();
        assert_eq!(store.get("totp/a").unwrap(), None);
        store.delete("totp/a").unwrap(); // 幂等
    }

    /// 本机冒烟：真实走一遍 Secret Service（gnome-keyring/kwallet）。
    /// CI 无桌面密钥环，标 `#[ignore]`，需要时 `cargo test -p laterterm-secrets -- --ignored`。
    #[test]
    #[ignore = "需要桌面会话中已解锁的 Secret Service"]
    fn keyring_roundtrip_against_live_service() {
        let store = KeyringStore::new();
        let account = totp_account("__laterterm_smoke__").unwrap();
        assert_eq!(store.get(&account).unwrap(), None);
        store.set(&account, "JBSWY3DPEHPK3PXP").unwrap();
        assert_eq!(
            store.get(&account).unwrap().as_deref(),
            Some("JBSWY3DPEHPK3PXP")
        );
        store.set(&account, "覆盖写入").unwrap();
        assert_eq!(store.get(&account).unwrap().as_deref(), Some("覆盖写入"));
        store.delete(&account).unwrap();
        assert_eq!(store.get(&account).unwrap(), None);
    }

    #[test]
    fn totp_account_validates_name() {
        assert_eq!(totp_account("跳板机").unwrap(), "totp/跳板机");
        assert_eq!(totp_account("  a  ").unwrap(), "totp/a");
        assert!(totp_account("").is_err());
        assert!(totp_account("a/b").is_err());
        assert!(totp_account("a\nb").is_err());
    }
}
