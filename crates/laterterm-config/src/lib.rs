//! laterterm-config — LaterTerm 设置持久化。
//!
//! 设置以 TOML 存于用户配置目录（Linux 为 `~/.config/laterterm/settings.toml`）。
//! 设计取舍：
//! - 字段全部 `#[serde(default)]`：文件缺字段或旧版本文件都能读到完整设置；
//! - 加载后做数值钳制：被手改坏的文件不至于让渲染层拿到 0 号字或负行高；
//! - 保存走「临时文件 + rename」原子替换，进程中途被杀不会留下半截配置。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod commands;
pub mod importers;
pub mod servers;
pub mod theme;
pub mod totp;

pub use commands::{CommandTemplate, normalize_shortcut, shortcut_conflicts};
pub use importers::{deepin_default_path, load_deepin_servers, load_xshell_sessions};
pub use servers::{ServerAuthKind, ServerEntry, ServersFile};
pub use theme::{DEFAULT_THEME, Theme, ThemeRegistry, themes_dir};
pub use totp::{TotpAlgorithm, TotpConfig, TotpEntry, generate_code, parse_secret_input};

pub const APP_DIR: &str = "laterterm";
pub const SETTINGS_FILE: &str = "settings.toml";

/// 终端外观与行为设置，对应工作台「设置」面板可调项。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// 内置面板快捷键覆盖；缺字段时沿用现有默认绑定。
    #[serde(default)]
    pub shortcuts: PanelShortcuts,
    /// 用户自定义快捷命令模板；缺字段的旧配置自动为空。
    #[serde(default)]
    pub commands: Vec<CommandTemplate>,
    /// TOTP 动态码条目元数据；密钥本体在系统密钥环（laterterm-secrets），绝不落盘。
    #[serde(default)]
    pub totp: Vec<totp::TotpEntry>,
    /// 服务器连接元数据；凭据同样只在密钥环。
    #[serde(default)]
    pub servers: Vec<servers::ServerEntry>,
    /// 旧主线 servers.toml 只迁移一次，避免已删除条目重启后复活。
    #[serde(default)]
    pub legacy_servers_migrated: bool,
}

/// 内置面板的快捷键覆盖；`None` 表示使用默认绑定，原始文本保持不变。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelShortcuts {
    pub commands_panel: Option<String>,
    pub servers_panel: Option<String>,
}

impl PanelShortcuts {
    /// 保存前复用统一快捷键校验；不归一化或清空已保存的绑定。
    pub fn validate(&self) -> Result<()> {
        for (panel, shortcut) in [
            ("快捷命令面板", &self.commands_panel),
            ("服务器面板", &self.servers_panel),
        ] {
            if let Some(shortcut) = shortcut {
                normalize_shortcut(shortcut).with_context(|| format!("{panel}快捷键格式不合法"))?;
            }
        }
        Ok(())
    }
}

/// 外观项。数值范围与设置面板的步进按钮一致。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub padding: f32,
    pub scrollback: usize,
    pub theme: String,
    /// 是否启用连字（calt）；终端下常有人偏好关闭。
    #[serde(default = "default_true")]
    pub ligatures: bool,
    /// 备选字体链（主字体缺字时按序回退，如 CJK）；空表示用系统默认回退。
    #[serde(default)]
    pub font_fallbacks: Vec<String>,

    /// 光标闪烁。默认开启；关闭后光标常显。
    #[serde(default = "default_true")]
    pub cursor_blink: bool,

    /// 平滑滚动：滚轮/触控板按像素累积连续滚动。默认开启。
    #[serde(default = "default_true")]
    pub smooth_scrolling: bool,
    /// 接收目录；空 = ~/Downloads。
    pub downloads_dir: String,
}

fn default_true() -> bool {
    true
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            font_family: "JetBrainsMono Nerd Font".into(),
            font_size: 13.0,
            // 1.2 行距读长日志更舒适；纯网格对齐不受影响（TUI 按单元格定位）
            line_height: 1.2,
            padding: 8.0,
            scrollback: 100_000,
            theme: DEFAULT_THEME.into(),
            ligatures: true,
            cursor_blink: true,
            smooth_scrolling: true,
            font_fallbacks: Vec::new(),
            downloads_dir: String::new(),
        }
    }
}

