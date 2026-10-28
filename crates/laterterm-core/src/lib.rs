//! LaterTerm 会话抽象层。
//!
//! [`TerminalSession`] 是本地 PTY 与 SSH 远程会话的共同契约：终端视图
//! （laterterm-termview）只面向本 trait 编程，「远程」不是另一个功能，
//! 只是换了 backend 的同一个终端（见 docs/architecture.md）。
//!
//! 本 crate 刻意保持零第三方依赖（纯 std），保证 SSH/编码/trzsz 等后续
//! 扩展不会把传输实现细节泄漏进视图层。

use std::io::{self, Read, Write};

/// 会话后端类型。M3 增加 `Ssh` 变体。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// 本地 PTY（laterterm-local）。
    Local,
}

/// 会话的字节流，由 [`TerminalSession::take_streams`] 一次性移交给视图。
///
/// reader 供视图的读泵线程阻塞读取（EOF 即会话结束信号）；
/// writer 供键盘输入与命令面板下发。
pub struct SessionStreams {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
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
}
