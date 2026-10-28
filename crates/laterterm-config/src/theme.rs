//! 主题定义与注册表。
//!
//! 主题 TOML 格式（`~/.config/laterterm/themes/<名字>.toml`）：
//!
//! ```toml
//! [colors]
//! background = "#1A1B26"
//! foreground = "#C0CAF5"
//! # ...共 19 个键：background/foreground/cursor + 16 ANSI 色
//! ```
//!
//! 设计取舍：
//! - 颜色统一 `#RRGGBB` 字符串，缺哪个键就回退内置 Tokyo Night 的对应值，
//!   手写主题漏一两键不至于整体失效；
//! - 自定义主题与内置主题同名时覆盖内置，方便微调预设；
//! - 坏文件跳过并告警，注册表永远至少有默认主题可用。

use crate::{APP_DIR, config_dir};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 默认主题名。
pub const DEFAULT_THEME: &str = "tokyo-night";

/// 自定义主题目录（`<配置目录>/laterterm/themes`）。
pub fn themes_dir() -> Option<PathBuf> {
    Some(config_dir()?.join(APP_DIR).join("themes"))
}

/// 一个终端主题。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    /// 主题名（显示与引用用；缺省取文件名）。
    #[serde(default)]
    pub name: String,
    pub colors: ThemeColors,
}

/// 主题颜色，19 个 `0xRRGGBB` 值。
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeColors {
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub black: u32,
    pub red: u32,
    pub green: u32,
    pub yellow: u32,
    pub blue: u32,
    pub magenta: u32,
    pub cyan: u32,
    pub white: u32,
    pub bright_black: u32,
    pub bright_red: u32,
    pub bright_green: u32,
    pub bright_yellow: u32,
    pub bright_blue: u32,
    pub bright_magenta: u32,
    pub bright_cyan: u32,
    pub bright_white: u32,
}

impl ThemeColors {
    /// 内置 Tokyo Night 预设，作为所有缺失键的回退基准。
    pub fn tokyo_night() -> Self {
        Self {
            background: 0x1A1B26,
            foreground: 0xC0CAF5,
            cursor: 0xC0CAF5,
            black: 0x15161E,
            red: 0xF7768E,
            green: 0x9ECE6A,
            yellow: 0xE0AF68,
            blue: 0x7AA2F7,
            magenta: 0xBB9AF7,
            cyan: 0x7DCFFF,
            white: 0xA9B1D6,
            bright_black: 0x414868,
            bright_red: 0xF7768E,
            bright_green: 0x9ECE6A,
            bright_yellow: 0xE0AF68,
            bright_blue: 0x7AA2F7,
            bright_magenta: 0xBB9AF7,
            bright_cyan: 0x7DCFFF,
            bright_white: 0xC0CAF5,
        }
    }

    /// 内置 Tokyo Night Storm 预设（更亮的背景）。
    pub fn tokyo_night_storm() -> Self {
        Self {
            background: 0x24283B,
            foreground: 0xC0CAF5,
            cursor: 0xC0CAF5,
            black: 0x1F2335,
            red: 0xF7768E,
            green: 0x9ECE6A,
            yellow: 0xE0AF68,
            blue: 0x7AA2F7,
            magenta: 0xBB9AF7,
            cyan: 0x7DCFFF,
            white: 0xA9B1D6,
            bright_black: 0x414868,
            bright_red: 0xFF7A93,
            bright_green: 0xB9F27C,
            bright_yellow: 0xFF9E64,
            bright_blue: 0x7DA6FF,
            bright_magenta: 0xBB9AF7,
            bright_cyan: 0x0DB9D7,
            bright_white: 0xC0CAF5,
        }
    }

    /// 内置 Gruvbox Dark 预设。
    pub fn gruvbox_dark() -> Self {
        Self {
            background: 0x282828,
            foreground: 0xEBDBB2,
            cursor: 0xEBDBB2,
            black: 0x282828,
            red: 0xCC241D,
            green: 0x98971A,
            yellow: 0xD79921,
            blue: 0x458588,
            magenta: 0xB16286,
            cyan: 0x689D6A,
            white: 0xA89984,
            bright_black: 0x928374,
            bright_red: 0xFB4934,
            bright_green: 0xB8BB26,
            bright_yellow: 0xFABD2F,
            bright_blue: 0x83A598,
            bright_magenta: 0xD3869B,
            bright_cyan: 0x8EC07C,
            bright_white: 0xEBDBB2,
        }
    }

