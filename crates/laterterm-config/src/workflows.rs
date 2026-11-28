//! 工作流：独立于服务器的自动化编排实体（2026-11-28 设计修订——不再内嵌
//! ServerEntry，与命令/服务器/动态码平级，经**引用名**组合三类资源）。
//!
//! 形态：右侧面板第 4 页签列表 + CRUD + 运行。步骤四类动作：
//! - `connect`：连接服务器（服务器名引用），v1 至多一个且必须是第一步——
//!   运行 = 发起连接，连接成功后剩余步骤在新会话字节流上自动推进；
//! - `totp` / `text` / `command`：expect 命中（或等提示符锚点）后发送动态码
//!   /字面文本/快捷命令，引用名口径与命令/动态码面板一致——**真值不进
//!   TOML**（TOTP 密钥在密钥环、命令模板在命令面板）。
//!
//! 字节口径与运行引擎见 app 层 FlowRunner（剥 ANSI、边沿触发、单步超时
//! 回人工）。

use crate::commands::normalize_shortcut;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

/// 单条工作流允许的最大步骤数（同隧道封顶口径）。
pub const MAX_STEPS_PER_WORKFLOW: usize = 16;

/// 工作流单步默认超时秒数（超时放弃该步回人工，流继续）。
pub const DEFAULT_STEP_TIMEOUT_SECS: u64 = 30;

/// 工作流单步超时上下限秒数。
pub const STEP_TIMEOUT_RANGE: (u64, u64) = (1, 600);

/// expect 正则长度上限。
pub const MAX_EXPECT_LEN: usize = 256;

/// 工作流单步（`[[workflows.steps]]`，`kind` 拍平区分动作）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WorkflowStep {
    /// 连接服务器（服务器名引用）。v1 至多一个且必须是第一步：运行 =
    /// 发起连接（凭据解析/跳板链走服务器面板既有流程），连接成功后剩余
    /// 步骤在新会话上自动推进。
    Connect {
        /// 服务器条目名（服务器面板的唯一键）。
        server: String,
    },
    /// expect 命中后发送 TOTP 动态码（密钥环 totp_account 别名引用）。
    Totp {
        /// expect 正则；留空/缺省 = 等提示符锚点（OSC 133 A）。
        #[serde(default)]
        expect: Option<String>,
        /// TOTP 账户名（动态码面板条目名）。
        account: String,
    },
    /// expect 命中后发送字面文本（菜单序号等非敏感内容；自动补回车）。
    Text {
        /// expect 正则；留空/缺省 = 等提示符锚点。
        #[serde(default)]
        expect: Option<String>,
        /// 字面文本。
        text: String,
    },
    /// expect 命中后执行快捷命令（命令名引用，运行时展开模板；自动补回车）。
    Command {
        /// expect 正则；留空/缺省 = 等提示符锚点。
        #[serde(default)]
        expect: Option<String>,
        /// 快捷命令名（命令面板条目名）。
        name: String,
    },
}

impl WorkflowStep {
    /// 步骤的 expect 正则（None = 等提示符锚点）。
    pub fn expect(&self) -> Option<&str> {
        match self {
            WorkflowStep::Totp { expect, .. }
            | WorkflowStep::Text { expect, .. }
            | WorkflowStep::Command { expect, .. } => expect.as_deref(),
            WorkflowStep::Connect { .. } => None,
        }
    }

    /// 步骤的引用名（服务器/账户/文本/命令名；expect 不算引用）。
    pub fn reference(&self) -> &str {
        match self {
            WorkflowStep::Connect { server } => server,
            WorkflowStep::Totp { account, .. } => account,
            WorkflowStep::Text { text, .. } => text,
            WorkflowStep::Command { name, .. } => name,
        }
    }

    /// 引用名的可变访问（归一化写回用）。
    fn reference_mut(&mut self) -> &mut String {
        match self {
            WorkflowStep::Connect { server } => server,
            WorkflowStep::Totp { account, .. } => account,
            WorkflowStep::Text { text, .. } => text,
            WorkflowStep::Command { name, .. } => name,
        }
    }

    /// 步骤展示摘要（列表行一步到位看清整条流；超长截断）。
    pub fn summary(&self) -> String {
        let body = match self {
            WorkflowStep::Connect { server } => format!("连接 {server}"),
            WorkflowStep::Totp { account, .. } => format!("发动态码 {account}"),
            WorkflowStep::Text { text, .. } => format!("发送 {text}"),
            WorkflowStep::Command { name, .. } => format!("执行命令 {name}"),
        };
        body.chars().take(40).collect()
    }
}

