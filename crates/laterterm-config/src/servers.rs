//! 服务器连接配置：只存连接元数据，凭据（密码/口令/passphrase）一律入系统
//! 密钥环（laterterm-secrets），本模块与 settings.toml 里都不出现密钥本体。

use anyhow::{Result, bail, Context as _};
use crate::commands::normalize_shortcut;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 认证方式元数据；凭据本体按 `laterterm_secrets::ssh_account(name)` 取自密钥环。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerAuthKind {
    /// 密码认证（密码在密钥环）。
    #[default]
    Password,
    /// keyboard-interactive 认证（口令/动态码在密钥环，跳板机二次验证常见）。
    KeyboardInteractive,
    /// 私钥文件认证（passphrase 可选，在密钥环）。
    PrivateKey,
    /// SSH agent 认证。
    Agent,
}

/// 一台可连接的服务器。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerEntry {
    /// 展示名，同时是密钥环条目与列表的唯一键。
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: ServerAuthKind,
    /// `auth = private-key` 时的私钥文件路径。
    pub key_path: Option<String>,
    /// 备注（纯展示）。
    pub note: String,
    /// 应用内快捷键（ctrl+alt+k 一类），可选；统一注册表查重。
    #[serde(default)]
    pub shortcut: Option<String>,
    /// 会话编码；缺省 UTF-8。
    pub encoding: Option<String>,
}

impl Default for ServerEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            auth: ServerAuthKind::Password,
            key_path: None,
            note: String::new(),
            shortcut: None,
            encoding: None,
        }
    }
}

impl ServerEntry {
    /// UI 保存前校验；与密钥环账户名规则（无 `/`、无控制字符）保持一致。
    pub fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            bail!("服务器名称不能为空");
        }
        if name.chars().count() > 80 {
            bail!("服务器名称过长");
        }
        if name.contains('/') || name.chars().any(char::is_control) {
            bail!("服务器名称不能包含 / 或控制字符");
        }
        let host = self.host.trim();
        if host.is_empty() {
            bail!("主机地址不能为空");
        }
        if host.len() > 253 {
            bail!("主机地址过长");
        }
        if host.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            bail!("主机地址不能包含空白或控制字符");
        }
        if self.port == 0 {
            bail!("端口不能为 0");
        }
        let username = self.username.trim();
        if username.is_empty() {
            bail!("登录用户名不能为空");
        }
        if username.len() > 64 || username.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            bail!("用户名不能包含空白或超过 64 字符");
        }
        if let Some(key_path) = &self.key_path
            && (key_path.chars().count() > 1024 || key_path.chars().any(char::is_control))
        {
            bail!("私钥路径不合法");
        }
        if self.auth == ServerAuthKind::PrivateKey && self.key_path.as_deref().unwrap_or("").trim().is_empty() {
            bail!("私钥认证必须填写私钥文件路径");
        }
        if self.note.chars().count() > 200 {
            bail!("备注过长（最多 200 字）");
        }
        if let Some(shortcut) = &self.shortcut {
            normalize_shortcut(shortcut).context("快捷键格式不合法")?;
        }
        Ok(())
    }

    /// 导入前整理：去除首尾空白，返回是否值得保留。
    pub fn sanitized(mut self) -> Option<Self> {
        self.name = self.name.trim().to_string();
        self.host = self.host.trim().to_string();
        self.username = self.username.trim().to_string();
        self.note = self.note.trim().to_string();
        self.key_path = self.key_path.take().map(|p| p.trim().to_string());
        self.shortcut = self
            .shortcut
            .take()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        if self.validate().is_ok() {
            Some(self)
        } else {
            None
        }
    }
}

/// 服务器导入/导出文件格式。只含连接元数据——凭据在系统密钥环里，永不落盘；
/// 导入后的条目首次连接时补输凭据（可勾选记住）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServersFile {
    /// 文件格式版本；缺省按当前版本解析（向前兼容旧文件）。
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub servers: Vec<ServerEntry>,
}