impl Settings {
    /// 保留当前同名配置，迁移标记与条目在同一 settings.toml 中原子保存。
    pub fn migrate_legacy_servers(&mut self, legacy: servers::ServerStore) -> bool {
        if self.legacy_servers_migrated {
            return false;
        }
        for entry in legacy.servers {
            let entry: servers::ServerEntry = entry.into();
            if let Some(entry) = entry.sanitized()
                && !self
                    .servers
                    .iter()
                    .any(|current| current.name == entry.name)
            {
                self.servers.push(entry);
            }
        }
        self.legacy_servers_migrated = true;
        true
    }

    pub fn upsert_command(&mut self, command: CommandTemplate) -> Result<()> {
        command.validate()?;
        if let Some(existing) = self
            .commands
            .iter_mut()
            .find(|item| item.name == command.name)
        {
            *existing = command;
        } else {
            self.commands.push(command);
        }
        Ok(())
    }

    pub fn remove_command(&mut self, name: &str) -> bool {
        let before = self.commands.len();
        self.commands.retain(|command| command.name != name);
        before != self.commands.len()
    }

    pub fn command_shortcut_conflicts(&self) -> Vec<String> {
        commands::shortcut_conflicts(self.commands.iter().filter_map(|command| {
            command
                .shortcut
                .as_deref()
                .map(|shortcut| (command.name.as_str(), shortcut))
        }))
    }

    /// 新增或按名称替换 TOTP 条目；密钥本体由调用方负责写入密钥环。
    pub fn upsert_totp(&mut self, entry: totp::TotpEntry) -> Result<()> {
        entry.validate()?;
        if let Some(existing) = self.totp.iter_mut().find(|item| item.name == entry.name) {
            *existing = entry;
        } else {
            self.totp.push(entry);
        }
        Ok(())
    }

    pub fn remove_totp(&mut self, name: &str) -> bool {
        let before = self.totp.len();
        self.totp.retain(|entry| entry.name != name);
        before != self.totp.len()
    }

    /// 把加载中的 TOTP 条目元数据修正到合法范围；无法挽救的条目直接丢弃。
    fn sanitize_totp(&mut self) {
        self.totp.retain_mut(|entry| {
            entry.name = entry.name.trim().to_string();
            if entry.name.is_empty() {
                return false;
            }
            entry.digits = entry.digits.clamp(6, 8);
            entry.period = entry.period.clamp(1, 3600);
            true
        });
    }

    /// 新增或按名称替换服务器条目。
    pub fn upsert_server(&mut self, server: servers::ServerEntry) -> Result<()> {
        server.validate()?;
        if let Some(existing) = self
            .servers
            .iter_mut()
            .find(|item| item.name == server.name)
        {
            *existing = server;
        } else {
            self.servers.push(server);
        }
        Ok(())
    }

    pub fn remove_server(&mut self, name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|server| server.name != name);
        before != self.servers.len()
    }

    /// 修正加载中的连接信息；错误快捷键保留原值供用户修正。
    fn sanitize_servers(&mut self) {
        self.servers.retain_mut(|server| {
            server.name = server.name.trim().to_string();
            server.host = server.host.trim().to_string();
            server.username = server.username.trim().to_string();
            server.note = server.note.trim().to_string();
            if let Some(key_path) = &server.key_path {
                let trimmed = key_path.trim().to_string();
                server.key_path = (!trimmed.is_empty()).then_some(trimmed);
            }
            if server.validate_connection().is_err() {
                return false;
            }
            server.warn_invalid_shortcut();
            true
        });
    }

    /// 用户配置目录下的设置文件路径。
    pub fn default_path() -> Option<PathBuf> {
        Some(config_dir()?.join(APP_DIR).join(SETTINGS_FILE))
    }
    /// 从指定路径加载设置；文件不存在时返回默认值（首次运行）。
    ///
    /// 解析失败会向上抛错，由调用方决定回退策略——避免把用户手改坏的
    /// 文件在下次保存时无声覆盖。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取设置失败: {}", path.display()))?;
        let mut settings: Settings =
            toml::from_str(&raw).with_context(|| format!("解析设置失败: {}", path.display()))?;
        settings.appearance.sanitize();
        settings.sanitize_totp();
        settings.sanitize_servers();
        Ok(settings)
    }

    /// 从默认路径加载；任何失败回退默认值并告警（启动路径不能被坏文件卡死）。
    pub fn load_or_default() -> Self {
        match Self::default_path() {
            Some(path) => Self::load(&path).unwrap_or_else(|error| {
                eprintln!("{error}，使用默认设置");
                Self::default()
            }),
            None => {
                eprintln!("无法定位用户配置目录，使用默认设置");
                Self::default()
            }
        }
    }

    /// 原子保存到指定路径（临时文件 + rename），自动创建父目录。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("序列化设置失败")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).with_context(|| format!("写入设置失败: {}", tmp.display()))?;
        // Windows 上 rename 不能覆盖已有文件，先移除再改名；Linux rename 本身原子。
        #[cfg(target_os = "windows")]
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("移除旧设置失败: {}", path.display()))?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换设置文件失败: {}", path.display()))?;
        Ok(())
    }

    /// 保存到默认路径；失败只告警不打断 UI。
    pub fn save(&self) {
        if let Some(path) = Self::default_path()
            && let Err(error) = self.save_to(&path)
        {
            eprintln!("{error:#}");
        }
    }

    /// 首次运行时生成默认配置文件，方便用户发现并手改。
    ///
    /// 返回是否真正创建（文件已存在则不动，坏文件也不会被覆盖）。
    pub fn ensure_settings_file(&self) -> bool {
        match Self::default_path() {
            Some(path) if !path.exists() => self.save_to(&path).is_ok(),
            _ => false,
        }
    }
}

