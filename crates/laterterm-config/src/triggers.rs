//! 输出触发器规则：终端输出正则命中 → 桌面通知 / 标签高亮。
//!
//! 刻意不依赖 regex：本 crate 只存规则原文与动作（先例 commands.rs 的
//! 职责边界），正则编译一律在 app 侧（audit.rs compile_rules，设置保存/
//! settings.toml 热加载时重编译并热替换，扫描在审计工作线程内进行）。
//! 因此加载期不校验语法——手改 settings.toml 的坏正则到编译期才「休眠 +
//! eprintln 告警」；UI 保存路径由 app 侧先行 `regex::bytes::Regex::new`
//! 校验后错误就地显示，坏正则不会经由 UI 落盘。

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// 规则名限长（与命令名同口径）。
const MAX_NAME_CHARS: usize = 80;
/// 正则模式限长（字节）：防误粘超长文本；正常触发词远小于此。
const MAX_PATTERN_LEN: usize = 1024;

/// 触发动作。
///
/// `Notify` 兼作未知值兜底（`#[serde(other)]` 物理上必须排末位，同
/// CursorStyle 的 Block 口径）：手改出坏值降级为通知而非解析失败拖垮
/// 整份配置。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerAction {
    /// 标签高亮（锁存到窗格，焦点/激活标签时清除）。
    Highlight,
    /// 桌面通知（按窗格+规则 30s 冷却，防刷屏）。
    #[serde(other)]
    #[default]
    Notify,
}

impl TriggerAction {
    /// 动作的中文展示（设置页规则行）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Notify => "通知",
            Self::Highlight => "高亮",
        }
    }
}

/// 可持久化的触发器规则。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TriggerRule {
    /// 规则名（唯一键，upsert/remove/去抖冷却均按名）。
    pub name: String,
    /// 正则模式（`regex::bytes` 语义，按 PTY 字节流匹配；中文模式可用——
    /// SSH GBK 会话扫描的是转码后 UTF-8 流，与审计日志同口径）。
    pub pattern: String,
    pub action: TriggerAction,
    /// 停用规则保留原文（不删配置，开关即改 enabled）。
    pub enabled: bool,
}

impl Default for TriggerRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            pattern: String::new(),
            action: TriggerAction::Notify,
            enabled: true,
        }
    }
}

impl TriggerRule {
    /// UI 保存前校验名称与模式（正则语法不在此校验，见模块头注记）。
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("规则名称不能为空");
        }
        if self.name.chars().count() > MAX_NAME_CHARS {
            bail!("规则名称过长");
        }
        if self.pattern.trim().is_empty() {
            bail!("匹配模式不能为空");
        }
        if self.pattern.len() > MAX_PATTERN_LEN {
            bail!("匹配模式过长（最多 {MAX_PATTERN_LEN} 字节）");
        }
        if self.pattern.contains('\0') {
            bail!("匹配模式不能包含 NUL");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_serializes_lowercase_and_unknown_falls_back_to_notify() {
        // TOML 中枚举序列化为小写字符串（rename_all lowercase）。toml 不支持
        // 裸枚举作文档根（UnsupportedType），经包装表往返（CursorStyle 同款）。
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            action: TriggerAction,
        }
        let body = toml::to_string(&Wrap {
            action: TriggerAction::Notify,
        })
        .unwrap();
        assert!(body.contains("action = \"notify\""), "{body}");
        let body = toml::to_string(&Wrap {
            action: TriggerAction::Highlight,
        })
        .unwrap();
        assert!(body.contains("action = \"highlight\""), "{body}");
        // 手改坏值：serde(other) 兜底为 Notify，不拖垮整份配置（CursorStyle 同款）
        let unknown: Wrap = toml::from_str("action = \"wave\"").unwrap();
        assert_eq!(unknown.action, TriggerAction::Notify);
        let known: Wrap = toml::from_str("action = \"highlight\"").unwrap();
        assert_eq!(known.action, TriggerAction::Highlight);
    }

    #[test]
    fn rule_defaults_and_validation() {
        // 旧文件缺字段 → serde(default) 补齐：动作默认通知、enabled 默认开
        let legacy: TriggerRule = toml::from_str("name = '错误'\npattern = 'ERROR'\n").unwrap();
        assert_eq!(legacy.action, TriggerAction::Notify);
        assert!(legacy.enabled);
        assert!(legacy.validate().is_ok());

        // 空名 / 空模式 / 超长模式 / NUL 一律拒绝（正则语法不在此校验）
        let mut rule = TriggerRule::default();
        assert!(rule.validate().is_err(), "空名应拒绝");
        rule.name = "规则".into();
        assert!(rule.validate().is_err(), "空模式应拒绝");
        rule.pattern = "x".repeat(2000);
        assert!(rule.validate().is_err(), "超长模式应拒绝");
        rule.pattern = "ER\0ROR".into();
        assert!(rule.validate().is_err(), "NUL 应拒绝");
        rule.pattern = "ERROR".into();
        assert!(rule.validate().is_ok());
        // 坏正则语法在这里是合法的（语法校验在 app 侧编译期）
        rule.pattern = "E(R?".into();
        assert!(rule.validate().is_ok());
    }
}