/// 一条工作流（右侧面板「工作流」页签的条目）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowEntry {
    /// 展示名，列表唯一键。
    pub name: String,
    /// 备注（纯展示）。
    pub note: String,
    /// 应用内快捷键（运行该工作流）；统一注册表查重。
    #[serde(default)]
    pub shortcut: Option<String>,
    /// 单步超时秒数（超时放弃该步回人工，流继续等后续步）。
    pub step_timeout_secs: u64,
    /// 步骤序列（有序推进）。
    pub steps: Vec<WorkflowStep>,
}

impl Default for WorkflowEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            note: String::new(),
            shortcut: None,
            step_timeout_secs: DEFAULT_STEP_TIMEOUT_SECS,
            steps: Vec::new(),
        }
    }
}

impl WorkflowEntry {
    /// UI 保存前严格校验：名称/步数/超时上限、connect 位置（至多一个且
    /// 必须第一步）、expect 形状、引用名非空。引用**存在性**不在此查
    /// （跨面板引用，运行期解析失败 toast 指明，与命令模板引用语义一致）；
    /// 快捷键格式与跨类别查重由统一注册表负责。
    pub fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            bail!("工作流名称不能为空");
        }
        if self.steps.is_empty() {
            bail!("工作流至少需要一步");
        }
        if self.steps.len() > MAX_STEPS_PER_WORKFLOW {
            bail!("工作流步骤数超过上限（最多 {MAX_STEPS_PER_WORKFLOW} 步）");
        }
        if !(STEP_TIMEOUT_RANGE.0..=STEP_TIMEOUT_RANGE.1).contains(&self.step_timeout_secs) {
            bail!(
                "单步超时须在 {}..={} 秒",
                STEP_TIMEOUT_RANGE.0,
                STEP_TIMEOUT_RANGE.1
            );
        }
        // v1 运行口径：工作流运行 = 连接 + 自动跑，第一步必须是 connect
        //（纯步骤流没有字节流挂点，等 audit 级注入方案再放开）
        if !matches!(self.steps.first(), Some(WorkflowStep::Connect { .. })) {
            bail!("工作流第一步必须是「连接服务器」");
        }
        for (index, step) in self.steps.iter().enumerate() {
            if let Some(expect) = step.expect() {
                if expect.chars().any(|ch| ch.is_control()) {
                    bail!("第 {} 步 expect 不能包含控制字符", index + 1);
                }
                if expect.chars().count() > MAX_EXPECT_LEN {
                    bail!(
                        "第 {} 步 expect 过长（最多 {MAX_EXPECT_LEN} 字）",
                        index + 1
                    );
                }
            }
            match step {
                WorkflowStep::Connect { server } => {
                    if server.trim().is_empty() {
                        bail!("第 {} 步的服务器名不能为空", index + 1);
                    }
                }
                _ => {
                    if step.reference().trim().is_empty() {
                        bail!(
                            "第 {} 步的动作内容不能为空（动态码账户/文本/命令名）",
                            index + 1
                        );
                    }
                }
            }
        }
        if let Some(shortcut) = &self.shortcut {
            normalize_shortcut(shortcut).context("快捷键格式不合法")?;
        }
        Ok(())
    }

    /// 首步的 connect 目标（None = 纯步骤流，运行在焦点窗格上）。
    pub fn connect_server(&self) -> Option<&str> {
        match self.steps.first() {
            Some(WorkflowStep::Connect { server }) => Some(server),
            _ => None,
        }
    }
}

