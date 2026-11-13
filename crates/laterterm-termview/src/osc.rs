//! OSC 旁路解析底座（cwd / notify / 提示符标记共用）与 OSC 133 提示符跟踪。
//!
//! # 为什么需要旁路解析器
//!
//! vte 0.15（经 `alacritty_terminal::vte` re-export，Cargo.lock 钉死）的
//! `ansi::Handler::osc_dispatch` 只路由内置 OSC 0/2/4/8/10/11/12/22/50/52/
//! 104/110/111/112；本仓库要用的 OSC 7（cwd）/ 9·777（notify）/ 133（提示符
//! 标记）全部落入 `unhandled`（仅 debug 日志），`Handler` trait 无任何挂钩。
//! 因此同一字节流喂两台**独立**的 vte 状态机：主 [`Term`](alacritty_terminal::
//! term::Term)（经 `ansi::Processor`）照常渲染，[`OscSink`] 只认 OSC——
//! 双状态机互不干扰，零新增依赖、不 fork 上游。
//!
//! # OSC 133 生命周期（FinalTerm 约定）
//!
//! ```text
//! 133;A（提示符开始）→ 133;B（提示符结束/输入行开始）→ 133;C（命令开始）
//!   → 命令输出 …→ 133;D;<exit>（命令结束）
//! ```
//!
//! v1 发起端只注入 A/C/D（bash 经 `PROMPT_COMMAND`/`PS0` 环境钩子，见
//! laterterm-local `shell_integration_env`）；B 仅解析不入锚点表（多行提示符
//! 时锚点取 A 行足够跳转定位）。
//!
//! # 行号锚定与滚动淘汰
//!
//! 标记在受理瞬间定格为「历史 + 屏幕」自顶向下的绝对行号（0 = 最老一行，
//! 与 [`SearchMatch`](crate::terminal::SearchMatch)::line 同口径）：
//! `total_lines() - 1 - (bottommost_line() - cursor.line)`。
//!
//! 淘汰修正依赖滚动计数：喂入前后 `topmost_line()`（= `Line(-history)`）差值
//! 累进 [`PromptTracker`]。**已知边界**：滚动缓冲打满后 alacritty 环形复用行，
//! 挤出量对外不可观测（`Term` 无滚动计数外泄），修正退化为下界——最老标记
//! 可能滞留、跳转可能偏下；预填充期（默认 10 万行缓冲覆盖绝大多数会话期）
//! 逐行精确。resize/reflow 使绝对行号整体失效，走整体作废而非重映射
//! （见 `TerminalState::invalidate_prompt_marks`）。

use std::collections::VecDeque;

use alacritty_terminal::vte::{Parser, Perform};

/// 旁路解析产出的事件。cwd（OSC 7）与 notify（OSC 9/777）子项共用同一枚举，
/// 接口一步定稳：后续子项只增消费端，不改变体形态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 133;A —— 提示符开始（提示符文本输出之前）。
    PromptStart,
    /// OSC 133;B —— 提示符结束 / 命令输入行开始。v1 仅解析，锚点表只收 A。
    CommandInputStart,
    /// OSC 133;C —— 命令开始（用户回车后、命令输出之前）。
    CommandStart,
    /// OSC 133;D[;code] —— 命令结束；`exit_code` 为 None 表示未携带或不可解析。
    CommandFinished { exit_code: Option<i32> },
    /// OSC 7 —— cwd 上报。`path` 为校验（只认 `file://` 协议，host 段任意）
    /// 并 percent 解码后的绝对路径；非法输入（非 file://、超长、解码后非
    /// UTF-8）不产事件。
    Cwd { path: String },
    /// OSC 9;body —— 桌面通知（iTerm 风格，无标题段）。
    Notify { body: String },
    /// OSC 777;notify;title;body —— 桌面通知（rxvt/URxvt 风格）。
    NotifyWithTitle { title: String, body: String },
    /// OSC 9;4 —— ConEmu 进度上报（先判首参数 = 4 再与 9;body 分流）。
    /// 值内嵌在 [`ProgressState`]（Set 携带百分比）。
    Progress(ProgressState),
}

