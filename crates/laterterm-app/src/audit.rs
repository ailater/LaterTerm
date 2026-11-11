//! 会话审计（audit 子项【logging】+【triggers】）：PTY 输出 tee 落盘与
//! 触发器规则扫描。
//!
//! 口径（评审定案）：
//! - 挂点 = `take_streams` 会话级装饰器（[`AuditSession`]），不动 termview
//!   读泵；装饰器叠放顺序 = audit 内层（贴近真实字节源）→ transfer 外层
//!   （ZMODEM 拦截，见 workspace::wrap_session）。因此 ZMODEM 传输期间入
//!   日志的是 PTY 原始输出字节（如实行为，非缺陷）。
//! - SSH GBK 会话的 tee 点在 `encoding.apply`（laterterm-ssh lib.rs:522）
//!   下游，落盘/扫描的都是转码后的 UTF-8 而非字节级原始流（中文模式可用，
//!   如实标注）。
//! - 读泵（termview read_stdout_blocking，已是独立线程）只做一次 `try_send`：
//!   chunk 通道有界 256 × 4KB ≈ 1MB，满则丢帧并计数——tee 允许缺段，绝不
//!   阻塞读泵（评审 6a：unbounded 在慢盘下内存无界）。丢帧区内日志缺段与
//!   触发器漏报并存（风险③同源），丢帧计数 v1 只计不提示。
//! - 工作线程常驻（每会话一条，recv 挂起零 CPU）：文件日志按需开关
//!   （OpenFile/CloseFile），触发器扫描随规则集启停——未开日志但有启用规则
//!   时 chunk 照常进扫描（tap 快路径 = 未开日志**且**无启用规则才直通）。
//! - 触发器扫描：滚动窗口 Vec<u8>（上限 64KB，超限截半并校正已扫偏移），
//!   每 chunk 从「上次扫止点前 8KB 重叠区」起扫——跨 chunk 边界匹配不漏，
//!   重叠区重复命中由去抖/锁存吸收；超长（>8KB）且跨界多 chunk 的匹配
//!   v1 不保证（重叠区上限，如实标注）。正则用 regex::bytes（非 UTF-8
//!   安全的 PTY 字节流，不自造轮子）。规则集经 Arc 热替换（设置保存/热加载
//!   时 AuditHub::swap_rules 重编译），编译失败的规则休眠 + eprintln 告警。
//! - 去抖：Notify 按 (窗格, 规则) 30s 冷却（worker 内 HashMap，常量不进
//!   配置防过度设计）；Highlight 锁存——worker 侧抑制重复上报，UI 侧
//!   Pane.highlighted 锁存，焦点/激活标签清除时经 ClearHighlight 复位。
//!   worker → UI 事件通道 unbounded（去抖后低频，无内存风险），与 chunk
//!   通道的有界口径不冲突。
//! - 文件名 `{YYYYMMDD-HHMMSS}_{净化label}.log`：label 取 workspace attach
//!   时传入的标题参数（SSH = 条目名、本地 = 「终端 N」），不经 session.title()
//!   （后者对 SSH 含「⇢」「:」跳板链字符，净化后整段变下划线）；时机 = 开启
//!   日志瞬间一次定型，后续 OSC 改名不改文件名。时间戳为 UTC——std 无本地
//!   时区能力，不为文件名引入时区依赖（sftp_panel::format_mtime 同款口径）。
//! - 权限 0o600：输出含潜在敏感内容（回显型密码提示仍可能入盘），不给
//!   umask 0644 机会。
//! - 收尾双路径，保证会话关闭完整：① 会话 EOF 时 [`AuditReader`] 返回 0
//!   前先发 `Finalize`（工作线程 flush 并释放句柄，且不再接受 OpenFile）；
//!   ② tap 随窗格 drop 时 sender 关闭 → 工作线程排空退出。两条路径都以
//!   flush 收尾，不丢已入通道的块。写失败（盘满/句柄失效）放弃本文件后续
//!   写入，tee 不拖累终端本体。
//! - 终端输出含 ANSI 转义序列：着色不切断词时子串匹配仍命中
//!   （\x1b[31mERROR\x1b[0m 含 ERROR），增量重绘切碎词的极端场景漏报——
//!   v1 不做 ANSI 剥离（开销与误伤大于收益），如实标注。

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use laterterm_config::{TriggerAction, TriggerRule};
use laterterm_core::{
    BackendKind, ProcessSnapshot, RunningProcess, SessionStreams, TerminalSession,
};
use regex::bytes::Regex;

use crate::workspace::PaneId;

/// chunk 通道容量：读泵单块上限 4KB（termview view.rs read_stdout_blocking
/// 的 4096B 缓冲），256 块 ≈ 1MB 积压；满即丢帧计数。
const CHUNK_CHANNEL_CAPACITY: usize = 256;

/// 文件名 label 限长（字符数）：超长 SSH 条目名/终端标题不再撑爆文件名。
const LABEL_MAX_CHARS: usize = 80;

/// 触发器滚动扫描窗口上限：超限截半（保留后半并校正已扫偏移），内存有界。
const SCAN_WINDOW_LIMIT: usize = 64 * 1024;