impl Appearance {
    /// 把越界数值拉回面板允许的范围，防止坏文件产生 0 号字 / 负行高。
    pub fn sanitize(&mut self) {
        self.font_size = self.font_size.clamp(8.0, 32.0);
        self.line_height = self.line_height.clamp(0.8, 2.0);
        self.padding = self.padding.clamp(0.0, 24.0);
        self.scrollback = self.scrollback.clamp(1_000, 1_000_000);
        if self.font_family.trim().is_empty() {
            self.font_family = Self::default().font_family;
        }
        self.theme = self.theme.trim().to_string();
        if self.theme.is_empty() {
            self.theme = DEFAULT_THEME.into();
        }
        self.font_fallbacks = self
            .font_fallbacks
            .iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
    }
}

/// 应用配置目录（`<配置目录>/laterterm`），用于文件监听等。
pub fn app_config_dir() -> Option<PathBuf> {
    Some(config_dir()?.join(APP_DIR))
}

/// 平台配置目录（Linux 优先，遵循 XDG）。
fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && Path::new(&xdg).is_absolute()
        {
            return Some(PathBuf::from(xdg));
        }
        home_dir().map(|home| home.join(".config"))
    }

    #[cfg(target_os = "macos")]
    {
        return home_dir().map(|home| home.join("Library").join("Application Support"));
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA").map(PathBuf::from);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "laterterm-config-test-{name}-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn partial_file_fills_defaults() {
        let raw = "[appearance]\nfont_size = 15.0\n";
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.appearance.font_size, 15.0);
        assert_eq!(settings.appearance.font_family, "JetBrainsMono Nerd Font");
        assert_eq!(settings.appearance.scrollback, 100_000);
    }

    #[test]
    fn panel_shortcuts_default_when_legacy_settings_omit_them() {
        for raw in ["", "[appearance]\nfont_size = 15.0\n", "[shortcuts]\n"] {
            let settings: Settings = toml::from_str(raw).unwrap();
            assert_eq!(settings.shortcuts, PanelShortcuts::default());
            assert_eq!(settings.shortcuts.commands_panel, None);
            assert_eq!(settings.shortcuts.servers_panel, None);
            settings.shortcuts.validate().unwrap();
            let saved = toml::to_string_pretty(&settings).unwrap();
            assert_eq!(toml::from_str::<Settings>(&saved).unwrap(), settings);
        }
        for raw in [
            "[shortcuts]\ncommands_panel = 'ctrl+shift+k'\n",
            "[shortcuts]\nservers_panel = 'ctrl+shift+s'\n",
        ] {
            let settings: Settings = toml::from_str(raw).unwrap();
            assert_ne!(settings.shortcuts, PanelShortcuts::default());
            settings.shortcuts.validate().unwrap();
            let saved = toml::to_string_pretty(&settings).unwrap();
            assert_eq!(toml::from_str::<Settings>(&saved).unwrap(), settings);
        }
    }

    #[test]
    fn panel_shortcuts_validate_using_normalizer_without_rewriting_text() {
        let shortcuts = PanelShortcuts {
            commands_panel: Some("  Control + Shift + K  ".into()),
            servers_panel: Some(" Super + Alt + S ".into()),
        };
        let original = shortcuts.clone();
        shortcuts.validate().unwrap();
        assert_eq!(shortcuts, original);
        assert_eq!(
            normalize_shortcut(shortcuts.commands_panel.as_deref().unwrap()).unwrap(),
            "ctrl+shift+k"
        );
        assert_eq!(
            normalize_shortcut(shortcuts.servers_panel.as_deref().unwrap()).unwrap(),
            "alt+cmd+s"
        );
    }

    #[test]
    fn panel_shortcuts_reject_each_invalid_override_without_rewriting_it() {
        for shortcut in ["", "   ", "ctrl", "ctrl+control+k", "alt++k", "unknown+k"] {
            for shortcuts in [
                PanelShortcuts {
                    commands_panel: Some(shortcut.into()),
                    ..Default::default()
                },
                PanelShortcuts {
                    servers_panel: Some(shortcut.into()),
                    ..Default::default()
                },
            ] {
                let original = shortcuts.clone();
                assert!(shortcuts.validate().is_err(), "{shortcuts:?}");
                assert_eq!(shortcuts, original);
            }
        }
    }

    #[test]
    fn settings_load_save_preserves_panel_and_legacy_shortcut_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let raw = r#"
