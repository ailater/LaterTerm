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
    namespaced_account("totp", name)
}

/// SSH 服务器密码 / keyboard-interactive 口令在密钥环中的账户名。
/// 旧版本私钥 passphrase 同用此条目，仅为兼容保留；新口令使用 [`ssh_passphrase_account`]。
pub fn ssh_account(name: &str) -> Result<String> {
    namespaced_account("ssh", name)
}

/// SSH 私钥口令的独立账户名，与同名服务器的密码条目隔离。
pub fn ssh_passphrase_account(name: &str) -> Result<String> {
    namespaced_account("ssh-passphrase", name)
}

/// WebDAV 配置同步密码的密钥环账户名。v1 单条目：url/username 是明文同步
/// 元数据（settings.toml [sync] 段），密码本体只存密钥环，绝不落盘。
pub fn webdav_account() -> Result<String> {
    namespaced_account("webdav", "password")
}

fn namespaced_account(namespace: &str, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("名称不能为空");
    }
    if name.contains('/') || name.chars().any(char::is_control) {
        anyhow::bail!("名称不能包含 / 或控制字符");
    }
    Ok(format!("{namespace}/{name}"))
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

    #[test]
    fn ssh_passphrase_account_namespaces_and_validates_like_ssh() {
        assert_eq!(
            ssh_passphrase_account("跳板机").unwrap(),
            "ssh-passphrase/跳板机"
        );
        assert_eq!(ssh_passphrase_account(" a ").unwrap(), "ssh-passphrase/a");
        assert_eq!(
            ssh_passphrase_account("  跳 板机  ").unwrap(),
            "ssh-passphrase/跳 板机"
        );
        for name in ["", "   ", "a/b", "a\nb", "a\tb", "a\0b"] {
            assert!(ssh_passphrase_account(name).is_err(), "{name:?}");
            assert!(ssh_account(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn ssh_passphrase_namespace_does_not_overwrite_or_delete_password() {
        let store = MemoryStore::new();
        let password = ssh_account("跳板机").unwrap();
        let passphrase = ssh_passphrase_account("跳板机").unwrap();
        assert_ne!(password, passphrase);
        assert_ne!(passphrase, totp_account("跳板机").unwrap());
        store.set(&password, "登录密码").unwrap();
        store.set(&passphrase, "私钥口令").unwrap();
        assert_eq!(store.get(&password).unwrap().as_deref(), Some("登录密码"));
        assert_eq!(store.get(&passphrase).unwrap().as_deref(), Some("私钥口令"));
        store.delete(&passphrase).unwrap();
        assert_eq!(store.get(&passphrase).unwrap(), None);
        assert_eq!(store.get(&password).unwrap().as_deref(), Some("登录密码"));
    }

    #[test]
    fn ssh_credentials_preserve_secret_whitespace() {
        let store = MemoryStore::new();
        for account in [
            ssh_account("跳板机").unwrap(),
            ssh_passphrase_account("跳板机").unwrap(),
        ] {
            for secret in ["  密码与口令  ", "   ", "\t口令\n"] {
                store.set(&account, secret).unwrap();
                assert_eq!(store.get(&account).unwrap().as_deref(), Some(secret));
            }
        }
    }

    #[test]
    fn ssh_account_namespaces_and_validates_like_totp() {
        assert_eq!(ssh_account("跳板机").unwrap(), "ssh/跳板机");
        assert_eq!(ssh_account(" a ").unwrap(), "ssh/a");
        assert!(ssh_account("").is_err());
        assert!(ssh_account("a/b").is_err());
        assert!(ssh_account("a\nb").is_err());
    }

    /// WebDAV 同步密码走独立命名空间：与 ssh/totp 同名条目互不覆盖、互不误删
    /// （ssh_passphrase 命名空间隔离同款口径）。
    #[test]
    fn webdav_account_namespaces_and_does_not_collide_with_other_namespaces() {
        assert_eq!(webdav_account().unwrap(), "webdav/password");
        assert_ne!(webdav_account().unwrap(), ssh_account("password").unwrap());
        assert_ne!(
            webdav_account().unwrap(),
            ssh_passphrase_account("password").unwrap()
        );
        assert_ne!(webdav_account().unwrap(), totp_account("password").unwrap());

        let store = MemoryStore::new();
        store.set(&webdav_account().unwrap(), "webdav密码").unwrap();
        store
            .set(&ssh_account("password").unwrap(), "ssh密码")
            .unwrap();
        assert_eq!(
            store.get(&webdav_account().unwrap()).unwrap().as_deref(),
            Some("webdav密码")
        );
        store.delete(&ssh_account("password").unwrap()).unwrap();
        // 删 ssh 条目不得波及 webdav 命名空间
        assert_eq!(
            store.get(&webdav_account().unwrap()).unwrap().as_deref(),
            Some("webdav密码")
        );
    }
}
