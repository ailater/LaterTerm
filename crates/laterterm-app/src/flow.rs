//! 登录流 expect 引擎（FlowRunner）：有序状态机消费会话输出字节流，
//! 逐步匹配 expect、产出动作事件，实现「连跳板机 → 自动应答动态码 →
//! 自动跑命令」的会话层自动化（设计定稿 2026-11-25，方案 A 会话层）。
//!
//! 与审计触发器（audit.rs）的差异：audit 是无序通知（冷却/锁存吸收重复），
//! 本引擎**有序推进**——命中即步进、每步至多触发一次（边沿触发）。
//!
//! 设计定稿四个坑的对应处置：
//! - ANSI 打断匹配：expect 匹配在剥 ANSI 后的可见文本字节上（`strip_ansi_step`
//!   状态机跨块安全，CSI/OSC/DCS/SOS/PM/APC 整段丢弃）；
//! - 自回显重触发：命中即推进并截掉命中点之前的窗口，原 expect 不再可见，
//!   不会电平式重复触发；动作发送由外层经 flume 回主线程（与
//!   finish_ssh_connect 同模式，见 wrap_session / send_command_to_pane），
//!   引擎自身无副作用；
//! - 单步超时回人工：`tick` 由外层定时驱动，超时发 [`FlowEvent::StepTimeout`]
//!   放弃该步继续等后续步（toast 在接线层做），会话不断线；放弃时清空
//!   文本窗口——人工接管的旧输出不应误触发后续正则；
//! - 到达 shell 可靠信号：expect=None 的步骤等 OSC 133 A 提示符锚点（raw
//!   字节扫描——OSC 本体会被剥 ANSI 丢弃，必须先于剥除识别），本地/SSH 有
//!   shell 集成时提示符千变万化也不误判；JumpServer 网关无集成时用正则
//!   expect 主路径。
//!
//! 字节口径：tee 处 GBK 已转 UTF-8（wrap_session 注记），expect 按字节语义
//! 匹配（regex::bytes），ASCII 与 UTF-8 中文提示均可写；不在 ESC 语法内的
//! 字节（trzsz 二进制夹杂等）原样参与匹配。同一 chunk 内锚点先于文本处理
//! （锚点计数语义）——步骤间通常有秒级间隔，单 chunk 内的严格时序不承诺。

// D1 先落引擎本体，D4 才接线 wrap_session tee / 运行 chip——接线后收回
#![allow(dead_code)]

use regex::bytes::Regex;
use std::time::{Duration, Instant};

/// 单步动作（引用名口径：密钥/命令真值不进流配置，外层运行时解析——
/// TOTP 走密钥环别名、命令走快捷命令名展开，引擎只报意图不碰真值）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FlowAction {
    /// 发送 TOTP 动态码（密钥环 totp_account 别名引用）。
    Totp { account: String },
    /// 发送字面文本（如备用口令）。
    Text { text: String },
    /// 发送快捷命令（命令名引用，外层展开模板）。
    Command { name: String },
    /// 无条件等待 N 秒后推进下一步（无发送动作）——发送类步骤之后的
    /// 定时缓冲（如发完命令等服务端就绪）。
    Wait { secs: u64 },
}

/// 流步骤。`expect=None` 表示等下一个提示符锚点（OSC 133 A）而非正则。
pub(crate) struct FlowStep {
    pub expect: Option<String>,
    pub action: FlowAction,
}

/// 引擎产出的事件（feed/tick 返回值，外层经 flume 回主线程执行）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FlowEvent {
    /// 命中步骤：外层解析动作并发送（边沿触发，每步至多一次）。
    Action { step: usize, action: FlowAction },
    /// 单步超时：该步放弃回人工（toast 由外层发），流继续等后续步。
    StepTimeout { step: usize },
    /// 全部步骤执行完毕（含超时放弃后走到头）。
    Done,
}

/// 构造错误：expect 正则编译失败，`step` 为步序号。
#[derive(Debug)]
pub(crate) struct FlowInitError {
    pub step: usize,
    pub source: regex::Error,
}

/// 步骤推进门控：正则命中 / 提示符锚点 / 纯定时（Wait 步）。
enum StepGate {
    Regex(Regex),
    PromptAnchor,
    Timer(Duration),
}

/// 编译后的步骤。
struct CompiledStep {
    gate: StepGate,
    action: FlowAction,
}