/// ConEmu 进度状态（OSC 9;4 的第二/第三参数归一结果）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    /// 移除进度指示（state=0）。
    Remove,
    /// 设置进度（state=1；0 = 无进度，1-100 常规值）。
    Set { percent: u8 },
    /// 出错（state=2）。
    Error,
    /// 不定（state=3，进行中但无法量化）。
    Indeterminate,
}

/// 提示符锚点（OSC 133;A 受理时定格）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptMark {
    /// 「历史 + 屏幕」绝对行号（0 = 最老一行，与 SearchMatch.line 同口径）。
    /// 查询期按受理后滚动淘汰量修正（见模块注记的已知边界）。
    pub abs_line: usize,
    /// 受理时的 `total_lines()`（淘汰修正与诊断基准）。
    pub total_at_receipt: usize,
    /// 该提示符之后命令的退出码：D 到达回填；None = 运行中或未知（孤儿命令）。
    pub exit_code: Option<i32>,
}

/// 提示符跳转方向（[`TerminalState::jump_prompt`](crate::terminal::
/// TerminalState::jump_prompt) 用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDirection {
    /// 上一条提示符（视口顶行之上最近的标记）。
    Previous,
    /// 下一条提示符（视口底行之下最近的标记）。
    Next,
}

/// 事件字符串长度上限（字符数）。OSC 内容是应用输出 = 不可信输入，
/// 解析端先截断，防病态超长序列撑内存；cwd/notify 子项再做各自校验。
const OSC_TEXT_MAX_CHARS: usize = 4096;

/// `&[u8]` → 截断后的有损 UTF-8 字符串（先按 UTF-8 有损解码，再按字符截断，
/// 不劈开多字节字符；vte 内部对 OSC 原文已有缓冲上限，此处再封一道）。
fn capped_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(OSC_TEXT_MAX_CHARS)
        .collect()
}

/// 解析 133;D 的退出码参数：十进制、非负、i32 可表示；其余视为未知（None）。
fn parse_exit_code(raw: &[u8]) -> Option<i32> {
    let text = std::str::from_utf8(raw).ok()?;
    text.trim()
        .parse::<i64>()
        .ok()
        .filter(|code| (0..=i32::MAX as i64).contains(code))
        .map(|code| code as i32)
}

/// OSC 7 上报 URI 的字节上限：合法路径远低于此，超长视为病态输入**拒绝**
/// 而非截断——截断会产生一个「看似合法」的错误路径。
const OSC7_MAX_URI_BYTES: usize = 1024;

/// 解析 OSC 7 参数 → 本地绝对路径。
///
/// 只认 `file://host/path` 形态（host 段任意：bash 钩子发 `file://主机名/路径`
/// 或 `file:///路径`；scheme 按 RFC 3986 大小写不敏感）。路径段 percent 解码
/// （`+` 不算空格——那是表单编码约定）；超长（[`OSC7_MAX_URI_BYTES`]）与
/// 解码后非 UTF-8 一律拒绝；畸形 `%` 转义按字面保留（宽容口径：错误路径
/// 好过静默丢 cwd）。OSC 7 是不可信输入，全部校验在本函数收口。
fn parse_osc7_cwd(raw: &[u8]) -> Option<String> {
    if raw.len() > OSC7_MAX_URI_BYTES {
        return None;
    }
    let uri = std::str::from_utf8(raw).ok()?;
    if !uri
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case("file://"))
    {
        return None;
    }
    // host 段到下一个 '/' 为止；`file://host` 无路径部分（rest 不含 '/'）
    // 属非法形态，`file:///path` 的空 host 合法。切片自 '/' 起，路径必以
    // '/' 开头（绝对路径）
    let rest = &uri[7..];
    let slash = rest.find('/')?;
    percent_decode(&rest[slash..])
}

