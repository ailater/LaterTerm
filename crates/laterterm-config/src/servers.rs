//! 远程服务器条目与持久化（M3 服务器面板的数据层）。
//!
//! 存储位置：`~/.config/laterterm/servers.toml`（与 settings/commands 同目录）。
//! 密码不落 TOML——`Keyring` 认证方式的密码存在系统密钥环
//! （laterterm-secrets，account 命名见 [`password_account`]），
//! 连接时后台线程取用。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 认证方式（持久化形态；连接时映射为 laterterm_ssh::SshAuth）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerAuthKind {
    /// ssh-agent（SSH_AUTH_SOCK）。
    Agent,
    /// 私钥文件路径。
    KeyFile(String),
    /// 系统密钥环存密码（account = [`password_account`]）。
    Keyring,
}

/// 一台远程服务器。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// 显示名（唯一标识，重名保存时后者覆盖前者）。
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub auth: ServerAuthKind,
    /// 终端编码（`"UTF-8"` / `"GBK"` / `"GB18030"`；缺省 UTF-8）。
    #[serde(default)]
    pub encoding: Option<String>,
}

fn default_port() -> u16 {
    22
}

/// 密码在系统密钥环里的 account 命名（与 TOTP 的命名同风格）。
pub fn password_account(name: &str) -> String {
    format!("laterterm-server-{name}")
}

/// 服务器列表（servers.toml 的读写单元）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStore {
    pub servers: Vec<ServerEntry>,
}

impl ServerStore {
    /// 从配置目录读（文件缺失/损坏回落空表，损坏不致命——面板可重建）。
    pub fn load() -> Self {
        let Some(path) = servers_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    /// 落盘（同名校验由调用方做，这里保序全量写）。
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = servers_path() else {
            return Err("无法定位配置目录（缺 HOME）".into());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }

    /// 按名取条目。
    pub fn get(&self, name: &str) -> Option<&ServerEntry> {
        self.servers.iter().find(|server| server.name == name)
    }

    /// 插入或按名覆盖（重命名语义 = 先 remove 再 upsert）。
    pub fn upsert(&mut self, entry: ServerEntry) {
        match self
            .servers
            .iter()
            .position(|server| server.name == entry.name)
        {
            Some(index) => self.servers[index] = entry,
            None => self.servers.push(entry),
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|server| server.name != name);
        before != self.servers.len()
    }
}

fn servers_path() -> Option<PathBuf> {
    crate::app_config_dir().map(|dir| dir.join("servers.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ServerEntry {
        ServerEntry {
            name: "跳板机".into(),
            host: "192.0.2.1".into(),
            port: 2222,
            username: "ops".into(),
            auth: ServerAuthKind::Keyring,
            encoding: Some("GBK".into()),
        }
    }

    #[test]
    fn toml_roundtrip_all_auth_kinds_and_defaults() {
        let mut store = ServerStore {
            servers: vec![
                sample(),
                ServerEntry {
                    name: "agent 机".into(),
                    host: "192.0.2.2".into(),
                    port: 22,
                    username: "root".into(),
                    auth: ServerAuthKind::Agent,
                    encoding: None,
                },
                ServerEntry {
                    name: "密钥机".into(),
                    host: "192.0.2.3".into(),
                    port: 22,
                    username: "root".into(),
                    auth: ServerAuthKind::KeyFile("/home/me/.ssh/id_ed25519".into()),
                    encoding: None,
                },
            ],
        };
        // 端口缺省 = 22（roundtrip 前先验序列化形态省略默认值后还能读回）
        let text = toml::to_string_pretty(&store).expect("序列化");
        assert!(text.contains("port = 2222"), "非默认端口必须显式: {text}");
        let back: ServerStore = toml::from_str(&text).expect("反序列化");
        assert_eq!(back, store);

        // 手写省略字段的最小 TOML 也能读（默认 port/encoding）
        let minimal = toml::from_str::<ServerStore>(
            "[[servers]]\nname = \"a\"\nhost = \"h\"\nusername = \"u\"\nauth = \"Agent\"\n",
        )
        .expect("最小形态");
        assert_eq!(minimal.servers[0].port, 22);
        assert_eq!(minimal.servers[0].encoding, None);
        store = minimal;
        assert_eq!(store.servers.len(), 1);
    }

    #[test]
    fn upsert_overwrites_by_name_and_remove_reports() {
        let mut store = ServerStore::default();
        store.upsert(sample());
        store.upsert(ServerEntry {
            name: "跳板机".into(),
            host: "203.0.113.9".into(),
            port: 22,
            username: "new".into(),
            auth: ServerAuthKind::Agent,
            encoding: None,
        });
        assert_eq!(store.servers.len(), 1, "同名覆盖不增条目");
        assert_eq!(store.servers[0].host, "203.0.113.9");

        assert!(store.remove("跳板机"));
        assert!(!store.remove("不存在"));
        assert!(store.servers.is_empty());
        assert_eq!(password_account("跳板机"), "laterterm-server-跳板机");
    }
}