[shortcuts]
commands_panel = "  Control + Shift + K  "
servers_panel = "   "

[[commands]]
name = "命令"
template = "pwd"
shortcut = "  Ctrl + Shift + L  "

[[totp]]
name = "动态码"
shortcut = "   "

[[servers]]
name = "服务器"
host = "example.com"
username = "ops"
shortcut = "  legacy shortcut  "
"#;
        std::fs::write(&path, raw).unwrap();
        let settings = Settings::load(&path).unwrap();
        settings.save_to(&path).unwrap();
        let original: toml::Value = toml::from_str(raw).unwrap();
        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.get("shortcuts"), original.get("shortcuts"));
        for collection in ["commands", "totp", "servers"] {
            assert_eq!(
                saved[collection][0]["shortcut"], original[collection][0]["shortcut"],
                "{collection} 的历史快捷键必须保留原值"
            );
        }
        assert_eq!(Settings::load(&path).unwrap(), settings);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let path = temp_settings_path("missing");
        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn load_sanitizes_out_of_range_values() {
        let path = temp_settings_path("sanitize");
        std::fs::write(
            &path,
            "[appearance]\nfont_size = 3.0\nline_height = 9.0\npadding = -5.0\nscrollback = 0\nfont_family = \"  \"\n",
        )
        .unwrap();
        let settings = Settings::load(&path).unwrap();
        let appearance = settings.appearance;
        assert_eq!(appearance.font_size, 8.0);
        assert_eq!(appearance.line_height, 2.0);
        assert_eq!(appearance.padding, 0.0);
        assert_eq!(appearance.scrollback, 1_000);
        assert_eq!(appearance.font_family, "JetBrainsMono Nerd Font");
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_settings_path("roundtrip");
        let mut settings = Settings::default();
        settings.appearance.font_size = 16.0;
        settings.appearance.scrollback = 50_000;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(settings, loaded);
        // 原子写不应留下临时文件
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn font_options_roundtrip_and_defaults() {
        // 旧版文件缺 ligatures/font_fallbacks 字段也能读出默认值
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.ligatures);
        assert!(legacy.appearance.font_fallbacks.is_empty());

        let path = temp_settings_path("font-options");
        let mut settings = Settings::default();
        settings.appearance.ligatures = false;
        settings.appearance.font_fallbacks = vec![
            " Sarasa Mono SC ".into(),
            "".into(),
            "Noto Sans Mono CJK SC".into(),
        ];
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.ligatures);
        // sanitize 应去空白、丢空项
        assert_eq!(
            loaded.appearance.font_fallbacks,
            vec!["Sarasa Mono SC", "Noto Sans Mono CJK SC"]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn cursor_and_smooth_scrolling_defaults_and_false_roundtrip() {
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.cursor_blink);
        assert!(legacy.appearance.smooth_scrolling);

        let path = temp_settings_path("scroll-options");
        let mut settings = Settings::default();
        settings.appearance.cursor_blink = false;
        settings.appearance.smooth_scrolling = false;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.cursor_blink);
        assert!(!loaded.appearance.smooth_scrolling);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn parse_error_is_reported_not_swallowed() {
        let path = temp_settings_path("broken");
        std::fs::write(&path, "not a toml file [[").unwrap();
        assert!(Settings::load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