/// 跨 chunk 重叠区：每次从「上次扫止点前 8KB」起扫，跨界匹配不漏；
/// 重叠区内的重复命中由 Notify 冷却 / Highlight 锁存吸收。
const SCAN_OVERLAP: usize = 8 * 1024;

/// Notify 去抖冷却：按 (窗格, 规则) 30s。常量不进配置（防过度设计）。
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// 触发器规则与事件
// ---------------------------------------------------------------------------

/// 已编译规则（规则集快照的元素）。编译失败的规则不入集（休眠）。
pub(crate) struct CompiledRule {
    name: String,
    action: TriggerAction,
    regex: Regex,
}

/// 工作线程 → UI 的触发事件（去抖后低频，unbounded 通道无内存风险）。
#[derive(Clone, Debug)]
pub(crate) struct TriggerEvent {
    pub pane: PaneId,
    /// 规则名（通知文案与调试定位）。
    pub rule: String,
    pub action: TriggerAction,
}

/// 审计共享枢纽：规则句柄（热替换）+ 事件发送端 + 「有启用规则」快路径旗标。
/// WorkspaceView 持有本体，每个 TapControl 克隆其内部 Arc。
#[derive(Clone)]
pub(crate) struct AuditHub {
    rules: Arc<parking_lot::Mutex<Arc<Vec<CompiledRule>>>>,
    events: flume::Sender<TriggerEvent>,
    /// 有启用（且编译成功）规则 = true：tap 快路径据此在「未开日志」时决定
    /// 是否仍向工作线程送 chunk（触发器扫描不依赖文件日志开关）。
    rules_active: Arc<AtomicBool>,
}

impl AuditHub {
    /// 建枢纽与事件接收端（WorkspaceView::new 持有 rx 供 render 头部泵消费）。
    pub(crate) fn new() -> (Self, flume::Receiver<TriggerEvent>) {
        let (events, rx) = flume::unbounded();
        let hub = Self {
            rules: Arc::new(parking_lot::Mutex::new(Arc::new(Vec::new()))),
            events,
            rules_active: Arc::new(AtomicBool::new(false)),
        };
        (hub, rx)
    }

    /// 重编译并整体热替换规则集（设置保存 / settings.toml 热加载时调用）。
    /// 编译失败（语法坏）的规则休眠 + eprintln 告警（快捷键无效告警先例）；
    /// 停用规则不编译。读侧按 chunk 取 Arc 快照，锁只护指针替换。
    pub(crate) fn swap_rules(&self, rules: &[TriggerRule]) {
        let compiled = compile_rules(rules);
        self.rules_active
            .store(!compiled.is_empty(), Ordering::Release);
        *self.rules.lock() = compiled;
    }
}

/// 编译规则集：无效正则跳过并告警（「休眠」，不阻塞其余规则生效）。
fn compile_rules(rules: &[TriggerRule]) -> Arc<Vec<CompiledRule>> {
    let mut compiled = Vec::new();
    for rule in rules {
        if !rule.enabled {
            continue; // 停用规则保留原文，不参与匹配
        }
        match Regex::new(&rule.pattern) {
            Ok(regex) => compiled.push(CompiledRule {
                name: rule.name.clone(),
                action: rule.action,
                regex,
            }),
            Err(error) => eprintln!("触发器规则「{}」正则无效，已休眠: {error}", rule.name),
        }
    }
    Arc::new(compiled)
}

// ---------------------------------------------------------------------------
// 装配入口与装饰器
// ---------------------------------------------------------------------------

/// 装配入口：把会话包成 [`AuditSession`]（审计 tap 内层），返回开关句柄给
/// Pane 持有。`log_enabled` = 全局默认（settings.appearance.session_logging），
/// attach 时作为文件日志初值；触发器扫描与该开关无关（见快路径口径）。
/// 与 TransferWrappedSession（workspace.rs）同构：装饰器是仓库既定演进位。
pub(crate) fn wrap_audit(
    session: Arc<dyn TerminalSession>,
    pane: PaneId,
    label: &str,
    log_enabled: bool,
    hub: &AuditHub,
) -> (Arc<dyn TerminalSession>, Arc<TapControl>) {
    let tap = TapControl::new(pane, label, log_enabled, hub);
    let session = Arc::new(AuditSession {
        inner: session,
        tap: Arc::clone(&tap),
    });
    (session, tap)
}

/// 会话装饰器：PTY 输出 tee 审计。先例 = TransferWrappedSession；
/// 装饰器通用纪律（评审 1）：`TerminalSession` 任何新增方法在此层与
/// TransferWrappedSession 层都必须显式转发（probe_running 为先例）。
pub(crate) struct AuditSession {
    inner: Arc<dyn TerminalSession>,
    tap: Arc<TapControl>,
}

impl TerminalSession for AuditSession {
    fn backend(&self) -> BackendKind {
        self.inner.backend()
    }

    fn title(&self) -> String {
        self.inner.title()
    }