/// 登录流 expect 引擎。字节流经 `feed` 喂入（会话 tee 的每个 chunk），
/// 超时由外层 `tick` 定时驱动；动作事件由外层经 flume 回主线程发送。
pub(crate) struct FlowRunner {
    steps: Vec<CompiledStep>,
    /// 当前等待的步骤序号；≥ steps.len() 即完成（done）。
    current: usize,
    done: bool,
    /// OSC 133 A 锚点跨块尾巴（保留至多锚点长度-1 字节）。
    anchor_carry: Vec<u8>,
    /// ANSI 剥离状态（跨块安全）。
    strip: StripState,
    /// 可见文本滚动窗口（regex 匹配域；命中/超时/锚点推进时截清）。
    text_window: Vec<u8>,
    /// 窗口扫止点（下一 chunk 从 scanned-重叠区起扫，补齐跨界匹配）。
    scanned: usize,
    step_timeout: Duration,
    /// 当前步 deadline（进入该步时设置；None 仅在 done 后）。
    step_deadline: Option<Instant>,
}

/// 文本窗口上限：超过截半保留后半（同 audit 扫描窗口口径，防长会话膨胀；
/// drop ≤ scanned 保证不截掉未扫数据，跨界匹配由重叠区补齐）。
const TEXT_WINDOW_LIMIT: usize = 64 * 1024;

/// 扫描重叠区：跨界命中的补齐余量（同 audit SCAN_OVERLAP 口径）。
const SCAN_OVERLAP: usize = 8 * 1024;

/// OSC 133 A（提示符开始）锚点：只认前缀，BEL/ST 终止符不作要求。
const PROMPT_ANCHOR: &[u8] = b"\x1b]133;A";

impl FlowRunner {
    /// 构造引擎。`now` 由调用方注入（首个步骤的 deadline 基准），测试可合成。
    pub(crate) fn new(
        steps: Vec<FlowStep>,
        step_timeout: Duration,
        now: Instant,
    ) -> Result<Self, FlowInitError> {
        let mut compiled = Vec::with_capacity(steps.len());
        for (index, step) in steps.into_iter().enumerate() {
            // Wait 步纯定时推进（不等锚点/正则）；其余按 expect 编译
            let gate = match (&step.action, step.expect) {
                (FlowAction::Wait { secs }, _) => StepGate::Timer(Duration::from_secs(*secs)),
                (_, None) => StepGate::PromptAnchor,
                (_, Some(pattern)) => {
                    // 登录提示词大小写不定（Password/PASSWORD/password），统一
                    // 大小写不敏感——expect 工具通行默认；高级用户可在模式内
                    // 用 (?-i) 关回敏感匹配
                    StepGate::Regex(Regex::new(&format!(r"(?i){pattern}")).map_err(|source| {
                        FlowInitError {
                            step: index,
                            source,
                        }
                    })?)
                }
            };
            compiled.push(CompiledStep {
                gate,
                action: step.action,
            });
        }
        let first_deadline = match compiled.first() {
            Some(CompiledStep {
                gate: StepGate::Timer(secs),
                ..
            }) => now + *secs,
            _ => now + step_timeout,
        };
        Ok(Self {
            steps: compiled,
            current: 0,
            done: false,
            anchor_carry: Vec::new(),
            strip: StripState::Ground,
            text_window: Vec::new(),
            scanned: 0,
            step_timeout,
            step_deadline: Some(first_deadline),
        })
    }

    /// 喂入会话输出 chunk，返回本 chunk 产出的有序事件。
    pub(crate) fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<FlowEvent> {
        let mut events = Vec::new();
        if self.done {
            return events;
        }
        // raw 锚点先于剥 ANSI 识别（OSC 本体会被剥掉）
        let mut anchors = scan_prompt_anchors(&mut self.anchor_carry, bytes);
        // 剥 ANSI → 可见文本进窗口
        let mut visible = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            strip_ansi_step(&mut self.strip, byte, &mut visible);
        }
        self.text_window.extend_from_slice(&visible);