impl ServersFile {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(servers: Vec<ServerEntry>) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            servers,
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let body =
            toml::to_string_pretty(self).context("序列化服务器导出文件失败")?;
        std::fs::write(path, body).with_context(|| format!("写入 {} 失败", path.display()))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let mut file: Self = toml::from_str(&body).context("解析服务器文件失败（需 [[servers]] 格式）")?;
        if file.version > Self::CURRENT_VERSION {
            bail!("文件版本 v{} 高于当前支持 v{}，请升级 LaterTerm", file.version, Self::CURRENT_VERSION);
        }
        file.version = Self::CURRENT_VERSION;
        Ok(file)
    }
}

use std::path::PathBuf;

/// 认证方式（持久化形态；连接时映射为 laterterm_ssh::SshAuth）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredAuthKind {
    /// ssh-agent（SSH_AUTH_SOCK）。
    Agent,
    /// 私钥文件路径。
    KeyFile(String),
    /// 系统密钥环存密码（account = [`password_account`]）。
    Keyring,
}

/// 一台远程服务器。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredServerEntry {
    /// 显示名（唯一标识，重名保存时后者覆盖前者）。
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub auth: StoredAuthKind,
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
    pub servers: Vec<StoredServerEntry>,
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
    pub fn get(&self, name: &str) -> Option<&StoredServerEntry> {
        self.servers.iter().find(|server| server.name == name)
    }

    /// 插入或按名覆盖（重命名语义 = 先 remove 再 upsert）。
    pub fn upsert(&mut self, entry: StoredServerEntry) {
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
    use crate::Settings;

    fn entry(name: &str, host: &str) -> ServerEntry {
        ServerEntry {
            name: name.into(),
            host: host.into(),
            username: "ops".into(),
            ..Default::default()
        }
    }

    #[test]
    fn validates_required_fields() {
        assert!(entry("跳板机", "10.0.0.1").validate().is_ok());
        assert!(entry("", "10.0.0.1").validate().is_err());
        assert!(entry("a/b", "10.0.0.1").validate().is_err());
        assert!(entry("主机为空", "").validate().is_err());

        let zero_port = ServerEntry {
            name: "p".into(),
            host: "h".into(),
            port: 0,
            ..Default::default()
        };
        assert!(zero_port.validate().is_err());

        let keyless = ServerEntry {
            auth: ServerAuthKind::PrivateKey,
            ..entry("k", "h")
        };
        assert!(keyless.validate().is_err(), "私钥认证必须带路径");
    }

    #[test]
    fn settings_server_crud_replaces_and_removes() {
        let mut settings = Settings::default();
        settings.upsert_server(entry("a", "10.0.0.1")).unwrap();
        settings.upsert_server(entry("a", "10.0.0.2")).unwrap();
        assert_eq!(settings.servers.len(), 1);
        assert_eq!(settings.servers[0].host, "10.0.0.2");
        assert!(settings.remove_server("a"));
        assert!(settings.servers.is_empty());
        assert!(!settings.remove_server("a"));
    }

    #[test]
    fn toml_roundtrip_keeps_auth_kind_and_optional_fields() {
        let raw = r#"
[[servers]]
name = "内网机"
host = "192.168.1.10"
port = 2222
username = "ops"
auth = "keyboard-interactive"
note = "老机房"

[[servers]]
name = "跳板"
host = "jump.example.com"
username = "root"
auth = "private-key"
key_path = "/home/me/.ssh/id_ed25519"
"#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.servers.len(), 2);
        assert_eq!(settings.servers[0].auth, ServerAuthKind::KeyboardInteractive);
        assert_eq!(settings.servers[0].port, 2222);
        assert_eq!(
            settings.servers[1].key_path.as_deref(),
            Some("/home/me/.ssh/id_ed25519")
        );
        // 缺省 port/auth 走默认值
        let legacy: Settings = toml::from_str("[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n").unwrap();
        assert_eq!(legacy.servers[0].port, 22);
        assert_eq!(legacy.servers[0].auth, ServerAuthKind::Password);
        assert!(settings.servers[0].validate().is_ok());
    }

    #[test]
    fn sanitize_drops_unsalvageable_and_trims_rest() {
        let raw = r#"
[[servers]]
name = "  跳板  "
host = " jump.example.com "
username = " root "
note = "ok"

[[servers]]
name = "  "
host = "h"
username = "u"
"#;
        let mut settings: Settings = toml::from_str(raw).unwrap();
        settings.sanitize_servers();
        assert_eq!(settings.servers.len(), 1);
        assert_eq!(settings.servers[0].name, "跳板");
        assert_eq!(settings.servers[0].host, "jump.example.com");
        assert_eq!(settings.servers[0].username, "root");
    }

    #[test]
    fn servers_file_roundtrip_and_version_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers-export.toml");
        let file = ServersFile::new(vec![ServerEntry {
            name: "跳板".into(),
            host: "jump.example.com".into(),
            port: 2222,
            username: "ops".into(),
            auth: ServerAuthKind::KeyboardInteractive,
            key_path: None,
            note: "导出测试".into(),
            shortcut: Some("ctrl+alt+1".into()),
            encoding: None,
        }]);
        file.save_to(&path).unwrap();
        let loaded = ServersFile::load_from(&path).unwrap();
        assert_eq!(loaded.version, ServersFile::CURRENT_VERSION);
        assert_eq!(loaded.servers, file.servers);
        assert!(loaded.servers[0].validate().is_ok());

        // 缺 version 的旧文件按当前版本解析
        let legacy: ServersFile =
            toml::from_str("[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n").unwrap();
        assert_eq!(legacy.version, 0);
        assert_eq!(legacy.servers.len(), 1);

        // 未来版本拒绝导入（version 必须在 [[servers]] 之前的文件顶层）
        let future = "version = 99\n[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n";
        std::fs::write(&path, future).unwrap();
        assert!(ServersFile::load_from(&path).is_err());
    }

    #[test]
    fn entry_sanitized_trims_and_drops_invalid() {
        let ok = ServerEntry {
            name: " 跳板 ".into(),
            host: " h ".into(),
            username: " ops ".into(),
            ..Default::default()
        }
        .sanitized()
        .unwrap();
        assert_eq!(ok.name, "跳板");
        assert_eq!(ok.host, "h");
        assert_eq!(ok.username, "ops");
        assert!(ServerEntry::default().sanitized().is_none(), "空条目丢弃");
    }
}

