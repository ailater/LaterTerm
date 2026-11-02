//! 快捷命令模板：只负责解析与安全展开，不执行 shell。

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

const MAX_TEMPLATE_LEN: usize = 4096;
const MAX_VARIABLE_LEN: usize = 64;
const MAX_VARIABLES: usize = 32;
const MAX_VALUE_LEN: usize = 1024;

fn default_confirm() -> bool {
    true
}

/// 可持久化的快捷命令模板。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandTemplate {
    pub name: String,
    pub template: String,
    pub shortcut: Option<String>,
    #[serde(default = "default_confirm")]
    pub confirm: bool,
}

impl Default for CommandTemplate {
    fn default() -> Self {
        Self {
            name: String::new(),
            template: String::new(),
            shortcut: None,
            confirm: true,
        }
    }
}

impl CommandTemplate {
    /// 提取模板中的有序、去重变量名，并同时校验模板语法。
    pub fn variables(&self) -> Result<Vec<String>> {
        parse_template(&self.template).map(|parts| {
            let mut variables = Vec::new();
            for part in parts {
                if let TemplatePart::Variable(name) = part
                    && !variables.contains(&name)
                {
                    variables.push(name);
                }
            }
            variables
        })
    }

    /// 用用户输入展开模板。变量值按 shell 单引号规则转义，模板自身不执行 eval。
    pub fn expand(&self, values: &BTreeMap<String, String>) -> Result<String> {
        let parts = parse_template(&self.template)?;
        let mut output = String::new();
        for part in parts {
            match part {
                TemplatePart::Literal(text) => output.push_str(&text),
                TemplatePart::Variable(name) => {
                    let value = values
                        .get(&name)
                        .ok_or_else(|| anyhow::anyhow!("缺少参数: {name}"))?;
                    validate_value(&name, value)?;
                    output.push_str(&shell_quote(value));
                }
            }
        }
        if output.len() > MAX_TEMPLATE_LEN {
            bail!("展开后的命令过长（最多 {MAX_TEMPLATE_LEN} 字节）");
        }
        if output.contains('\0') || output.contains('\r') || output.contains('\n') {
            bail!("命令不能包含 NUL 或换行");
        }
        Ok(output)
    }

    /// 用于 UI 保存前校验名称、模板和快捷键。
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("命令名称不能为空");
        }
        if self.name.chars().count() > 80 {
            bail!("命令名称过长");
        }
        self.variables()?;
        if let Some(shortcut) = &self.shortcut {
            validate_shortcut(shortcut)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
enum TemplatePart {
    Literal(String),
    Variable(String),
}

fn parse_template(template: &str) -> Result<Vec<TemplatePart>> {
    if template.is_empty() {
        bail!("命令模板不能为空");
    }
    if template.len() > MAX_TEMPLATE_LEN {
        bail!("命令模板过长（最多 {MAX_TEMPLATE_LEN} 字节）");
    }
    if template.contains('\0') || template.contains('\r') || template.contains('\n') {
        bail!("命令模板不能包含 NUL 或换行");
    }

    let mut parts = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = template[cursor..].find("{{") {
        let start = cursor + relative_start;
        if start > cursor {
            parts.push(TemplatePart::Literal(template[cursor..start].to_string()));
        }
        let name_start = start + 2;
        let Some(relative_end) = template[name_start..].find("}}") else {
            bail!("模板占位符未闭合");
        };
        let end = name_start + relative_end;
        let name = template[name_start..end].trim();
        if name.is_empty()
            || name.len() > MAX_VARIABLE_LEN
            || !name.chars().enumerate().all(|(index, ch)| {
                ch == '_'
                    || ch.is_ascii_alphanumeric() && index > 0
                    || ch.is_ascii_alphabetic() && index == 0
            })
        {
            bail!("非法模板变量名: {name}");
        }
        parts.push(TemplatePart::Variable(name.to_string()));
        cursor = end + 2;
        if parts.len() > MAX_VARIABLES * 2 {
            bail!("模板变量过多");
        }
    }
    if cursor < template.len() {
        parts.push(TemplatePart::Literal(template[cursor..].to_string()));
    }
    if parts.is_empty() {
        parts.push(TemplatePart::Literal(template.to_string()));
    }
    if template.contains("{{") || template.contains("}}") {
        // Any unmatched brace pair is almost certainly a typo.
        let reconstructed: String = parts
            .iter()
            .map(|part| match part {
                TemplatePart::Literal(text) => text.clone(),
                TemplatePart::Variable(name) => format!("{{{{{name}}}}}"),
            })
            .collect();
        if reconstructed != template {
            bail!("模板包含无法解析的占位符");
        }
    }
    Ok(parts)
}

fn validate_value(name: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("参数不能为空: {name}");
    }
    if value.len() > MAX_VALUE_LEN {
        bail!("参数过长: {name}");
    }
    if value.contains('\0') || value.contains('\r') || value.contains('\n') {
        bail!("参数不能包含 NUL 或换行: {name}");
    }
    Ok(())
}