        // 有序推进：timer 步等 tick，prompt 步消费锚点计数，regex 步扫文本窗口
        while !self.done {
            match &self.steps[self.current].gate {
                StepGate::Timer(_) => break,
                StepGate::PromptAnchor => {
                    if anchors == 0 {
                        break;
                    }
                    anchors -= 1;
                    let action = self.steps[self.current].action.clone();
                    events.push(FlowEvent::Action {
                        step: self.current,
                        action,
                    });
                    // 等 prompt 的步骤把此前的输出全部让位，旧文本不得误触发后续正则
                    self.text_window.clear();
                    self.scanned = 0;
                    self.advance(now, &mut events);
                }
                StepGate::Regex(regex) => {
                    let start = self.scanned.saturating_sub(SCAN_OVERLAP);
                    let Some(hit) = regex.find(&self.text_window[start..]) else {
                        self.scanned = self.text_window.len();
                        break;
                    };
                    let hit_end = start + hit.end();
                    let action = self.steps[self.current].action.clone();
                    events.push(FlowEvent::Action {
                        step: self.current,
                        action,
                    });
                    // 截到命中终点之后：原 expect 文本不再可见（防自回显重触发），
                    // 新步从回显内容之后继续匹配
                    self.text_window.drain(..hit_end);
                    self.scanned = 0;
                    self.advance(now, &mut events);
                }
            }
        }
        self.trim_window();
        events
    }

    /// 超时驱动（外层定时器）：普通步过期 → 放弃回人工继续后续步；
    /// Wait 步到期 → 等待完成，直接推进（非失败，无超时事件）。
    pub(crate) fn tick(&mut self, now: Instant) -> Vec<FlowEvent> {
        let mut events = Vec::new();
        if self.done {
            return events;
        }
        if let Some(deadline) = self.step_deadline
            && now >= deadline
        {
            let wait_completed = matches!(self.steps[self.current].gate, StepGate::Timer(_));
            if wait_completed {
                self.advance(now, &mut events);
            } else {
                events.push(FlowEvent::StepTimeout { step: self.current });
                // 人工接管：此前未匹配上的输出整体作废，防旧文本误触发后续正则
                self.text_window.clear();
                self.scanned = 0;
                self.advance(now, &mut events);
            }
        }
        events
    }

    /// 中止：后续 feed/tick 不再产出任何事件（会话保持，UI 由外层收 chip）。
    pub(crate) fn abort(&mut self) {
        self.done = true;
    }

    /// 流是否已结束（完成、中止或放弃到头）。
    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// 当前等待的步骤序号；None = 已结束（D4 运行 chip 用）。
    pub(crate) fn current_step(&self) -> Option<usize> {
        if self.done { None } else { Some(self.current) }
    }

    /// 推进到下一步并重置 deadline（Wait 步按其秒数、其余按单步超时）；
    /// 走到头发 Done。
    fn advance(&mut self, now: Instant, events: &mut Vec<FlowEvent>) {
        self.current += 1;
        if self.current >= self.steps.len() {
            self.done = true;
            self.step_deadline = None;
            events.push(FlowEvent::Done);
        } else {
            let deadline = match &self.steps[self.current].gate {
                StepGate::Timer(secs) => now + *secs,
                _ => now + self.step_timeout,
            };
            self.step_deadline = Some(deadline);
        }
    }

    /// 窗口超限截半（同 audit：drop ≤ scanned 不截未扫数据）。
    fn trim_window(&mut self) {
        if self.text_window.len() > TEXT_WINDOW_LIMIT {
            let drop = self.text_window.len() - TEXT_WINDOW_LIMIT / 2;
            self.text_window.drain(..drop);
            self.scanned = self.scanned.saturating_sub(drop);
        }
    }
}

/// ANSI 剥离单字节状态机（跨块安全）：Ground 输出可见字节，其余语法态整段
/// 丢弃。覆盖 CSI（`ESC [` 参数* 中间* 终字节）、OSC/DCS/SOS/PM/APC（BEL 或
/// ST `ESC \` 终止）、两字节 ESC 序列；非语法字节（含 \r\n 等控制符）原样
/// 保留，正则作者自行处理空白。
#[derive(Clone, Copy, PartialEq)]
enum StripState {
    Ground,
    Esc,
    Csi,
    /// OSC/DCS/SOS/PM/APC 串内。
    StringSeq,
    /// 串内收到 ESC，等 ST 第二字节。
    StringEsc,
}