impl From<StoredServerEntry> for ServerEntry {
    fn from(entry: StoredServerEntry) -> Self {
        let (auth, key_path) = match entry.auth {
            StoredAuthKind::Agent => (ServerAuthKind::Agent, None),
            StoredAuthKind::KeyFile(path) => (ServerAuthKind::PrivateKey, Some(path)),
            StoredAuthKind::Keyring => (ServerAuthKind::Password, None),
        };
        Self { name: entry.name, host: entry.host, port: entry.port, username: entry.username,
            auth, key_path, encoding: entry.encoding, ..Self::default() }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn main_servers_convert_without_losing_key_or_encoding() {
        let stored = StoredServerEntry { name: "旧服务器".into(), host: "host".into(), port: 2222,
            username: "ops".into(), auth: StoredAuthKind::KeyFile("/tmp/id".into()), encoding: Some("GBK".into()) };
        let entry = ServerEntry::from(stored);
        assert_eq!(entry.auth, ServerAuthKind::PrivateKey);
        assert_eq!(entry.key_path.as_deref(), Some("/tmp/id"));
        assert_eq!(entry.encoding.as_deref(), Some("GBK"));
        assert!(entry.validate().is_ok());
        let file = ServersFile::new(vec![entry]);
        let text = toml::to_string(&file).unwrap();
        assert_eq!(toml::from_str::<ServersFile>(&text).unwrap(), file);
    }
}
