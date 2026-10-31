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
    if !modifiers.contains(&key.as_str())
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "[]\\\\-_=;,./".contains(ch))
    {
        return Ok(normalized.join("+"));
    }
    if modifiers.contains(&key.as_str()) {
        bail!("快捷键必须包含主按键");
    }
    bail!("快捷键包含非法按键")
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