/// POSIX shell 单引号转义：`a'b` -> `'a'\''b'`。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 归一化 `修饰键+主按键`，用于校验及冲突比较，不修改保存的原始文本。
///
/// `control` 等同于 `ctrl`，`super`、`meta` 等同于 `cmd`；修饰键按
/// `ctrl`、`alt`、`shift`、`cmd` 的顺序输出，同一修饰键不能重复。
pub fn normalize_shortcut(shortcut: &str) -> Result<String> {
    let normalized = shortcut
        .split('+')
        .map(|part| part.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if normalized.len() < 2 || normalized.iter().any(String::is_empty) {
        bail!("快捷键格式应为 ctrl+alt+k 一类组合");
    }
    let key = normalized.last().expect("至少包含两个按键");
    if key.len() > 20 || normalized.join("+").len() > 40 {
        bail!("快捷键过长");
    }
    let modifiers = ["ctrl", "control", "alt", "shift", "cmd", "super", "meta"];
    if modifiers.contains(&key.as_str()) {
        bail!("快捷键必须包含主按键");
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "[]\\\\-_=;,./".contains(ch))
    {
        bail!("快捷键包含非法按键");
    }

    let mut present = [false; 4];
    for modifier in &normalized[..normalized.len() - 1] {
        let index = match modifier.as_str() {
            "ctrl" | "control" => 0,
            "alt" => 1,
            "shift" => 2,
            "cmd" | "super" | "meta" => 3,
            _ => bail!("未知快捷键修饰键: {modifier}"),
        };
        if present[index] {
            bail!("快捷键修饰键重复: {modifier}");
        }
        present[index] = true;
    }
    let mut result = ["ctrl", "alt", "shift", "cmd"]
        .into_iter()
        .zip(present)
        .filter_map(|(name, included)| included.then_some(name))
        .collect::<Vec<_>>();
    result.push(key);
    Ok(result.join("+"))
}

fn validate_shortcut(shortcut: &str) -> Result<()> {
    normalize_shortcut(shortcut).map(|_| ())
}

pub fn shortcut_conflicts<'a, I>(shortcuts: I) -> Vec<String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut seen = BTreeMap::new();
    let mut conflicts = Vec::new();
    for (name, shortcut) in shortcuts {
        let Ok(normalized) = normalize_shortcut(shortcut) else {
            continue;
        };
        if let Some(previous) = seen.insert(normalized.clone(), name.to_string()) {
            conflicts.push(format!("{previous} 与 {name} 冲突: {normalized}"));
        }
    }
    conflicts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_are_ordered_and_deduplicated() {
        let command = CommandTemplate {
            name: "ssh".into(),
            template: "ssh {{user}}@{{host}} -p {{port}} --as {{user}}".into(),
            ..Default::default()
        };
        assert_eq!(command.variables().unwrap(), vec!["user", "host", "port"]);
    }

    #[test]
    fn expands_with_shell_quoting() {
        let command = CommandTemplate {
            name: "ssh".into(),
            template: "ssh {{user}}@{{host}} -p {{port}}".into(),
            ..Default::default()
        };
        let values = BTreeMap::from([
            ("user".into(), "alice".into()),
            ("host".into(), "jump.example".into()),
            ("port".into(), "22".into()),
        ]);
        assert_eq!(
            command.expand(&values).unwrap(),
            "ssh 'alice'@'jump.example' -p '22'"
        );
    }

    #[test]
    fn quote_embedded_single_quote() {
        let command = CommandTemplate {
            name: "echo".into(),
            template: "printf %s {{value}}".into(),
            ..Default::default()
        };
        let values = BTreeMap::from([("value".into(), "a'b".into())]);
        assert_eq!(command.expand(&values).unwrap(), "printf %s 'a'\\''b'");
    }

    #[test]
    fn rejects_bad_template_and_control_values() {
        let command = CommandTemplate {
            name: "bad".into(),
            template: "echo {{user".into(),
            ..Default::default()
        };
        assert!(command.variables().is_err());
        let command = CommandTemplate {
            name: "bad".into(),
            template: "echo {{user}}".into(),
            ..Default::default()
        };
        let values = BTreeMap::from([("user".into(), "x\ny".into())]);
        assert!(command.expand(&values).is_err());
    }

    #[test]
    fn normalizes_and_detects_shortcut_conflicts() {
        assert_eq!(
            normalize_shortcut(" Ctrl+Shift+K ").unwrap(),
            "ctrl+shift+k"
        );
        assert!(normalize_shortcut("ctrl").is_err());
        let conflicts = shortcut_conflicts([("a", "ctrl+k"), ("b", " Ctrl+K ")]);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn normalizes_shortcut_modifier_aliases_and_order() {
        for (input, expected) in [
            (" Control + Left ", "ctrl+left"),
            ("CONTROL+UP", "ctrl+up"),
            ("Ctrl+Right", "ctrl+right"),
            ("ctrl+down", "ctrl+down"),
            ("shift+control+k", "ctrl+shift+k"),
            ("SHIFT+ALT+CONTROL+K", "ctrl+alt+shift+k"),
            ("meta+shift+superkey", "shift+cmd+superkey"),
            ("Super+ALT+Control+Shift+K", "ctrl+alt+shift+cmd+k"),
            ("cmd+alt+k", "alt+cmd+k"),
            ("META+ALT+K", "alt+cmd+k"),
        ] {
            let normalized = normalize_shortcut(input).unwrap();
            assert_eq!(normalized, expected, "{input}");
            assert_eq!(
                normalize_shortcut(&normalized).unwrap(),
                normalized,
                "{input}"
            );
        }
    }

    #[test]
    fn shortcut_conflicts_compare_aliases_and_modifier_order() {
        let conflicts = shortcut_conflicts([
            ("方向", "ctrl+left"),
            ("方向别名", "Control+Left"),
            ("另一个方向", "ctrl+right"),
            ("多修饰键", "alt+ctrl+shift+k"),
            ("重排", "shift+control+alt+k"),
            ("平台", "meta+alt+1"),
            ("平台别名", "alt+super+1"),
            ("无效输入", "hyper+alt+1"),
        ]);
        assert_eq!(
            conflicts,
            [
                "方向 与 方向别名 冲突: ctrl+left",
                "多修饰键 与 重排 冲突: ctrl+alt+shift+k",
                "平台 与 平台别名 冲突: alt+cmd+1",
            ]
        );
    }

    #[test]
    fn shortcut_rejects_duplicate_and_unknown_modifiers() {
        for input in [
            "ctrl+ctrl+k",
            "ctrl+control+left",
            "alt+alt+k",
            "shift+SHIFT+k",
            "cmd+super+k",
            "meta+cmd+k",
        ] {
            let error = normalize_shortcut(input).unwrap_err().to_string();
            assert!(error.contains("重复"), "{input}: {error}");
        }
        for input in ["hyper+k", "ctrl+unknown+left", "k+ctrl+left"] {
            let error = normalize_shortcut(input).unwrap_err().to_string();
            assert!(error.contains("修饰键"), "{input}: {error}");
        }
    }

    #[test]
    fn shortcut_rejects_malformed_inputs() {
        for input in [
            "",
            "ctrl",
            "ctrl+",
            "+ctrl+k",
            "ctrl++k",
            "ctrl+ +k",
            "ctrl+shift",
            "ctrl+\0",
            "ctrl+!",
            "ctrl+a b",
            "ctrl+中文",
            "ctrl+abcdefghijklmnopqrstu",
            "control+alt+shift+super+abcdefghijklmnopq",
        ] {
            assert!(normalize_shortcut(input).is_err(), "{input}");
        }
    }

    #[test]
    fn existing_builtin_and_punctuation_shortcuts_remain_compatible() {
        for shortcut in [
            "ctrl+t",
            "ctrl+w",
            "ctrl+j",
            "ctrl+k",
            "ctrl+left",
            "ctrl+right",
            "ctrl+up",
            "ctrl+down",
            "ctrl+shift+[",
            "ctrl+shift+]",
            "ctrl+shift+f",
            "ctrl+alt+1",
            "ctrl+-",
            "ctrl+_",
            "ctrl+=",
            "ctrl+;",
            "ctrl+,",
            "ctrl+.",
            "ctrl+/",
            "ctrl+\\",
        ] {
            assert_eq!(normalize_shortcut(shortcut).unwrap(), shortcut);
        }
        for (builtin, equivalent) in [
            ("ctrl+t", "control+t"),
            ("ctrl+left", "CONTROL+LEFT"),
            ("ctrl+right", "Control+Right"),
            ("ctrl+up", "control+up"),
            ("ctrl+down", "Ctrl+Down"),
            ("ctrl+shift+[", "shift+control+["),
            ("ctrl+shift+]", "shift+ctrl+]"),
            ("ctrl+shift+f", "shift+control+F"),
        ] {
            assert_eq!(
                shortcut_conflicts([("内置", builtin), ("用户", equivalent)]).len(),
                1,
                "{builtin} 与 {equivalent} 应当冲突"
            );
        }
    }

    #[test]
    fn persisted_shortcuts_keep_original_spelling_and_detect_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            r#"
[[commands]]
name = "one"
template = "printf one"
shortcut = " Shift + Control + K "

[[commands]]
name = "two"
template = "printf two"
shortcut = "ctrl+shift+k"
"#,
        )
        .unwrap();
        let mut settings = crate::Settings::load(&path).unwrap();
        assert_eq!(
            settings.commands[0].shortcut.as_deref(),
            Some(" Shift + Control + K ")
        );
        assert_eq!(settings.command_shortcut_conflicts().len(), 1);
        settings
            .upsert_command(settings.commands[0].clone())
            .unwrap();
        settings.save_to(&path).unwrap();
        let reloaded = crate::Settings::load(&path).unwrap();
        assert_eq!(reloaded.commands, settings.commands);
        assert_eq!(reloaded.command_shortcut_conflicts().len(), 1);
    }

    #[test]
    fn server_and_totp_legacy_shortcuts_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            r#"