/// 解析 ConEmu 进度参数（OSC 9;4 的第三/第四参数）→ [`ProgressState`]。
///
/// `state_raw` ∈ {0,1,2,3}，其余视为非法 → None（调用方静默忽略 = 非法
/// 回退：进度保持上一次的值，不被垃圾序列清掉）。state=1(Set) 必须携带
/// 数字 progress：0 = 无进度，1-100 常规；缺失/非数字/>100 都算非法 →
/// None（不做钳制——钳制会把越界垃圾「扶正」成看似合法的 100%）。
/// 其余状态的 progress 参数忽略。
fn parse_progress(state_raw: &[u8], progress_raw: Option<&[u8]>) -> Option<ProgressState> {
    let state = match state_raw {
        b"0" => ProgressState::Remove,
        b"1" => {
            let percent = progress_raw
                .and_then(|raw| std::str::from_utf8(raw).ok())
                .and_then(|text| text.trim().parse::<u8>().ok())
                .filter(|value| *value <= 100)?;
            ProgressState::Set { percent }
        }
        b"2" => ProgressState::Error,
        b"3" => ProgressState::Indeterminate,
        _ => return None,
    };
    Some(state)
}

/// percent 解码：`%XX` 十六进制字节对；畸形转义按字面保留。解码结果必须
/// 是合法 UTF-8（cwd 要进 PathBuf 与展示，非 UTF-8 路径 v1 不支持）。
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 十六进制位取值（大小写均可）。
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// OSC 事件分发器：实现 [`Perform`] 但**只**挂 `osc_dispatch`，其余动作
/// （打印/CSI/ESC 等）全部走默认空实现——主屏渲染完全由主 `Term` 负责。
struct OscDispatch<'a> {
    events: &'a mut VecDeque<OscEvent>,
}

impl Perform for OscDispatch<'_> {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(head) = params.first() else {
            return;
        };
        match *head {
            b"133" => self.dispatch_133(params),
            b"7" => {
                // 形态：OSC 7 ; file://host/path ST —— 只认 file 协议（host
                // 段任意），percent 解码取绝对路径；其余协议与非法输入一律
                // 忽略（不可信输入，校验收口在 parse_osc7_cwd）。
                if let Some(path) = params.get(1).and_then(|raw| parse_osc7_cwd(raw)) {
                    self.events.push_back(OscEvent::Cwd { path });
                }
            }
            b"9" => {
                // 双义分流：首参数 = 4 → ConEmu 进度（OSC 9;4;state;progress），
                // 否则 → iTerm 通知（OSC 9;body）。这是两种协议复用同一 OSC
                // 号，按「首参数 = 4 即进度」严格判定：进入进度分支后解析
                // 缺段/非法一律整条丢弃（不回落消息通道，取舍见单测注记）。
                if params.get(1).is_some_and(|sub| *sub == b"4") {
                    if let Some(state_raw) = params.get(2)
                        && let Some(state) = parse_progress(state_raw, params.get(3).copied())
                    {
                        self.events.push_back(OscEvent::Progress(state));
                    }
                } else if let Some(body) = params.get(1) {
                    let body = capped_text(body);
                    self.events.push_back(OscEvent::Notify { body });
                }
            }
            b"777" => {
                // 形态：OSC 777 ; notify ; title ; body ST（rxvt/URxvt）；
                // 777 还承载其它子命令（如 run），非 notify 一律忽略
                if params
                    .get(1)
                    .is_some_and(|kind| kind.eq_ignore_ascii_case(b"notify"))
                {
                    let title = params.get(2).map(|t| capped_text(t)).unwrap_or_default();
                    let body = params.get(3).map(|b| capped_text(b)).unwrap_or_default();
                    self.events
                        .push_back(OscEvent::NotifyWithTitle { title, body });
                }
            }
            _ => {} // 其余 OSC 与旁路无关（内置项由主 Term 的 Handler 处理）
        }
    }
}

impl OscDispatch<'_> {
    /// OSC 133 参数形态：`["133", "A"|"B"|"C"]` / `["133", "D"]` /
    /// `["133", "D", code]`。孤儿 C（zsh 只发 C 不发 A 的集成）由
    /// [`PromptTracker`] 侧容忍——C 不入锚点表。
    fn dispatch_133(&mut self, params: &[&[u8]]) {
        let Some(sub) = params.get(1) else {
            return;
        };
        let event = match sub.first() {
            Some(b'A') => OscEvent::PromptStart,
            Some(b'B') => OscEvent::CommandInputStart,
            Some(b'C') => OscEvent::CommandStart,
            Some(b'D') => OscEvent::CommandFinished {
                exit_code: params.get(2).and_then(|raw| parse_exit_code(raw)),
            },
            _ => return, // 未知子码（如 133;E）：静默忽略，不污染事件流
        };
        self.events.push_back(event);
    }
}

