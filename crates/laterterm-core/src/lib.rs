//! LaterTerm 会话抽象层。
//!
//! [`TerminalSession`] 是本地 PTY 与 SSH 远程会话的共同契约：终端视图
//! （laterterm-termview）只面向本 trait 编程，「远程」不是另一个功能，
//! 只是换了 backend 的同一个终端（见 docs/architecture.md）。
//!
//! 本 crate 刻意保持零第三方依赖（纯 std），保证 SSH/编码/trzsz 等后续
//! 扩展不会把传输实现细节泄漏进视图层。

use std::io::{self, Read, Write};

/// 会话后端类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// 本地 PTY（laterterm-local）。
    Local,
    /// SSH 远程会话（laterterm-ssh）。
    Ssh,
}

/// 会话的字节流，由 [`TerminalSession::take_streams`] 一次性移交给视图。
///
/// reader 供视图的读泵线程阻塞读取（EOF 即会话结束信号）；
/// writer 供键盘输入与命令面板下发。
pub struct SessionStreams {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

/// 关闭二次确认探测到的「运行中进程」。
///
/// `name` 为探测到的进程名（如 `sleep`、`vim`）；仅用于弹窗如实展示，
/// 不承担精确诊断职责。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningProcess {
    /// 进程名（/proc 口径的 comm；读不到时由实现方给「未知进程」占位）。
    pub name: String,
}

/// 共享 /proc 快照的单条进程记录（`/proc/<pid>/stat` 的前几个字段 + comm）。
///
/// 纯数据、无平台依赖：trait 签名在任意平台编译，快照槽在非 Linux 下恒 None。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    /// 进程状态字符（如 `R`/`S`/`Z`）；`Z` = 僵尸，探测时排除。
    pub state: char,
    /// 进程名（stat 第 2 字段，内核截 15 字符）。
    pub comm: String,
}

/// 一次关闭请求共享的 /proc 全表快照。
///
/// 由首个需要子树判据（判据 b2）的会话惰性建立、同一请求内的其余窗格
/// 复用——N 个空闲窗格的窗口关闭至多一次全表扫描，而非每窗格一遍。
#[derive(Clone, Debug, Default)]
pub struct ProcessSnapshot {
    pub entries: Vec<ProcEntry>,
}

/// 终端会话契约。
///
/// 实现方保证：`resize`/`kill` 线程安全且 `kill` 幂等；`take_streams`
/// 只能成功一次（防止双读者把字节流分裂），二次调用返回错误。
pub trait TerminalSession: Send + Sync {
    /// 后端标识（状态行/徽标展示用）。
    fn backend(&self) -> BackendKind;

    /// 会话初始标题（如 shell 名；运行期标题仍以 OSC 0/2 上报为准）。
    fn title(&self) -> String;

    /// 一次性取出会话的字节流。
    fn take_streams(&self) -> io::Result<SessionStreams>;

    /// 视口尺寸变化。本地 = PTY resize；SSH = 窗口变更转义序列。
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;

    /// 请求终止会话（幂等）。
    fn kill(&self);

    /// 探测会话内是否有「运行中」的前台/子进程（关闭二次确认用）。
    ///
    /// 返回 `None` = 无运行中进程 **或** 探测不可用/不可判定——调用方一律
    /// 视为无需确认直接放行（关闭优先于误拦截）。默认实现恒 `None`：
    /// 不覆写的后端（如 SSH 远程，进程表在远端）天然降级为不确认。
    ///
    /// `snapshot` 是调用方持有的共享 /proc 快照槽：实现方按需惰性建立
    /// （`get_or_insert_with`），同一次关闭请求内的多个窗格复用同一份，
    /// 把全表扫描收敛到每请求至多一次。
    fn probe_running(&self, snapshot: &mut Option<ProcessSnapshot>) -> Option<RunningProcess> {
        // 触碰参数防止未用告警；默认实现不读快照。
        let _ = snapshot;
        None
    }
}