[[servers]]
name = "server"
host = "example.test"
username = "ops"
shortcut = "Shift+Control+Left"

[[totp]]
name = "otp"
shortcut = "META+alt+1"
"#,
        )
        .unwrap();
        let mut settings = crate::Settings::load(&path).unwrap();
        assert_eq!(settings.servers.len(), 1);
        assert_eq!(settings.totp.len(), 1);
        assert_eq!(
            settings.servers[0].shortcut.as_deref(),
            Some("Shift+Control+Left")
        );
        assert_eq!(settings.totp[0].shortcut.as_deref(), Some("META+alt+1"));
        settings.upsert_server(settings.servers[0].clone()).unwrap();
        settings.upsert_totp(settings.totp[0].clone()).unwrap();
        assert_eq!(
            shortcut_conflicts([
                ("server", settings.servers[0].shortcut.as_deref().unwrap()),
                ("server duplicate", "ctrl+shift+left"),
                ("otp", settings.totp[0].shortcut.as_deref().unwrap()),
                ("otp duplicate", "alt+cmd+1"),
            ])
            .len(),
            2
        );
        settings.save_to(&path).unwrap();
        let reloaded = crate::Settings::load(&path).unwrap();
        assert_eq!(reloaded.servers, settings.servers);
        assert_eq!(reloaded.totp, settings.totp);
    }

    #[test]
    fn duplicate_and_unknown_modifiers_fail_save_validation() {
        for shortcut in ["ctrl+control+left", "hyper+left"] {
            let command = CommandTemplate {
                name: "cmd".into(),
                template: "true".into(),
                shortcut: Some(shortcut.into()),
                ..Default::default()
            };
            let server = crate::ServerEntry {
                name: "server".into(),
                host: "example.test".into(),
                username: "ops".into(),
                shortcut: Some(shortcut.into()),
                ..Default::default()
            };
            let totp = crate::TotpEntry {
                name: "otp".into(),
                shortcut: Some(shortcut.into()),
                ..Default::default()
            };
            let mut settings = crate::Settings::default();
            assert!(settings.upsert_command(command).is_err(), "{shortcut}");
            assert!(settings.upsert_server(server).is_err(), "{shortcut}");
            assert!(settings.upsert_totp(totp).is_err(), "{shortcut}");
        }
    }

    #[test]
    fn legacy_invalid_shortcuts_keep_server_and_totp_values_after_load_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            r#"