    fn take_streams(&self) -> io::Result<SessionStreams> {
        let mut streams = self.inner.take_streams()?;
        // 读端 tee：视图读泵阻塞的是 AuditReader，tee 旁路永不反压读泵
        streams.reader = Box::new(AuditReader {
            inner: streams.reader,
            tap: Arc::clone(&self.tap),
        });
        Ok(streams)
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.inner.resize(cols, rows)
    }

    fn kill(&self) {
        self.inner.kill();
    }

    /// 关闭确认探测透传：包装层不参与判定（同 TransferWrappedSession 口径）。
    fn probe_running(&self, snapshot: &mut Option<ProcessSnapshot>) -> Option<RunningProcess> {
        self.inner.probe_running(snapshot)
    }

    /// 非预期退出判别透传：审计层在最内层贴近真实字节源，判定一律转
    /// inner（SSH 层置位）。默认方法不穿透装饰器，必须显式转发——漏转发
    /// 则重连判别在最外层恒读 false（评审 1 必须项）。
    fn exit_unexpected(&self) -> bool {
        self.inner.exit_unexpected()
    }
}

/// 读端 tee：termview 读泵阻塞读本 reader；每块先 tee 给审计工作线程
/// （try_send 永不阻塞），EOF 先发 Finalize 再返回 0（读泵以 0 为会话结束）。
struct AuditReader {
    inner: Box<dyn Read + Send>,
    tap: Arc<TapControl>,
}