/// 旁路 OSC 解析器：持独立 [`Parser`]，把识别出的 OSC 事件攒进内部队列，
/// 由 `TerminalState::process_bytes` 在 term 锁内取走（见 terminal.rs）。
pub(crate) struct OscSink {
    parser: Parser,
    events: VecDeque<OscEvent>,
}

impl OscSink {
    pub(crate) fn new() -> Self {
        Self {
            parser: Parser::new(),
            events: VecDeque::new(),
        }
    }

    /// 喂入一段 PTY 字节（与主 `Term` 同一块缓冲，先后各喂一次）。
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        let mut dispatch = OscDispatch {
            events: &mut self.events,
        };
        self.parser.advance(&mut dispatch, bytes);
    }

    /// 取走自上次以来积攒的全部事件。
    pub(crate) fn take_events(&mut self) -> VecDeque<OscEvent> {
        std::mem::take(&mut self.events)
    }
}

/// 提示符标记表 + 滚动淘汰计数 + 命令运行窗态。
///
/// 挂在独立 `Mutex<PromptTracker>`（TerminalState 字段）而非 Term 互斥体内：
/// 锁序固定 **term → tracker**（process_bytes 持 term 锁期间末段取本锁；
/// resize 作废路径只取本锁），杜绝与渲染/读侧的交叉加锁死锁。
pub(crate) struct PromptTracker {
    /// A 标记（按受理先后 = 行号升序）。容量封顶防长会话无界增长。
    marks: VecDeque<PromptMark>,
    /// 运行中窗态：C 与 D 之间为 true（v1 只暴露数据，不驱动 UI）。
    running: bool,
    /// 最近一条已结束命令是否失败（D exit≠0 置位，exit=0 清零）。
    last_failed: bool,
    /// 受理后累计的滚动淘汰行数（查询期把标记的绝对行号向下修正）。
    /// 见模块注记：预填充期逐行精确，缓冲打满后为下界。
    evicted: u64,
}

impl PromptTracker {
    /// 标记表容量：按「最近 256 条命令」封顶，最老的先淘汰
    /// （跳转 UX 与失败指示都只关心近期命令）。
    const MAX_MARKS: usize = 256;

    pub(crate) fn new() -> Self {
        Self {
            marks: VecDeque::new(),
            running: false,
            last_failed: false,
            evicted: 0,
        }
    }

    /// 累计滚动淘汰量（process_bytes 每步调用，量为 0 时可省）。
    pub(crate) fn note_scroll(&mut self, evicted_delta: u64) {
        self.evicted += evicted_delta;
    }

    /// 受理 A：追加提示符锚点。隐式关闭上一命令的运行窗（孤儿 C 容忍：
    /// 命令没发 D 就回到提示符，运行态就此收束）。
    pub(crate) fn push_prompt(&mut self, abs_line: usize, total_at_receipt: usize) {
        self.running = false;
        self.marks.push_back(PromptMark {
            abs_line,
            total_at_receipt,
            exit_code: None,
        });
        while self.marks.len() > Self::MAX_MARKS {
            self.marks.pop_front();
        }
    }

    /// 受理 C：进入运行窗。ALT_SCREEN 期间冻结（`alt_screen=true` 时不置位，
    /// 全屏应用的 C 是噪音）。
    pub(crate) fn note_command_start(&mut self, alt_screen: bool) {
        if alt_screen {
            return;
        }
        self.running = true;
    }

    /// 受理 D：收束运行窗 + 失败判定 + 退出码回填最近一条未关闭标记。
    /// ALT_SCREEN 期间照常执行（D 的失败判定不依赖行号，属冻结例外）。
    pub(crate) fn note_command_finished(&mut self, exit_code: Option<i32>) {
        self.running = false;
        if let Some(code) = exit_code {
            self.last_failed = code != 0;
        }
        if let Some(mark) = self
            .marks
            .iter_mut()
            .rev()
            .find(|mark| mark.exit_code.is_none())
        {
            mark.exit_code = exit_code;
        }
    }