[[commands]]
name = "command"
template = "true"
shortcut = "ctrl+hyper+1"

[[servers]]
name = "duplicate"
host = "example.test"
username = "ops"
shortcut = " Ctrl + Control + Left "

[[servers]]
name = "unknown"
host = "example.test"
username = "ops"
shortcut = "hyper+left"

[[servers]]
name = "empty-shortcut"
host = "example.test"
username = "ops"
shortcut = " "

[[servers]]
name = "bad-host"
host = ""
username = "ops"
shortcut = "hyper+left"

[[totp]]
name = "otp"
shortcut = "ctrl+ctrl+right"
"#,
        )
        .unwrap();
        let mut settings = crate::Settings::load(&path).unwrap();
        assert_eq!(
            settings.servers.len(),
            3,
            "合法连接信息不能被错误快捷键影响"
        );
        assert_eq!(
            settings.commands[0].shortcut.as_deref(),
            Some("ctrl+hyper+1")
        );
        assert_eq!(
            settings.servers[0].shortcut.as_deref(),
            Some(" Ctrl + Control + Left ")
        );
        assert_eq!(settings.servers[1].shortcut.as_deref(), Some("hyper+left"));
        assert_eq!(settings.servers[2].shortcut.as_deref(), Some(" "));
        assert_eq!(
            settings.totp[0].shortcut.as_deref(),
            Some("ctrl+ctrl+right")
        );
        assert!(settings.commands[0].validate().is_err());
        assert!(settings.servers[0].validate().is_err());
        assert!(settings.servers[1].validate().is_err());
        assert!(settings.servers[2].validate().is_err());
        assert!(settings.totp[0].validate().is_err());
        assert!(
            settings
                .upsert_command(settings.commands[0].clone())
                .is_err()
        );
        assert!(settings.upsert_server(settings.servers[0].clone()).is_err());
        assert!(settings.upsert_totp(settings.totp[0].clone()).is_err());
        settings.save_to(&path).unwrap();
        let reloaded = crate::Settings::load(&path).unwrap();
        assert_eq!(reloaded.commands, settings.commands);
        assert_eq!(reloaded.servers, settings.servers);
        assert_eq!(reloaded.totp, settings.totp);
    }

    #[test]
    fn importing_servers_retains_invalid_shortcut_without_disabling_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers.toml");
        let file = crate::ServersFile::new(vec![crate::ServerEntry {
            name: " server ".into(),
            host: " example.test ".into(),
            username: " ops ".into(),
            shortcut: Some(" Ctrl + Control + Left ".into()),
            ..Default::default()
        }]);
        file.save_to(&path).unwrap();
        let imported = crate::ServersFile::load_from(&path).unwrap();
        let server = imported
            .servers
            .into_iter()
            .next()
            .unwrap()
            .sanitized()
            .unwrap();
        assert_eq!(server.name, "server");
        assert_eq!(server.host, "example.test");
        assert_eq!(server.shortcut.as_deref(), Some(" Ctrl + Control + Left "));
        assert!(server.validate().is_err());
        let mut settings = crate::Settings::default();
        assert!(settings.upsert_server(server.clone()).is_err());
        // UI 的导入路径在检查重名后直接保存 sanitized 返回的条目。
        settings.servers.push(server);
        let settings_path = dir.path().join("settings.toml");
        settings.save_to(&settings_path).unwrap();
        assert_eq!(
            crate::Settings::load(&settings_path).unwrap().servers,
            settings.servers
        );
    }

    #[test]
    fn settings_command_crud_replaces_and_removes() {
        let mut settings = crate::Settings::default();
        settings
            .upsert_command(CommandTemplate {
                name: "a".into(),
                template: "echo a".into(),
                ..Default::default()
            })
            .unwrap();
        settings
            .upsert_command(CommandTemplate {
                name: "a".into(),
                template: "echo b".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(settings.commands.len(), 1);
        assert_eq!(settings.commands[0].template, "echo b");
        assert!(settings.remove_command("a"));
        assert!(settings.commands.is_empty());
        assert!(!settings.remove_command("a"));
    }

    #[test]
    fn legacy_defaults_and_validation() {
        let command: CommandTemplate =
            toml::from_str("name = 'clear'\ntemplate = 'clear'").unwrap();
        assert!(command.confirm);
        assert!(command.validate().is_ok());
        assert!(
            CommandTemplate {
                name: "".into(),
                template: "clear".into(),
                ..command
            }
            .validate()
            .is_err()
        );
    }
}
