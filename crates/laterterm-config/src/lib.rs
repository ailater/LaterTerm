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

pub mod theme;

pub use theme::{DEFAULT_THEME, Theme, ThemeRegistry, themes_dir};

pub const APP_DIR: &str = "laterterm";
pub const SETTINGS_FILE: &str = "settings.toml";

/// 终端外观与行为设置，对应工作台「设置」面板可调项。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
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
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            font_family: "JetBrainsMono Nerd Font".into(),
            font_size: 13.0,
            line_height: 1.0,
            padding: 8.0,
            scrollback: 100_000,
            theme: DEFAULT_THEME.into(),
        }
    }
}

impl Settings {
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
        let mut settings: Settings = toml::from_str(&raw)
            .with_context(|| format!("解析设置失败: {}", path.display()))?;
        settings.appearance.sanitize();
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
            std::fs::remove_file(path).with_context(|| format!("移除旧设置失败: {}", path.display()))?;
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
        path.push(format!("laterterm-config-test-{name}-{}.toml", std::process::id()));
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
    fn parse_error_is_reported_not_swallowed() {
        let path = temp_settings_path("broken");
        std::fs::write(&path, "not a toml file [[").unwrap();
        assert!(Settings::load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