    /// 作废全部标记并重置滚动计数基准（resize/reflow 后绝对行号整体失效）。
    /// 运行窗与失败态保留：命令事实未变，红点指示不应因窗口缩放丢失。
    pub(crate) fn invalidate(&mut self) {
        self.marks.clear();
        self.evicted = 0;
    }

    /// 淘汰修正后的标记快照（升序）。出界标记**真删**（retain），防表内
    /// 无限滞留。修正口径：`abs_line - evicted`，落到 `>= total_now` 即丢弃。
    pub(crate) fn visible_marks(&mut self, total_now: usize) -> Vec<PromptMark> {
        let evicted = self.evicted;
        self.marks
            .retain(|mark| (mark.abs_line as u64).saturating_sub(evicted) < total_now as u64);
        self.marks
            .iter()
            .map(|mark| PromptMark {
                abs_line: (mark.abs_line as u64).saturating_sub(evicted) as usize,
                ..*mark
            })
            .collect()
    }

    /// 最近一条已结束命令是否失败（OSC 133;D exit≠0）。
    pub(crate) fn last_command_failed(&self) -> bool {
        self.last_failed
    }

    /// 当前是否有命令运行中（C 与 D 之间的窗口态）。
    pub(crate) fn command_running(&self) -> bool {
        self.running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 便捷喂入：返回 sink 积攒的事件。
    fn feed(sink: &mut OscSink, bytes: &[u8]) -> Vec<OscEvent> {
        sink.feed(bytes);
        sink.take_events().into_iter().collect()
    }

    // ───────────── 133 四类标记解析 ─────────────

    #[test]
    fn osc133_four_marks_parse_with_bel_and_st_terminators() {
        let mut sink = OscSink::new();
        // BEL 结尾（我们注入的钩子形态）
        assert_eq!(
            feed(&mut sink, b"\x1b]133;A\x07"),
            vec![OscEvent::PromptStart]
        );
        // ST 结尾（kitty 式集成形态）
        assert_eq!(
            feed(&mut sink, b"\x1b]133;B\x1b\\"),
            vec![OscEvent::CommandInputStart]
        );
        assert_eq!(
            feed(&mut sink, b"\x1b]133;C\x07"),
            vec![OscEvent::CommandStart]
        );
        assert_eq!(
            feed(&mut sink, b"\x1b]133;D;127\x07"),
            vec![OscEvent::CommandFinished {
                exit_code: Some(127)
            }]
        );
        // D 不带退出码（部分集成如此）→ None，不是解析失败
        assert_eq!(
            feed(&mut sink, b"\x1b]133;D\x07"),
            vec![OscEvent::CommandFinished { exit_code: None }]
        );
        // 跨 chunk 撕裂的序列照常解析（vte 状态机跨 feed 保持）
        sink.feed(b"\x1b]133;");
        assert_eq!(
            feed(&mut sink, b"D;2\x07"),
            vec![OscEvent::CommandFinished { exit_code: Some(2) }]
        );
    }

    #[test]
    fn osc133_malformed_and_unknown_are_ignored() {
        let mut sink = OscSink::new();
        // 裸 133 / 未知子码 / 越界退出码 / 非数字退出码：一律不产事件
        assert!(feed(&mut sink, b"\x1b]133\x07").is_empty());
        assert!(feed(&mut sink, b"\x1b]133;E\x07").is_empty());
        assert_eq!(
            feed(&mut sink, b"\x1b]133;D;-1\x07"),
            vec![OscEvent::CommandFinished { exit_code: None }]
        );
        assert_eq!(
            feed(&mut sink, b"\x1b]133;D;abc\x07"),
            vec![OscEvent::CommandFinished { exit_code: None }]
        );
        // 超过 i32 上限视为未知（防御集成端乱传）
        assert_eq!(
            feed(&mut sink, b"\x1b]133;D;99999999999\x07"),
            vec![OscEvent::CommandFinished { exit_code: None }]
        );
    }

    // ───────────── OSC 7 cwd（file:// 校验 + percent 解码） ─────────────

    #[test]
    fn osc7_cwd_parses_file_uri_with_and_without_host() {
        let mut sink = OscSink::new();
        // 带 host 段（bash 钩子形态之一：file://主机名/路径）
        assert_eq!(
            feed(&mut sink, b"\x1b]7;file://myhost/home/u\x07"),
            vec![OscEvent::Cwd {
                path: "/home/u".into()
            }]
        );
        // 无 host（file:/// 起始，路径直接跟空 host）
        assert_eq!(
            feed(&mut sink, b"\x1b]7;file:///home/u\x07"),
            vec![OscEvent::Cwd {
                path: "/home/u".into()
            }]
        );
        // scheme 大小写不敏感（RFC 3986）
        assert_eq!(
            feed(&mut sink, b"\x1b]7;FILE:///srv\x07"),
            vec![OscEvent::Cwd {
                path: "/srv".into()
            }]
        );
    }

    #[test]
    fn osc7_cwd_percent_decodes_utf8_and_spaces() {
        let mut sink = OscSink::new();
        // 中文 + 空格路径（bash 钩子对 $PWD 原样上报，含空格/中文的目录
        // 由发起端 percent 编码）
        assert_eq!(
            feed(
                &mut sink,
                b"\x1b]7;file:///home/u/%E4%B8%AD%E6%96%87%20dir\x07"
            ),
            vec![OscEvent::Cwd {
                path: "/home/u/中文 dir".into()
            }]
        );
        // 大写十六进制位同样可解（%2F 解码为 /，路径含双斜杠是合法 POSIX 路径）
        assert_eq!(
            feed(&mut sink, b"\x1b]7;file:///p/%2Fx\x07").pop(),
            Some(OscEvent::Cwd {
                path: "/p//x".into()
            })
        );
        // 畸形转义按字面保留（宽容口径：% 后非十六进制不成转义）
        assert_eq!(
            feed(&mut sink, b"\x1b]7;file:///a%zzb\x07").pop(),
            Some(OscEvent::Cwd {
                path: "/a%zzb".into()
            })
        );
    }

    #[test]
    fn osc7_cwd_rejects_non_file_overlong_and_invalid_utf8() {
        let mut sink = OscSink::new();
        // 非 file 协议一律拒绝（OSC 7 是不可信输入）
        for uri in [
            "kitty://x",
            "https://example.com/",
            "ftp:///pub",
            "file:/no-authority/path",
            "file://host-no-path",
            "",
        ] {
            assert!(
                feed(&mut sink, format!("\x1b]7;{uri}\x07").as_bytes()).is_empty(),
                "{uri} 不应产事件"
            );
        }
        // 超长（>1KB）拒绝——不是截断，截断会产生看似合法的错误路径
        let long_path = format!("file:///{}/x", "d".repeat(OSC7_MAX_URI_BYTES));
        assert!(feed(&mut sink, format!("\x1b]7;{long_path}\x07").as_bytes()).is_empty());
        // 恰好 1KB 上界内仍接受（需完整 OSC 序列含 BEL 终止符）
        let ok_path = format!("file:///{}/x", "d".repeat(OSC7_MAX_URI_BYTES - 16));
        assert_eq!(
            feed(&mut sink, format!("\x1b]7;{ok_path}\x07").as_bytes()).len(),
            1
        );
        // percent 解码出非法 UTF-8（%FF）拒绝
        assert!(feed(&mut sink, b"\x1b]7;file:///\xff\x07").is_empty());
        assert!(feed(&mut sink, b"\x1b]7;file:///%FF\x07").is_empty());
    }

    // ───────────── notify 底座接口（后续子项消费） ─────────────

    #[test]
    fn osc_notify_events_parse() {
        let mut sink = OscSink::new();
        assert_eq!(
            feed(&mut sink, "\x1b]9;构建完成\x07".as_bytes()),
            vec![OscEvent::Notify {
                body: "构建完成".into()
            }]
        );
        assert_eq!(
            feed(&mut sink, "\x1b]777;notify;标题;内容\x07".as_bytes()),
            vec![OscEvent::NotifyWithTitle {
                title: "标题".into(),
                body: "内容".into()
            }]
        );
        // 777 的非 notify 子命令忽略；缺段的 notify 用空串补齐不 panic
        assert!(feed(&mut sink, b"\x1b]777;run;ls\x07").is_empty());
        assert_eq!(
            feed(&mut sink, b"\x1b]777;notify;t\x07"),
            vec![OscEvent::NotifyWithTitle {
                title: "t".into(),
                body: String::new()
            }]
        );
    }

    // ───────────── OSC 9 双义（消息 vs 9;4 进度）与 ConEmu 进度状态机 ─────────────

    #[test]
    fn osc9_disambiguates_progress_prefix_from_message() {
        let mut sink = OscSink::new();
        // 9;4;state;progress → ConEmu 进度
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;1;50\x07"),
            vec![OscEvent::Progress(ProgressState::Set { percent: 50 })]
        );
        // 裸 9;4（无 state/progress 段）→ 首参数 = 4 已按进度分流、解析缺段
        // 失败 → 整条丢弃。取舍注记：严格按「首参数 = 4 即进度」判（见
        // dispatch 注释），不为残缺序列回落消息通道——把控制序列当通知
        // 弹给用户的代价高于静默丢一条本就畸形的输入
        assert!(feed(&mut sink, b"\x1b]9;4\x07").is_empty());
        // 以「4」开头的普通消息不受分流影响
        assert_eq!(
            feed(
                &mut sink,
                b"\x1b]9;4 \xe5\x88\x86\xe9\x92\x9f\xe5\x90\x8e\xe5\xae\x8c\xe6\x88\x90\x07"
            ),
            vec![OscEvent::Notify {
                body: "4 分钟后完成".into()
            }]
        );
        // state 越界（ConEmu 的 4=paused 本版未收录）→ 整条忽略（非法回退）
        assert!(feed(&mut sink, b"\x1b]9;4;4;50\x07").is_empty());
        // state=1 缺 progress 段 → 忽略（Set 必须带数字）
        assert!(feed(&mut sink, b"\x1b]9;4;1\x07").is_empty());
        // 出错态忽略 progress 段（只有 Set 携带值）
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;2;99\x07"),
            vec![OscEvent::Progress(ProgressState::Error)]
        );
    }

    #[test]
    fn osc9_progress_state_machine() {
        let mut sink = OscSink::new();
        // 0 移除 / 3 不定 / 2 出错
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;0;0\x07"),
            vec![OscEvent::Progress(ProgressState::Remove)]
        );
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;3\x07"),
            vec![OscEvent::Progress(ProgressState::Indeterminate)]
        );
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;2\x07"),
            vec![OscEvent::Progress(ProgressState::Error)]
        );
        // Set 边界：0 = 无进度、100 = 满
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;1;0\x07"),
            vec![OscEvent::Progress(ProgressState::Set { percent: 0 })]
        );
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;1;100\x07"),
            vec![OscEvent::Progress(ProgressState::Set { percent: 100 })]
        );
        // 越界（101）/ 非数字 / 负数 → 非法回退（事件丢弃，不钳不猜）
        assert!(feed(&mut sink, b"\x1b]9;4;1;101\x07").is_empty());
        assert!(feed(&mut sink, b"\x1b]9;4;1;abc\x07").is_empty());
        assert!(feed(&mut sink, b"\x1b]9;4;1;-5\x07").is_empty());
        // 数字两侧空白容忍（应用端常见 printf 手误）
        assert_eq!(
            feed(&mut sink, b"\x1b]9;4;1; 42 \x07"),
            vec![OscEvent::Progress(ProgressState::Set { percent: 42 })]
        );
    }

    #[test]
    fn osc_text_is_capped_against_pathological_sequences() {
        let mut sink = OscSink::new();
        let long_body = "x".repeat(OSC_TEXT_MAX_CHARS + 100);
        let events = feed(&mut sink, format!("\x1b]9;{long_body}\x07").as_bytes());
        assert_eq!(
            events,
            vec![OscEvent::Notify {
                body: "x".repeat(OSC_TEXT_MAX_CHARS)
            }]
        );
    }

    #[test]
    fn non_osc_bytes_leave_no_events() {
        let mut sink = OscSink::new();
        // 普通文本 + CSI + 内置 OSC（标题）：旁路只认自家的几个
        assert!(
            feed(
                &mut sink,
                b"hello \x1b[31mred\x1b[0m \x1b]0;title\x07 world"
            )
            .is_empty()
        );
    }

    // ───────────── PromptTracker：淘汰修正 / 作废 / 生命周期 ─────────────

    #[test]
    fn tracker_visible_marks_shift_by_evicted_and_drop_out_of_range() {
        let mut tracker = PromptTracker::new();
        tracker.push_prompt(10, 100);
        tracker.push_prompt(20, 110);
        // 无滚动：原样
        assert_eq!(
            tracker.visible_marks(200),
            vec![
                PromptMark {
                    abs_line: 10,
                    total_at_receipt: 100,
                    exit_code: None
                },
                PromptMark {
                    abs_line: 20,
                    total_at_receipt: 110,
                    exit_code: None
                },
            ]
        );
        // 滚动淘汰 5 行：绝对行号整体下移 5
        tracker.note_scroll(5);
        let marks = tracker.visible_marks(200);
        assert_eq!(marks[0].abs_line, 5);
        assert_eq!(marks[1].abs_line, 15);
        // 滚出缓冲（total 缩到 8）：行号 5 的标记出界丢弃，表内真删
        assert_eq!(tracker.visible_marks(8).len(), 1);
        assert_eq!(tracker.visible_marks(200).len(), 1);
    }

    #[test]
    fn tracker_invalidate_clears_marks_and_scroll_base() {
        let mut tracker = PromptTracker::new();
        tracker.push_prompt(30, 40);
        tracker.note_scroll(7);
        tracker.invalidate();
        assert!(tracker.visible_marks(100).is_empty());
        // 计数基准重置：作废后再滚动从 0 起算（新受理的标记按新基准修正）
        tracker.push_prompt(5, 10);
        tracker.note_scroll(2);
        assert_eq!(tracker.visible_marks(100)[0].abs_line, 3);
    }

    #[test]
    fn tracker_command_lifecycle_running_and_exit_backfill() {
        let mut tracker = PromptTracker::new();
        assert!(!tracker.command_running());
        // A → C → D;3：运行窗开合 + 退出码回填最近未关闭标记 + 失败置位
        tracker.push_prompt(0, 10);
        tracker.note_command_start(false);
        assert!(tracker.command_running());
        tracker.note_command_finished(Some(3));
        assert!(!tracker.command_running());
        assert!(tracker.last_command_failed());
        assert_eq!(tracker.visible_marks(100)[0].exit_code, Some(3));

        // 下一条成功：失败态清零，旧标记退出码保持
        tracker.push_prompt(5, 15);
        tracker.note_command_start(false);
        tracker.note_command_finished(Some(0));
        assert!(!tracker.last_command_failed());
        let marks = tracker.visible_marks(100);
        assert_eq!(marks[0].exit_code, Some(3));
        assert_eq!(marks[1].exit_code, Some(0));

        // 未知退出码（D 不带 code）：不改失败态，只收束运行窗
        tracker.note_command_start(false);
        tracker.note_command_finished(None);
        assert!(!tracker.command_running());
        assert!(!tracker.last_command_failed());
    }

    #[test]
    fn tracker_tolerates_orphan_c_and_d_and_caps_marks() {
        let mut tracker = PromptTracker::new();
        // 孤儿 C（zsh 只发 C 不发 A）：不入表，只开运行窗；随后 A 收束
        tracker.note_command_start(false);
        assert!(tracker.command_running());
        assert!(tracker.visible_marks(100).is_empty());
        tracker.push_prompt(0, 10);
        assert!(!tracker.command_running());

        // 孤儿 D（无未关闭标记）：不 panic，只更新失败态
        tracker.note_command_finished(Some(9));
        assert!(tracker.last_command_failed());

        // 容量封顶：最老标记淘汰，表大小恒 ≤ MAX_MARKS
        for line in 0..(super::PromptTracker::MAX_MARKS + 32) {
            tracker.push_prompt(line, line + 1);
        }
        let marks = tracker.visible_marks(1_000_000);
        assert_eq!(marks.len(), super::PromptTracker::MAX_MARKS);
        assert_eq!(
            marks.first().map(|mark| mark.abs_line),
            Some((super::PromptTracker::MAX_MARKS + 32) - super::PromptTracker::MAX_MARKS)
        );
    }
}