/// 值级归一化（幂等；载入路径挂载，同 sanitize_totp 口径——不可挽救的
/// 条目整条丢弃，坏步骤丢步骤）。trim 各字段；expect 空串降级 None；
/// 引用名 trim 后为空的步骤丢弃；超时钳制；归一化后无步骤的条目丢弃。
pub(crate) fn sanitize_workflow(entry: &mut WorkflowEntry) -> bool {
    entry.name = entry.name.trim().to_string();
    entry.note = entry.note.trim().to_string();
    entry.step_timeout_secs = entry
        .step_timeout_secs
        .clamp(STEP_TIMEOUT_RANGE.0, STEP_TIMEOUT_RANGE.1);
    entry.steps.retain_mut(|step| {
        let trimmed_expect = step
            .expect()
            .map(|expect| expect.trim().to_string())
            .filter(|expect| !expect.is_empty());
        match step {
            WorkflowStep::Totp { expect, .. }
            | WorkflowStep::Text { expect, .. }
            | WorkflowStep::Command { expect, .. } => *expect = trimmed_expect,
            WorkflowStep::Connect { .. } => {}
        }
        *step.reference_mut() = step.reference().trim().to_string();
        if step.reference().trim().is_empty() {
            eprintln!("工作流 {:?} 的空引用步骤已忽略", entry.name);
            return false;
        }
        true
    });
    !entry.name.is_empty() && !entry.steps.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totp_step(expect: Option<&str>, account: &str) -> WorkflowStep {
        WorkflowStep::Totp {
            expect: expect.map(str::to_owned),
            account: account.to_owned(),
        }
    }

    /// roundtrip：kind 拍平、expect 缺省补 None、connect 位置保真。
    #[test]
    fn workflow_roundtrip_keeps_kind_and_fields() {
        let raw = r#"
[[workflows]]
name = "跳板全流程"
note = "连堡垒机 + 动态码 + 进目录"
step_timeout_secs = 45

[[workflows.steps]]
kind = "connect"
server = "堡垒机"

[[workflows.steps]]
kind = "totp"
expect = "Opt token:"
account = "bastion"

[[workflows.steps]]
kind = "command"
name = "cd /data"
"#;
        let settings: crate::Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.workflows.len(), 1);
        let flow = &settings.workflows[0];
        assert_eq!(flow.name, "跳板全流程");
        assert_eq!(flow.step_timeout_secs, 45);
        assert_eq!(flow.connect_server(), Some("堡垒机"));
        assert_eq!(flow.steps[1], totp_step(Some("Opt token:"), "bastion"));
        assert_eq!(
            flow.steps[2],
            WorkflowStep::Command {
                expect: None,
                name: "cd /data".into(),
            }
        );
        // roundtrip：序列化再反序列化逐字段一致
        let dumped = toml::to_string(flow).unwrap();
        let reparsed: WorkflowEntry = toml::from_str(&dumped).unwrap();
        assert_eq!(reparsed, *flow);
        assert!(reparsed.validate().is_ok());
    }

    /// 严格校验：connect 位置/引用名非空/expect 控制字符/步数与超时上限。
    #[test]
    fn workflow_validate_rejects_bad_shapes() {
        let base = |steps: Vec<WorkflowStep>, timeout: u64| WorkflowEntry {
            name: "测试".into(),
            note: String::new(),
            shortcut: None,
            step_timeout_secs: timeout,
            steps,
        };
        // 首步不是 connect（v1 运行口径：第一步必须是连接服务器）
        assert!(base(vec![totp_step(None, "a")], 30).validate().is_err());
        // connect 服务器名空
        assert!(
            base(
                vec![WorkflowStep::Connect {
                    server: "  ".into()
                }],
                30
            )
            .validate()
            .is_err()
        );
        // 空步骤表
        assert!(base(vec![], 30).validate().is_err(), "空流应报错");
        // 空引用名
        assert!(
            base(
                vec![WorkflowStep::Command {
                    expect: None,
                    name: "  ".into()
                }],
                30
            )
            .validate()
            .is_err()
        );
        // expect 控制字符
        assert!(
            base(vec![totp_step(Some("bad\nexpect"), "a")], 30)
                .validate()
                .is_err()
        );
        // 超时越界
        assert!(base(vec![totp_step(None, "a")], 0).validate().is_err());
        // 步数上限
        let steps: Vec<WorkflowStep> = (0..=MAX_STEPS_PER_WORKFLOW)
            .map(|index| WorkflowStep::Text {
                expect: None,
                text: format!("{index}"),
            })
            .collect();
        assert!(base(steps, 30).validate().is_err());
        // 合法流通过（首步 connect + 后续 totp）
        assert!(
            base(
                vec![
                    WorkflowStep::Connect {
                        server: "堡垒机".into()
                    },
                    totp_step(None, "a")
                ],
                30
            )
            .validate()
            .is_ok()
        );
    }

    /// 归一化：trim、空 expect 降级 None、空引用步骤丢弃、timeout 钳制、
    /// 空名/无步骤条目整条丢弃；幂等。
    #[test]
    fn sanitize_workflow_trims_and_drops_bad_entries() {
        let mut entry = WorkflowEntry {
            name: "  测试  ".into(),
            note: " 备注 ".into(),
            shortcut: None,
            step_timeout_secs: 9999,
            steps: vec![
                totp_step(Some("  token  "), "  bastion  "),
                WorkflowStep::Command {
                    expect: None,
                    name: "".into(),
                },
            ],
        };
        assert!(sanitize_workflow(&mut entry));
        assert_eq!(entry.name, "测试");
        assert_eq!(entry.note, "备注");
        assert_eq!(entry.step_timeout_secs, STEP_TIMEOUT_RANGE.1);
        assert_eq!(entry.steps.len(), 1);
        assert_eq!(entry.steps[0].expect(), Some("token"));
        assert_eq!(entry.steps[0].reference(), "bastion");
        let once = entry.clone();
        assert!(sanitize_workflow(&mut entry));
        assert_eq!(entry, once, "幂等");

        // 全坏步骤 → 条目丢弃
        let mut entry = WorkflowEntry {
            steps: vec![WorkflowStep::Command {
                expect: None,
                name: "".into(),
            }],
            ..Default::default()
        };
        assert!(!sanitize_workflow(&mut entry), "空名+无步骤应整条丢弃");
    }
}