impl Read for AuditReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.tap.tap_bytes(&buf[..n]);
        } else {
            // EOF：先收尾（flush+释放句柄）再交出会话结束信号
            self.tap.finalize();
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// 开关句柄（Pane 持有）
// ---------------------------------------------------------------------------

/// 会话审计开关句柄（每窗格一个，`Pane.audit` 持有 Arc）：
/// - 读泵热路径只碰原子量（未开日志且无启用规则 = 直通，不 send 不分配）；
/// - 工作线程在构造时即常驻 spawn（recv 挂起零 CPU），文件日志经
///   OpenFile/CloseFile 消息按需开关——路径在开启瞬间定型，建目录/开文件
///   全在工作线程（不碰 gpui 主线程）；
/// - 关 = 发 CloseFile（flush+关文件，会话仍在可再开）；
/// - 窗格 drop（Arc 归零）摘走 sender → 工作线程排空收尾退出。
pub(crate) struct TapControl {
    /// 日志文件名 label 原文（attach 标题参数）；净化在开启瞬间做。
    label: String,
    /// 共享枢纽（规则句柄/事件发送端/快路径旗标），与 WorkspaceView 同源。
    hub: AuditHub,
    /// chunk/控制消息通道：工作线程唯一入口。构造即建，随句柄 drop 断开。
    inner: parking_lot::Mutex<TapInner>,
    /// 文件日志快路径开关。开启顺序 = 先发 OpenFile 再置 true；关闭顺序 =
    /// 先清 true 再发 CloseFile（消息序有界通道内保序）。
    logging: AtomicBool,
    /// 丢帧计数（通道满被丢弃的 chunk 数；评审 6a 代价，v1 只计不提示）。
    dropped_chunks: AtomicU64,
}

struct TapInner {
    tx: flume::Sender<LogMessage>,
}

impl TapControl {
    /// 构造并 spawn 常驻工作线程；`log_enabled`（全局默认）为真则立即开文件。
    pub(crate) fn new(pane: PaneId, label: &str, log_enabled: bool, hub: &AuditHub) -> Arc<Self> {
        let (tx, rx) = flume::bounded(CHUNK_CHANNEL_CAPACITY);
        // 工作线程常驻：文件句柄/扫描窗口/去抖状态全在该线程，消息驱动
        let worker_hub = hub.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("laterterm-audit".into())
            .spawn(move || AuditWorker::new(pane, worker_hub).run(rx))
        {
            // spawn 失败：tap 退化为直通（send 全部 Disconnected 静默），终端不受影响
            eprintln!("会话审计工作线程启动失败: {error}");
        }
        let tap = Arc::new(Self {
            label: label.to_owned(),
            hub: hub.clone(),
            inner: parking_lot::Mutex::new(TapInner { tx }),
            logging: AtomicBool::new(false),
            dropped_chunks: AtomicU64::new(0),
        });
        if log_enabled {
            tap.set_logging(true);
        }
        tap
    }

    /// 当前是否在记录文件日志（标签右键菜单勾选态读口）。
    pub(crate) fn is_logging(&self) -> bool {
        self.logging.load(Ordering::Acquire)
    }

    /// 丢帧计数（后续版本 UI 提示预留，v1 只计）。
    #[allow(dead_code)]
    pub(crate) fn dropped_chunks(&self) -> u64 {
        self.dropped_chunks.load(Ordering::Relaxed)
    }

    /// 热开/热关文件日志（标签右键菜单、attach 初值共用）。
    pub(crate) fn set_logging(&self, on: bool) {
        self.set_logging_in(logs_dir(), on);
    }

    /// `set_logging` 的目录注入版（测试用 tempdir 挂真实目录；生产恒 logs_dir）。
    /// 关路径不使用目录，参数仅为统一入口。
    fn set_logging_in(&self, dir: Option<PathBuf>, on: bool) {
        if on {
            if self.is_logging() {
                return; // 已开启：保持现文件不重复开（菜单重复点击幂等）
            }
            let Some(dir) = dir else {
                eprintln!("无法定位会话日志目录（无可用状态目录），本次开启未生效");
                return;
            };
            // 文件名在开启瞬间一次定型：时间戳取此刻、label 就地净化（评审 4）；
            // 建目录/开文件在工作线程（OpenFile 消息），主线程零文件 IO。
            // OpenFile 是控制消息：通道满被丢 = logging=true 但文件从未打开，
            // 整个日志会话静默缺失——比丢一块数据重得多，故失败即回滚旗标
            // （UI 呈现为"未开启"，用户可重试；chunk 丢失仍走 v1 计数口径）。
            let name = log_file_name(&self.label, SystemTime::now());
            if self.send(LogMessage::OpenFile(dir.join(name))) {
                self.logging.store(true, Ordering::Release);
            } else {
                eprintln!("会话日志开启失败（控制通道饱和），本次未生效，可重试");
            }
        } else {
            // 关：先清快路径（读泵不再按日志口径 send），再发 CloseFile——
            // 工作线程 flush+关文件（非 EOF，会话仍在可再开新文件）
            self.logging.store(false, Ordering::Release);
            self.send(LogMessage::CloseFile);
        }
    }

    /// UI 清除高亮锁存后复位 worker 侧抑制：允许下次命中重新上报。
    pub(crate) fn clear_highlights(&self) {
        self.send(LogMessage::ClearHighlight);
    }

    /// 读泵热路径（termview read_stdout_blocking 线程内调用）：有界通道
    /// try_send，满则丢帧计数——绝不阻塞读泵。未开日志且无启用规则 = 直通。
    fn tap_bytes(&self, bytes: &[u8]) {
        if !self.is_logging() && !self.hub.rules_active.load(Ordering::Acquire) {
            return;
        }
        // 分配只发生在需要 tee/扫描时（快路径不分配）
        self.send(LogMessage::Chunk(bytes.to_vec()));
    }

    /// 会话 EOF 收尾信号（AuditReader 返回 0 前发出）。
    fn finalize(&self) {
        if self.is_logging() {
            // Full/Disconnected 都不影响完整性：逐块已 flush，sender 断开仍收尾
            self.send(LogMessage::Finalize);
        }
    }

    /// 通道发送 + 丢帧计数（Full 计入；Disconnected = 工作线程已收尾，静默）。
    /// 返回是否落地：调用方据此区分数据块（丢失走计数口径）与控制消息
    /// （丢失需回滚状态，见 set_logging_in 的 OpenFile 分支）。
    fn send(&self, message: LogMessage) -> bool {
        let tx = self.inner.lock().tx.clone();
        match tx.try_send(message) {
            Ok(()) => true,
            Err(flume::TrySendError::Full(_)) => {
                self.dropped_chunks.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(flume::TrySendError::Disconnected(_)) => false,
        }
    }
}

impl Drop for TapControl {
    fn drop(&mut self) {
        // 窗格回收路径的收尾：把槽内 sender 换成必然断连的占位（旧 sender
        // 随赋值 drop，成为最后一个克隆）→ 工作线程排空退出（flush 后关闭）
        self.logging.store(false, Ordering::Release);
        self.inner.get_mut().tx = dead_sender();
    }
}

/// 构造一个必然断连的发送端占位：Drop 后 TapInner.tx 恒有值，迟到的
/// send 全部走 Disconnected 静默分支，worker 侧 recv Err 即排空收尾。
fn dead_sender() -> flume::Sender<LogMessage> {
    let (tx, rx) = flume::bounded(1);
    drop(rx);
    tx
}

// ---------------------------------------------------------------------------
// 工作线程：文件日志 + 触发器扫描
// ---------------------------------------------------------------------------

/// 工作线程消息：chunk 数据、文件日志开关、会话收尾、高亮锁存复位。
enum LogMessage {
    Chunk(Vec<u8>),
    /// 开启文件日志：路径已在开启瞬间定型（时间戳取此刻）；建目录/原子建
    /// 文件在本线程做（0o600，冲突探测 -2/-3 后缀）。
    OpenFile(PathBuf),
    /// 热关文件日志：flush + 关闭（会话仍在，可再开新文件）。
    CloseFile,
    /// 会话 EOF（AuditReader 返回 0 前发出）：flush + 关闭并终结——后续
    /// Chunk 不再写入，OpenFile 被忽略（已退出窗格无输出可记）。
    Finalize,
    /// UI 已清除该窗格高亮锁存：重置 worker 侧抑制，下次命中重新上报。
    ClearHighlight,
}

/// 每会话审计工作线程状态：文件句柄 + 扫描窗口 + 去抖/锁存。纯逻辑可单测。
struct AuditWorker {
    pane: PaneId,
    hub: AuditHub,
    file: Option<std::fs::File>,
    /// Finalize 后置位：文件终结，不再接受 OpenFile/Chunk 写入。
    finalized: bool,
    /// 触发器滚动扫描窗口（上限 SCAN_WINDOW_LIMIT，超限截半）。
    window: Vec<u8>,
    /// 窗口内已扫止点（字节偏移）；每 chunk 扫完后推进到窗口末尾。
    scanned: usize,
    /// Notify 去抖：规则名 → 上次上报时刻（30s 冷却）。
    notify_cooldowns: HashMap<String, Instant>,
    /// Highlight 锁存：已上报未清除的规则名（命中抑制，ClearHighlight 复位）。
    highlight_latched: HashSet<String>,
}

impl AuditWorker {
    fn new(pane: PaneId, hub: AuditHub) -> Self {
        Self {
            pane,
            hub,
            file: None,
            finalized: false,
            window: Vec::new(),
            scanned: 0,
            notify_cooldowns: HashMap::new(),
            highlight_latched: HashSet::new(),
        }
    }

    /// 消息驱动主循环：recv 在 sender 全部 drop 且排空后返回 Err——自然收尾。
    fn run(mut self, rx: flume::Receiver<LogMessage>) {
        while let Ok(message) = rx.recv() {
            self.handle(message);
        }
        // sender 断开：排空已完，句柄 drop 即关闭（逐块已 flush，无残留缓冲）
    }

    fn handle(&mut self, message: LogMessage) {
        match message {
            LogMessage::Chunk(bytes) => {
                // 文件 tee（逐块 write+flush，写失败放弃本文件后续写入）
                if let Some(file) = self.file.as_mut()
                    && let Err(error) = file.write_all(&bytes).and_then(|()| file.flush())
                {
                    eprintln!("会话日志写入失败: {error}");
                    self.file = None;
                }
                // 触发器扫描：取规则集 Arc 快照，锁只护指针、扫描在锁外
                if !self.finalized {
                    let rules = self.hub.rules.lock().clone();
                    self.scan_chunk(&rules, &bytes);
                }
            }
            LogMessage::OpenFile(path) => {
                if self.finalized {
                    return; // 会话已 EOF：不再接受新文件（防复活）
                }
                if self.file.is_some() {
                    return; // 已有文件：先 CloseFile 再开（幂等防呆）
                }
                self.file = open_log_file_with_dir(&path);
            }
            LogMessage::CloseFile => {
                // flush 逐块已做，直接释放句柄；可再 OpenFile
                self.file = None;
            }
            LogMessage::Finalize => {
                // 会话 EOF 收尾：释放句柄并终结
                self.file = None;
                self.finalized = true;
            }
            LogMessage::ClearHighlight => {
                self.highlight_latched.clear();
            }
        }
    }

    /// 增量扫描一个 chunk：追加进滚动窗口 → 超限截半 → 从重叠区起扫 →
    /// 推进扫止点。跨界匹配由下一 chunk 的重叠区补齐（见模块头注记）。
    fn scan_chunk(&mut self, rules: &[CompiledRule], bytes: &[u8]) {
        self.window.extend_from_slice(bytes);
        if self.window.len() > SCAN_WINDOW_LIMIT {
            // 截半保留后半；drop ≤ scanned 保证不截掉未扫数据
            let drop = (self.window.len() - SCAN_WINDOW_LIMIT / 2).min(self.scanned);
            self.window.drain(..drop);
            self.scanned -= drop;
        }
        // 起扫点 = 上次扫止点前 8KB（跨界补齐）；重叠区重复命中由
        // Notify 冷却 / Highlight 锁存吸收
        let start = self.scanned.saturating_sub(SCAN_OVERLAP);
        for rule in rules {
            if !rule.regex.is_match(&self.window[start..]) {
                continue;
            }
            match rule.action {
                TriggerAction::Notify => {
                    let now = Instant::now();
                    if self
                        .notify_cooldowns
                        .get(&rule.name)
                        .is_some_and(|sent| now.duration_since(*sent) < NOTIFY_COOLDOWN)
                    {
                        continue; // 冷却中：吸收重叠双发与高频重复
                    }
                    self.notify_cooldowns.insert(rule.name.clone(), now);
                    let _ = self.hub.events.send(TriggerEvent {
                        pane: self.pane,
                        rule: rule.name.clone(),
                        action: TriggerAction::Notify,
                    });
                }
                TriggerAction::Highlight => {
                    if !self.highlight_latched.insert(rule.name.clone()) {
                        continue; // 锁存中：UI 清除（ClearHighlight）前不再上报
                    }
                    let _ = self.hub.events.send(TriggerEvent {
                        pane: self.pane,
                        rule: rule.name.clone(),
                        action: TriggerAction::Highlight,
                    });
                }
            }
        }
        self.scanned = self.window.len();
    }
}

/// 工作线程入口：建目录 + 原子建文件（0o600）都在线程内，失败只 eprintln
/// （tee 是旁路，绝不影响终端本体；默认关的用户不产生空目录）。
fn open_log_file_with_dir(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("会话日志目录创建失败（{}）: {error}", parent.display());
        return None;
    }
    match open_log_file(path) {
        Ok(file) => Some(file),
        Err(error) => {
            eprintln!("会话日志文件创建失败（{}）: {error}", path.display());
            None
        }
    }
}

/// 原子建日志文件：`create_new` 探测同秒同名冲突，冲突依次尝试 -2/-3/… 后缀
/// （原子建文件，无 TOCTOU）。Unix 显式 0o600（输出含敏感内容，不给 umask
/// 0644 机会；Windows 无此语义）。100 次仍冲突则放弃（同一秒内同名 label 开
/// 百个日志现实中不可达，纯防呆上限）。
fn open_log_file(base: &std::path::Path) -> io::Result<std::fs::File> {
    let stem = base
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session")
        .to_owned();
    let extension = base
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("log")
        .to_owned();
    let parent = base
        .parent()
        .map(|dir| dir.to_path_buf())
        .unwrap_or_default();
    let candidates = std::iter::once(String::new()).chain((2..=100).map(|n| format!("-{n}")));
    for suffix in candidates {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 0o600：日志含终端输出，默认权限收紧（Linux 优先口径）
            options.mode(0o600);
        }
        match options.open(parent.join(format!("{stem}{suffix}.{extension}"))) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "日志文件名冲突探测 100 次仍失败",
    ))
}