fn strip_ansi_step(state: &mut StripState, byte: u8, out: &mut Vec<u8>) {
    match *state {
        StripState::Ground => match byte {
            0x1b => *state = StripState::Esc,
            _ => out.push(byte),
        },
        StripState::Esc => match byte {
            b'[' => *state = StripState::Csi,
            b']' | b'P' | b'X' | b'^' | b'_' => *state = StripState::StringSeq,
            // ESC 中间字节（nF/Fp 序列如 ESC ( B），继续等终字节
            0x20..=0x2f => {}
            // 两字节序列终字节：整段丢弃；CAN/SUB 等异常终止：丢 ESC 本身
            _ => *state = StripState::Ground,
        },
        StripState::Csi => match byte {
            0x40..=0x7e => *state = StripState::Ground,
            // 罕见：序列内嵌 ESC，重新起头
            0x1b => *state = StripState::Esc,
            _ => {} // 参数 0x30..=0x3f / 中间 0x20..=0x2f / 私有子参数
        },
        StripState::StringSeq => match byte {
            0x07 => *state = StripState::Ground, // BEL 终止
            0x1b => *state = StripState::StringEsc,
            _ => {} // 串内容整段丢弃
        },
        StripState::StringEsc => match byte {
            b'\\' => *state = StripState::Ground, // ST 完整终止
            0x1b => {}                            // 连续 ESC，继续等
            _ => {
                // 非 ST：新序列打断（如 OSC 中嵌 ESC [），按 Esc 语义重处理
                *state = StripState::Esc;
                strip_ansi_step(state, byte, out);
            }
        },
    }
}