    fn set(&mut self, key: &str, value: u32) -> bool {
        match key {
            "background" => self.background = value,
            "foreground" => self.foreground = value,
            "cursor" => self.cursor = value,
            "black" => self.black = value,
            "red" => self.red = value,
            "green" => self.green = value,
            "yellow" => self.yellow = value,
            "blue" => self.blue = value,
            "magenta" => self.magenta = value,
            "cyan" => self.cyan = value,
            "white" => self.white = value,
            "bright_black" => self.bright_black = value,
            "bright_red" => self.bright_red = value,
            "bright_green" => self.bright_green = value,
            "bright_yellow" => self.bright_yellow = value,
            "bright_blue" => self.bright_blue = value,
            "bright_magenta" => self.bright_magenta = value,
            "bright_cyan" => self.bright_cyan = value,
            "bright_white" => self.bright_white = value,
            _ => return false,
        }
        true
    }

    fn entries(&self) -> [(&'static str, u32); 19] {
        [
            ("background", self.background),
            ("foreground", self.foreground),
            ("cursor", self.cursor),
            ("black", self.black),
            ("red", self.red),
            ("green", self.green),
            ("yellow", self.yellow),
            ("blue", self.blue),
            ("magenta", self.magenta),
            ("cyan", self.cyan),
            ("white", self.white),
            ("bright_black", self.bright_black),
            ("bright_red", self.bright_red),
            ("bright_green", self.bright_green),
            ("bright_yellow", self.bright_yellow),
            ("bright_blue", self.bright_blue),
            ("bright_magenta", self.bright_magenta),
            ("bright_cyan", self.bright_cyan),
            ("bright_white", self.bright_white),
        ]
    }
}

/// 解析 `#RRGGBB` / `RRGGBB` 颜色字符串。
pub fn parse_hex_color(value: &str) -> Option<u32> {
    let hex = value.trim().trim_start_matches('#');
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

fn format_hex_color(value: u32) -> String {
    format!("#{value:06X}")
}

// 序列化为 { "background": "#1A1B26", ... }；未知键忽略、缺键回退默认。
impl Serialize for ThemeColors {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let map: BTreeMap<&str, String> = self
            .entries()
            .into_iter()
            .map(|(key, value)| (key, format_hex_color(value)))
            .collect();
        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ThemeColors {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = BTreeMap::<String, String>::deserialize(deserializer)?;
        let mut colors = Self::tokyo_night();
        for (key, value) in &map {
            match parse_hex_color(value) {
                Some(rgb) if !colors.set(key, rgb) => {} // 未知键：忽略
                Some(_) => {}
                None => eprintln!("主题颜色 {key}={value} 无法解析，使用回退值"),
            }
        }
        Ok(colors)
    }
}

impl Theme {
    /// 从 TOML 文本解析主题；`file_stem` 作为 name 的缺省值。
    pub fn parse(file_stem: &str, raw: &str) -> anyhow::Result<Self> {
        let mut theme: Theme = toml::from_str(raw)
            .map_err(|error| anyhow::anyhow!("解析主题 {file_stem} 失败: {error}"))?;
        if theme.name.trim().is_empty() {
            theme.name = file_stem.to_string();
        }
        Ok(theme)
    }
}

/// 主题注册表：内置预设 + 用户自定义（同名覆盖内置）。
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeRegistry {
    themes: BTreeMap<String, Theme>,
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl ThemeRegistry {
    /// 仅内置预设。
    pub fn builtin() -> Self {
        let presets = [
            (DEFAULT_THEME, ThemeColors::tokyo_night()),
            ("tokyo-night-storm", ThemeColors::tokyo_night_storm()),
            ("gruvbox-dark", ThemeColors::gruvbox_dark()),
        ];
        Self {
            themes: presets
                .into_iter()
                .map(|(name, colors)| {
                    (
                        name.to_string(),
                        Theme {
                            name: name.to_string(),
                            colors,
                        },
                    )
                })
                .collect(),
        }
    }

    /// 内置预设 + 自定义主题目录（目录不存在或单个文件损坏都不致命）。
    pub fn load() -> Self {
        match themes_dir() {
            Some(dir) => Self::load_from_dir(&dir),
            None => Self::builtin(),
        }
    }

    /// 可测试版本：从指定目录加载自定义主题。
    pub fn load_from_dir(dir: &Path) -> Self {
        let mut registry = Self::builtin();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return registry;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|raw| Theme::parse(stem, &raw))
            {
                Ok(theme) => {
                    registry.themes.insert(theme.name.clone(), theme);
                }
                Err(error) => eprintln!("{error}，已跳过该主题"),
            }
        }
        registry
    }

    /// 按名取主题；不存在时回退默认主题。
    pub fn resolve(&self, name: &str) -> &Theme {
        self.themes
            .get(name)
            .unwrap_or_else(|| self.themes.get(DEFAULT_THEME).expect("内置预设必然存在"))
    }

    /// 按名取主题（严格匹配）。
    pub fn get(&self, name: &str) -> Option<&Theme> {
        self.themes.get(name)
    }

    /// 全部主题名（BTreeMap 保证稳定顺序，内置在前不保证但排序稳定）。
    pub fn names(&self) -> Vec<String> {
        self.themes.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_color_parsing() {
        assert_eq!(parse_hex_color("#1A1B26"), Some(0x1A1B26));
        assert_eq!(parse_hex_color("1a1b26"), Some(0x1A1B26));
        assert_eq!(parse_hex_color("0x1A1B26"), Some(0x1A1B26));
        assert_eq!(parse_hex_color("#1A1B2"), None);
        assert_eq!(parse_hex_color("#GGGGGG"), None);
        assert_eq!(parse_hex_color(""), None);
    }

    #[test]
    fn colors_serialize_roundtrip() {
        let colors = ThemeColors::gruvbox_dark();
        let map = toml::to_string(&colors).unwrap();
        assert!(map.contains("background = \"#282828\""));
        let parsed: ThemeColors = toml::from_str(&map).unwrap();
        assert_eq!(colors, parsed);
    }

    #[test]
    fn missing_keys_fall_back_to_tokyo_night() {
        let colors: ThemeColors =
            toml::from_str("background = \"#000000\"\nred = \"#FF0000\"").unwrap();
        assert_eq!(colors.background, 0x000000);
        assert_eq!(colors.red, 0xFF0000);
        // 缺失键回退 Tokyo Night
        let fallback = ThemeColors::tokyo_night();
        assert_eq!(colors.foreground, fallback.foreground);
        assert_eq!(colors.bright_white, fallback.bright_white);
    }

    #[test]
    fn bad_color_value_falls_back_but_keeps_good_ones() {
        let colors: ThemeColors =
            toml::from_str("background = \"not-a-color\"\nforeground = \"#ABCDEF\"").unwrap();
        assert_eq!(colors.background, ThemeColors::tokyo_night().background);
        assert_eq!(colors.foreground, 0xABCDEF);
    }

    #[test]
    fn theme_parse_defaults_name_to_file_stem() {
        let theme = Theme::parse("my-theme", "[colors]\nbackground = \"#123456\"").unwrap();
        assert_eq!(theme.name, "my-theme");
        assert_eq!(theme.colors.background, 0x123456);
    }

    #[test]
    fn registry_builtin_and_resolve_fallback() {
        let registry = ThemeRegistry::builtin();
        assert!(registry.get("tokyo-night").is_some());
        assert!(registry.get("gruvbox-dark").is_some());
        // 未知主题回退默认，而不是 None
        assert_eq!(registry.resolve("no-such-theme").name, DEFAULT_THEME);
    }

    #[test]
    fn registry_custom_dir_overrides_and_adds() {
        let dir =
            std::env::temp_dir().join(format!("laterterm-themes-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 自定义覆盖内置
        std::fs::write(
            dir.join("tokyo-night.toml"),
            "[colors]\nbackground = \"#000000\"\n",
        )
        .unwrap();
        // 新增主题
        std::fs::write(
            dir.join("solarized.toml"),
            "[colors]\nbackground = \"#002B36\"\n",
        )
        .unwrap();
        // 坏文件跳过
        std::fs::write(dir.join("broken.toml"), "]]not toml").unwrap();
        // 非 toml 忽略
        std::fs::write(dir.join("readme.txt"), "hello").unwrap();

        let registry = ThemeRegistry::load_from_dir(&dir);
        assert_eq!(registry.resolve("tokyo-night").colors.background, 0x000000);
        assert_eq!(registry.resolve("solarized").colors.background, 0x002B36);
        assert_eq!(registry.names().len(), 4); // 3 内置 + solarized；broken 被跳过
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