// ---------------------------------------------------------------------------
// 目录定位与打开
// ---------------------------------------------------------------------------

/// 会话日志目录：`<状态目录>/laterterm/logs`（Linux = XDG_STATE_HOME，回落
/// ~/.local/state；见 `laterterm_config::app_state_dir`）。目录本体由工作
/// 线程在开文件前惰性创建。
pub(crate) fn logs_dir() -> Option<PathBuf> {
    laterterm_config::app_state_dir().map(|dir| dir.join("logs"))
}

/// 设置页「打开日志目录」：系统文件管理器打开。目录定位/创建与文件管理器
/// 进程拉起全部下沉后台线程——按钮回调在 gpui 主线程触发，主线程不做文件
/// IO（仓库纪律）；失败只 eprintln 不阻塞 UI。
pub(crate) fn open_logs_dir() {
    match std::thread::Builder::new()
        .name("laterterm-open-logs".into())
        .spawn(open_logs_dir_blocking)
    {
        Ok(_handle) => {}
        Err(error) => eprintln!("打开日志目录线程启动失败: {error}"),
    }
}

/// 「打开日志目录」的实际动作（后台线程内执行）：目录不存在先建（让按钮
/// 总有反馈），再按平台惯例拉起文件管理器（跨平台分叉先例 = termview
/// open_url 的 cfg! 三分叉）。
fn open_logs_dir_blocking() {
    let Some(dir) = logs_dir() else {
        eprintln!("无法定位会话日志目录（无可用状态目录）");
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("创建会话日志目录失败（{}）: {error}", dir.display());
        return;
    }
    let target = dir.to_string_lossy().into_owned();
    let (program, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        ("open", vec![target])
    } else if cfg!(target_os = "windows") {
        ("explorer", vec![target])
    } else {
        ("xdg-open", vec![target])
    };
    if let Err(error) = std::process::Command::new(program).args(&args).spawn() {
        eprintln!("打开日志目录失败: {error}");
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑：文件名与时间戳
// ---------------------------------------------------------------------------

/// 日志文件名：`{YYYYMMDD-HHMMSS}_{净化label}.log`。
fn log_file_name(label: &str, at: SystemTime) -> String {
    format!("{}_{}.log", timestamp_compact(at), sanitize_label(label))
}

/// 开启瞬间时间戳 → `{YYYYMMDD-HHMMSS}`（UTC，见模块头注记）。
fn timestamp_compact(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// 文件名 label 净化：白名单 `[A-Za-z0-9._\-一-龥]` 外替换 `_`，限长 80 字符。
/// SSH 跳板链的「⇢」「:」「@」整段落下划线是预期行为（故 label 取 attach 时
/// 的条目名而非 session.title()）；CJK 基本区汉字原样保留。
fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '-')
                || ('一'..='龥').contains(&c)
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.chars().take(LABEL_MAX_CHARS).collect()
}

/// Howard Hinnant 的 civil_from_days：天数（自 1970-01-01）→ (年, 月, 日)。
/// 与 sftp_panel::format_mtime 同款算法（私有函数，复制一份并保持同步）。
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

// ---------------------------------------------------------------------------
// 测试（纯 std + flume + regex，可脱离 gpui 运行）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录（hand-rolled，先例 = config tests 的 temp_settings_path；
    /// app 无 tempfile dev-dep，不为此单测新增依赖）。
    fn unique_temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "laterterm-audit-test-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rule(name: &str, pattern: &str, action: TriggerAction) -> TriggerRule {
        TriggerRule {
            name: name.into(),
            pattern: pattern.into(),
            action,
            enabled: true,
        }
    }

    /// 编译单条测试规则为快照。
    fn compiled(pattern: &str, action: TriggerAction) -> Arc<Vec<CompiledRule>> {
        compile_rules(&[rule("t", pattern, action)])
    }

    /// 带已编译规则的测试 worker（不发事件到真通道的变体按需自建）。
    fn test_worker(rules: Arc<Vec<CompiledRule>>) -> (AuditWorker, flume::Receiver<TriggerEvent>) {
        let (events, rx) = flume::unbounded();
        let hub = AuditHub {
            rules: Arc::new(parking_lot::Mutex::new(rules)),
            events,
            rules_active: Arc::new(AtomicBool::new(true)),
        };
        (AuditWorker::new(PaneId(7), hub), rx)
    }

    #[test]
    fn sanitize_label_keeps_ascii_cjk_and_replaces_others() {
        assert_eq!(sanitize_label("终端 1"), "终端_1");
        assert_eq!(sanitize_label("prod⇢jump:22"), "prod_jump_22");
        assert_eq!(
            sanitize_label("ops@web-01.example.com"),
            "ops_web-01.example.com"
        );
        assert_eq!(sanitize_label("a_b-c.d"), "a_b-c.d");
        assert_eq!(sanitize_label(""), "");
    }

    #[test]
    fn sanitize_label_caps_at_80_chars() {
        let long = "字".repeat(100);
        assert_eq!(sanitize_label(&long).chars().count(), 80);
        // 截断按字符计：ASCII 长串同样取前 80
        let ascii = "x".repeat(100);
        assert_eq!(sanitize_label(&ascii).len(), 80);
    }

    #[test]
    fn log_file_name_formats_utc_timestamp_and_label() {
        // Unix 纪元 → 19700101-000000；时间戳口径 = UTC（模块头注记）
        assert_eq!(
            log_file_name("终端", SystemTime::UNIX_EPOCH),
            "19700101-000000_终端.log"
        );
        // 86_400 × 19_673 = 2023-11-12（经 date -u 核实；闰年累计走
        // civil_from_days 校验），+3_723s = 01:02:03
        let at = UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 19_673 + 3_723);
        assert_eq!(log_file_name("ops@web", at), "20231112-010203_ops_web.log");
    }

    #[test]
    fn open_log_file_probes_collision_suffix_and_never_overwrites() {
        let dir = unique_temp_dir("open");
        let base = dir.join("20260101-000000_x.log");
        // 同秒同名：先占位 base，再开 → 应得 -2 后缀而非报错或覆盖
        std::fs::write(&base, b"occupied").unwrap();
        let file = open_log_file(&base).unwrap();
        drop(file);
        let second = dir.join("20260101-000000_x-2.log");
        assert!(second.exists(), "冲突应落 -2 后缀");
        assert_eq!(
            std::fs::read(&base).unwrap(),
            b"occupied",
            "create_new 不得覆盖既有文件"
        );
        // Unix 权限口径：0600（新文件）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&second).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "新日志文件权限必须为 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_file_lifecycle_open_write_close_reopen_and_finalize() {
        let dir = unique_temp_dir("lifecycle");
        let first = dir.join("20260101-000000_a.log");
        let second = dir.join("20260101-000000_b.log");
        let (events, _rx) = flume::unbounded();
        let hub = AuditHub {
            rules: Arc::new(parking_lot::Mutex::new(Arc::new(Vec::new()))),
            events,
            rules_active: Arc::new(AtomicBool::new(false)),
        };
        let mut worker = AuditWorker::new(PaneId(1), hub);
        worker.handle(LogMessage::OpenFile(first.clone()));
        worker.handle(LogMessage::Chunk(b"one ".to_vec()));
        worker.handle(LogMessage::CloseFile);
        worker.handle(LogMessage::Chunk(b"dropped".to_vec()));
        worker.handle(LogMessage::OpenFile(second.clone()));
        worker.handle(LogMessage::Chunk(b"two".to_vec()));
        worker.handle(LogMessage::Finalize);
        worker.handle(LogMessage::Chunk(b" post".to_vec()));
        worker.handle(LogMessage::OpenFile(dir.join("revived.log")));
        assert_eq!(
            std::fs::read(&first).unwrap(),
            b"one ",
            "CloseFile 后的 Chunk 不得再落旧文件（无 dropped 残段）"
        );
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        assert!(
            !dir.join("revived.log").exists(),
            "Finalize 后 OpenFile 必须被忽略（防复活）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_matches_across_chunk_boundary_exactly_once() {
        let (mut worker, rx) = test_worker(compiled("ERROR", TriggerAction::Notify));
        // 匹配跨 chunk 边界：前块尾「ER」+ 后块头「ROR」
        worker.scan_chunk(&compiled("ERROR", TriggerAction::Notify), b"xx ER");
        assert!(rx.try_recv().is_err(), "跨界匹配在补齐前不得上报");
        worker.scan_chunk(&compiled("ERROR", TriggerAction::Notify), b"ROR yy");
        let event = rx.try_recv().expect("补齐后应命中");
        assert_eq!(event.rule, "t");
        assert_eq!(event.action, TriggerAction::Notify);
        assert_eq!(event.pane, PaneId(7));
        // 重叠区重复扫描被 30s 冷却吸收：再来一块同内容不双发
        worker.scan_chunk(&compiled("ERROR", TriggerAction::Notify), b"more ERROR");
        assert!(rx.try_recv().is_err(), "冷却期内不得双发");
    }

    #[test]
    fn scan_highlight_latches_until_cleared() {
        let (mut worker, rx) = test_worker(compiled("FAIL", TriggerAction::Highlight));
        worker.scan_chunk(&compiled("FAIL", TriggerAction::Highlight), b"FAIL");
        assert_eq!(rx.try_recv().unwrap().action, TriggerAction::Highlight);
        // 锁存中重复命中不上报（worker 侧抑制；UI 锁存为第二道防刷屏）
        worker.scan_chunk(&compiled("FAIL", TriggerAction::Highlight), b"FAIL again");
        assert!(rx.try_recv().is_err());
        // UI 清除高亮 → ClearHighlight 复位 → 下次命中重新上报
        worker.handle(LogMessage::ClearHighlight);
        worker.scan_chunk(&compiled("FAIL", TriggerAction::Highlight), b"FAIL 3rd");
        assert_eq!(rx.try_recv().unwrap().action, TriggerAction::Highlight);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn scan_window_truncation_keeps_recent_data_matchable() {
        let (mut worker, rx) = test_worker(compiled("TAIL", TriggerAction::Notify));
        // 先灌 70KB 无关字节（>64KB 触发截半），尾部再放命中词
        let filler = vec![b'x'; 70 * 1024];
        worker.scan_chunk(&compiled("TAIL", TriggerAction::Notify), &filler);
        worker.scan_chunk(&compiled("TAIL", TriggerAction::Notify), b"the TAIL end");
        assert!(rx.try_recv().is_ok(), "截半后近期数据仍可匹配");
        assert!(
            worker.window.len() <= SCAN_WINDOW_LIMIT,
            "窗口必须收敛到上限内"
        );
    }

    #[test]
    fn compile_rules_skips_disabled_and_invalid_patterns() {
        let disabled = TriggerRule {
            name: "off".into(),
            pattern: "AAA".into(),
            action: TriggerAction::Notify,
            enabled: false, // 与 rule() 助手不同：显式停用
        };
        let rules = vec![
            disabled.clone(),
            rule("bad", "E(R?", TriggerAction::Notify), // 非法语法
            rule("good", "OK", TriggerAction::Highlight),
        ];
        let compiled = compile_rules(&rules);
        assert_eq!(compiled.len(), 1, "停用与非法正则均不入集（休眠）");
        assert_eq!(compiled[0].name, "good");
        // 只剩停用规则：整个快照为空（rules_active 随之熄灭）
        let all_off = compile_rules(std::slice::from_ref(&disabled));
        assert!(all_off.is_empty());
    }

    #[test]
    fn hub_swap_rules_updates_active_flag() {
        let (hub, rx) = AuditHub::new();
        assert!(!hub.rules_active.load(Ordering::Acquire));
        hub.swap_rules(&[rule("t", "GO", TriggerAction::Notify)]);
        assert!(hub.rules_active.load(Ordering::Acquire));
        hub.swap_rules(&[]);
        assert!(!hub.rules_active.load(Ordering::Acquire));
        drop(rx);
    }

    #[test]
    fn tap_control_off_without_rules_is_passthrough_and_no_dropped_count() {
        let (hub, _rx) = AuditHub::new();
        let tap = TapControl::new(PaneId(1), "t", false, &hub);
        assert!(!tap.is_logging(), "初值 false = 直通");
        // 关态且无规则 tap_bytes / finalize 直通：不 panic、不计数
        tap.tap_bytes(b"x");
        tap.finalize();
        assert_eq!(tap.dropped_chunks(), 0);
    }

    #[test]
    fn tap_control_scans_even_when_logging_off_if_rules_active() {
        let (hub, rx) = AuditHub::new();
        hub.swap_rules(&[rule("t", "PING", TriggerAction::Highlight)]);
        let tap = TapControl::new(PaneId(3), "t", false, &hub);
        assert!(!tap.is_logging(), "文件日志未开");
        tap.tap_bytes(b"a PING b");
        // 未开日志但有启用规则：chunk 照常进扫描并产生事件。
        // 事件在常驻工作线程异步产生，轮询等待（有界 5s 防悬挂）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let event = loop {
            match rx.try_recv() {
                Ok(event) => break event,
                Err(flume::TryRecvError::Empty) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "规则激活时未开日志也应扫描"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(flume::TryRecvError::Disconnected) => {
                    panic!("事件通道不应断开（tap 仍持有）");
                }
            }
        };
        assert_eq!(event.action, TriggerAction::Highlight);
        assert_eq!(event.pane, PaneId(3));
    }

    #[test]
    fn tap_control_set_logging_writes_visible_bytes_to_injected_dir() {
        let (hub, _rx) = AuditHub::new();
        let tap = TapControl::new(PaneId(2), "会话 2", false, &hub);
        let dir = unique_temp_dir("tap-enable");
        tap.set_logging_in(Some(dir.clone()), true);
        assert!(tap.is_logging());
        tap.tap_bytes("内容".as_bytes());
        tap.set_logging_in(Some(dir.clone()), false);
        assert!(!tap.is_logging());
        // 工作线程异步落盘：轮询等待（有界 5s，文件内容只增不减，无假失败窗口）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut body = String::new();
        while std::time::Instant::now() < deadline {
            if let Some(entry) = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .find(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
                && let Ok(content) = std::fs::read_to_string(entry.path())
            {
                body = content;
                if !body.is_empty() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(body, "内容", "热开→tap→热关后文件应完整落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