/// raw 流上跨块扫描 OSC 133 A 锚点，返回本次 chunk 的锚点个数
/// （carry 保留至多锚点长-1 字节，任意拆分点可覆盖）。
fn scan_prompt_anchors(carry: &mut Vec<u8>, bytes: &[u8]) -> usize {
    let mut buf = std::mem::take(carry);
    buf.extend_from_slice(bytes);
    let mut count = 0usize;
    let mut search = 0usize;
    while let Some(pos) = buf[search..]
        .windows(PROMPT_ANCHOR.len())
        .position(|window| window == PROMPT_ANCHOR)
    {
        count += 1;
        search += pos + PROMPT_ANCHOR.len();
    }
    let keep = (PROMPT_ANCHOR.len() - 1).min(buf.len());
    *carry = buf[buf.len() - keep..].to_vec();
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(steps: Vec<(&str, FlowAction)>) -> FlowRunner {
        let t0 = Instant::now();
        let steps = steps
            .into_iter()
            .map(|(expect, action)| FlowStep {
                expect: Some(expect.to_owned()),
                action,
            })
            .collect();
        FlowRunner::new(steps, Duration::from_secs(30), t0).expect("正则应合法")
    }

    fn text_step(text: &str) -> (&str, FlowAction) {
        (text, FlowAction::Text { text: "x".into() })
    }

    /// 单步命中 → Action + Done；同文本回显再喂不再触发（边沿防自回显）。
    #[test]
    fn expect_hit_emits_action_once_and_ignores_echo() {
        let mut flow = runner(vec![text_step("Opt token:")]);
        let t0 = Instant::now();
        let events = flow.feed(b"Opt token: ", t0);
        assert_eq!(
            events,
            vec![
                FlowEvent::Action {
                    step: 0,
                    action: FlowAction::Text { text: "x".into() }
                },
                FlowEvent::Done,
            ]
        );
        assert!(flow.is_done());
        assert_eq!(flow.current_step(), None);
        // 自回显重放：已推进 + 窗口已截，原 expect 不再触发
        assert!(flow.feed(b"Opt token: ", t0).is_empty());
    }

    /// ANSI 颜色码夹杂不打断匹配（剥除后命中）。
    #[test]
    fn ansi_codes_do_not_break_match() {
        let mut flow = runner(vec![text_step("Opt token:")]);
        let events = flow.feed(b"\x1b[32mOpt \x1b[1mtoken:\x1b[0m \x1b[33m", Instant::now());
        assert_eq!(
            events[0],
            FlowEvent::Action {
                step: 0,
                action: FlowAction::Text { text: "x".into() }
            }
        );
    }

    /// CSI 序列跨块拆开（剥除器跨块状态）不产生可见杂字。
    #[test]
    fn csi_split_across_chunks_is_stripped() {
        let mut flow = runner(vec![text_step("token:")]);
        flow.feed(b"\x1b[3", Instant::now());
        let events = flow.feed(b"2mtoken: ", Instant::now());
        assert!(matches!(events[0], FlowEvent::Action { step: 0, .. }));
    }

    /// expect 文本跨 chunk 拆开：扫止点+重叠区补齐，第二块命中。
    #[test]
    fn match_across_chunks_via_window() {
        let mut flow = runner(vec![text_step("Opt token:")]);
        let t0 = Instant::now();
        assert!(flow.feed(b"Opt to", t0).is_empty());
        let events = flow.feed(b"ken: please", t0);
        assert!(matches!(events[0], FlowEvent::Action { step: 0, .. }));
    }

    /// UTF-8 中文提示跨块（含拆在多字节字符中间）：字节窗口语义下命中。
    #[test]
    fn utf8_cjk_pattern_across_chunks() {
        let pattern = "请输入动态口令:";
        let bytes = pattern.as_bytes();
        // 拆点落在「输」的 UTF-8 序列中间
        let mut flow = runner(vec![text_step(pattern)]);
        let t0 = Instant::now();
        assert!(flow.feed(&bytes[..7], t0).is_empty());
        let events = flow.feed(&bytes[7..], t0);
        assert!(matches!(events[0], FlowEvent::Action { step: 0, .. }));
    }

    /// expect=None 等提示符锚点：OSC 133 A 的 BEL/ST 两种终止均触发。
    #[test]
    fn prompt_anchor_triggers_wait_prompt_step() {
        let t0 = Instant::now();
        for terminator in [&b"\x1b]133;A\x07$ "[..], b"\x1b]133;A\x1b\\$ "] {
            let mut flow = FlowRunner::new(
                vec![FlowStep {
                    expect: None,
                    action: FlowAction::Command {
                        name: "cd /data".into(),
                    },
                }],
                Duration::from_secs(30),
                t0,
            )
            .expect("构造");
            let events = flow.feed(terminator, t0);
            assert_eq!(
                events,
                vec![
                    FlowEvent::Action {
                        step: 0,
                        action: FlowAction::Command {
                            name: "cd /data".into()
                        }
                    },
                    FlowEvent::Done,
                ]
            );
        }
    }

    /// 锚点跨块拆开仍可识别（carry 补齐）。
    #[test]
    fn prompt_anchor_split_across_chunks() {
        let t0 = Instant::now();
        let mut flow = FlowRunner::new(
            vec![FlowStep {
                expect: None,
                action: FlowAction::Text { text: "x".into() },
            }],
            Duration::from_secs(30),
            t0,
        )
        .expect("构造");
        assert!(flow.feed(b"\x1b]13", t0).is_empty());
        let events = flow.feed(b"3;A\x07", t0);
        assert!(matches!(events[0], FlowEvent::Action { step: 0, .. }));
    }

    /// 多步有序推进：事件序列与步骤顺序一致，各步动作原样透出。
    #[test]
    fn multi_step_sequential_actions() {
        let steps = vec![
            (
                "password:",
                FlowAction::Text {
                    text: "secret".into(),
                },
            ),
            (
                "MFA|OTP:",
                FlowAction::Totp {
                    account: "bastion".into(),
                },
            ),
            (
                r"\$$",
                FlowAction::Command {
                    name: "cd /data".into(),
                },
            ),
        ];
        let runner_steps: Vec<FlowStep> = steps
            .into_iter()
            .map(|(expect, action)| FlowStep {
                expect: Some(expect.to_owned()),
                action,
            })
            .collect();
        let mut flow =
            FlowRunner::new(runner_steps, Duration::from_secs(30), Instant::now()).expect("构造");
        let t0 = Instant::now();
        let mut events = flow.feed(b"Password: ", t0);
        events.extend(flow.feed(b"Opt MFA: ", t0));
        events.extend(flow.feed(b"user@host:~$", t0));
        assert_eq!(
            events,
            vec![
                FlowEvent::Action {
                    step: 0,
                    action: FlowAction::Text {
                        text: "secret".into()
                    }
                },
                FlowEvent::Action {
                    step: 1,
                    action: FlowAction::Totp {
                        account: "bastion".into()
                    }
                },
                FlowEvent::Action {
                    step: 2,
                    action: FlowAction::Command {
                        name: "cd /data".into()
                    }
                },
                FlowEvent::Done,
            ]
        );
    }

    /// 单步超时回人工：StepTimeout 放弃该步，流继续，后续步照常命中。
    #[test]
    fn step_timeout_falls_back_to_manual_and_flow_continues() {
        let timeout = Duration::from_secs(30);
        let t0 = Instant::now();
        let mut flow = FlowRunner::new(
            vec![
                FlowStep {
                    expect: Some("never-appears".into()),
                    action: FlowAction::Text { text: "x".into() },
                },
                FlowStep {
                    expect: Some("Opt token:".into()),
                    action: FlowAction::Totp {
                        account: "bastion".into(),
                    },
                },
            ],
            timeout,
            t0,
        )
        .expect("构造");
        // 超时前 tick 无事件
        assert!(
            flow.tick(t0 + timeout - Duration::from_millis(1))
                .is_empty()
        );
        let events = flow.tick(t0 + timeout);
        assert_eq!(events, vec![FlowEvent::StepTimeout { step: 0 }]);
        assert_eq!(flow.current_step(), Some(1));
        // 放弃步后流继续：后续步照常自动应答
        let events = flow.feed(b"Opt token: ", t0 + timeout);
        assert!(matches!(events[0], FlowEvent::Action { step: 1, .. }));
        assert!(events.last().is_some_and(|e| matches!(e, FlowEvent::Done)));
    }

    /// 超时回人工清窗口：旧文本（含已让步正则的内容）不得误触发后续步。
    #[test]
    fn timeout_clears_stale_text_window() {
        let timeout = Duration::from_secs(30);
        let t0 = Instant::now();
        let mut flow = FlowRunner::new(
            vec![
                FlowStep {
                    expect: Some("slow-prompt".into()),
                    action: FlowAction::Text { text: "x".into() },
                },
                FlowStep {
                    expect: Some("slow-prompt".into()),
                    action: FlowAction::Text { text: "y".into() },
                },
            ],
            timeout,
            t0,
        )
        .expect("构造");
        flow.feed(b"slow-prompt", t0); // 老步超时前留下的可见文本
        flow.tick(t0 + timeout);
        // 步骤 1 的正则与步骤 0 相同：若超时未清窗口，旧文本会立即误触发
        let events = flow.feed(b"tail", t0 + timeout);
        assert!(events.is_empty(), "旧文本不得误触发: {events:?}");
    }

    /// 长输出窗口截半后尾部命中不受影响。
    #[test]
    fn long_output_trim_keeps_tail_matching() {
        let mut flow = runner(vec![text_step("Opt token:")]);
        let t0 = Instant::now();
        let noise = vec![b'a'; 100 * 1024];
        assert!(flow.feed(&noise, t0).is_empty());
        let events = flow.feed(b"Opt token: ", t0);
        assert!(matches!(events[0], FlowEvent::Action { step: 0, .. }));
    }

    /// abort 后引擎静默（会话保持，不再产出事件）。
    #[test]
    fn abort_stops_engine() {
        let mut flow = runner(vec![text_step("token:")]);
        flow.abort();
        assert!(flow.is_done());
        assert!(flow.feed(b"token: ", Instant::now()).is_empty());
        assert!(flow.tick(Instant::now()).is_empty());
    }

    /// Wait 步纯定时推进：不等锚点/正则，到时直接进入下一步（无超时
    /// 事件、无动作发送）；首步为 Wait 时 deadline 按其秒数。
    #[test]
    fn wait_step_advances_by_timer_without_action() {
        let t0 = Instant::now();
        let mut flow = FlowRunner::new(
            vec![
                FlowStep {
                    expect: None,
                    action: FlowAction::Wait { secs: 5 },
                },
                FlowStep {
                    expect: Some("token:".into()),
                    action: FlowAction::Totp {
                        account: "bastion".into(),
                    },
                },
            ],
            Duration::from_secs(30),
            t0,
        )
        .expect("构造");
        // 5s 内输出再喂也不推进（等定时，不等锚点）
        assert!(
            flow.feed(b"\x1b]133;A\x07token: ", t0 + Duration::from_secs(1))
                .is_empty()
        );
        assert!(flow.tick(t0 + Duration::from_secs(1)).is_empty());
        // 5s 到 → 推进到 totp 步（无 StepTimeout、无 Action）
        let events = flow.tick(t0 + Duration::from_secs(5));
        assert!(events.is_empty(), "等待完成不是超时: {events:?}");
        assert_eq!(flow.current_step(), Some(1));
        // 后续步照常正则命中
        let events = flow.feed(b"token: ", t0 + Duration::from_secs(6));
        assert!(matches!(events[0], FlowEvent::Action { step: 1, .. }));
        assert!(events.last().is_some_and(|e| matches!(e, FlowEvent::Done)));
    }

    /// 非法正则报错并指明步序号。
    #[test]
    fn invalid_regex_reports_step() {
        let error = FlowRunner::new(
            vec![
                FlowStep {
                    expect: Some("(valid)".into()),
                    action: FlowAction::Text { text: "x".into() },
                },
                FlowStep {
                    expect: Some("([bad".into()),
                    action: FlowAction::Text { text: "x".into() },
                },
            ],
            Duration::from_secs(30),
            Instant::now(),
        )
        .err()
        .expect("应报错");
        assert_eq!(error.step, 1);
    }
}
