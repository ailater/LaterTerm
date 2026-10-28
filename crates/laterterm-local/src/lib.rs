//! LaterTerm 本地会话：portable-pty + 子进程回收线程。
//!
//! 职责边界（见 docs/architecture.md）：PTY 的开启/spawn/resize/终止全部
//! 收敛在本 crate，应用层只拿到 [`TerminalSession`] 契约；shell 子进程的
//! 回收线程在 spawn 时一并启动，调用方无需关心 wait。

use laterterm_core::{BackendKind, SessionStreams, TerminalSession};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// 本地会话启动参数。
pub struct LocalOptions {
    /// shell 可执行文件路径（如 $SHELL 解析结果）。
    pub shell: String,
    /// 传给 shell 的额外参数。
    pub args: Vec<String>,
    /// 额外环境变量（TERM/COLORTERM 由本 crate 注入默认值）。
    pub env: Vec<(String, String)>,
    /// 初始列数。
    pub cols: u16,
    /// 初始行数。
    pub rows: u16,
}

/// 本地 PTY 会话。
pub struct LocalSession {
    master: Mutex<Box<dyn MasterPty + Send>>,
    streams: Mutex<Option<SessionStreams>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    closing: AtomicBool,
    title: String,
}

impl LocalSession {
    /// 打开 PTY 并启动 shell。
    ///
    /// 同时启动子进程回收线程；`slave` 端在取完读写流后立即关闭，
    /// 保证 shell 退出后 master 读端能收到 EOF。
    pub fn spawn(options: LocalOptions) -> io::Result<Self> {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: options.rows,
                cols: options.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;

        let mut command = CommandBuilder::new(&options.shell);
        command.args(&options.args);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for (key, value) in &options.env {
            command.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;

        let killer = child.clone_killer();
        // 回收线程：wait 阻塞到 shell 退出，防止僵尸进程
        std::thread::spawn(move || {
            if let Err(error) = child.wait() {
                eprintln!("回收 shell 子进程失败: {error}");
            }
        });

        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        let reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        drop(pair.slave);

        let title = Path::new(&options.shell)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| options.shell.clone());

        Ok(Self {
            master: Mutex::new(pair.master),
            streams: Mutex::new(Some(SessionStreams { reader, writer })),
            killer: Mutex::new(Some(killer)),
            closing: AtomicBool::new(false),
            title,
        })
    }
}

impl TerminalSession for LocalSession {
    fn backend(&self) -> BackendKind {
        BackendKind::Local
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn take_streams(&self) -> io::Result<SessionStreams> {
        self.streams
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| io::Error::other("会话字节流已被取走（take_streams 只能调用一次）"))
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    fn kill(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(mut killer) = self.killer.lock().unwrap().take()
            && let Err(error) = killer.kill()
        {
            eprintln!("终止终端会话失败: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn shell_path() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }

    /// 会话契约：echo 一次输出 → 读端拿到字节；二次 take_streams 报错。
    #[test]
    fn echo_output_reaches_reader_and_streams_are_take_once() {
        let session = LocalSession::spawn(LocalOptions {
            shell: shell_path(),
            args: vec!["-c".into(), "printf laterterm-ok".into()],
            env: vec![],
            cols: 20,
            rows: 5,
        })
        .expect("spawn 失败");

        let mut streams = session.take_streams().expect("首次 take_streams");
        let mut output = String::new();
        streams
            .reader
            .read_to_string(&mut output)
            .expect("读到 EOF");
        assert!(output.contains("laterterm-ok"), "实际输出: {output}");

        assert!(session.take_streams().is_err(), "二次 take_streams 应报错");
    }

    /// resize 在会话存活期内应成功（契约要求，不阻塞）。
    #[test]
    fn resize_applies_without_error() {
        let session = LocalSession::spawn(LocalOptions {
            shell: shell_path(),
            args: vec!["-c".into(), "sleep 30".into()],
            env: vec![],
            cols: 20,
            rows: 5,
        })
        .expect("spawn 失败");

        session.resize(80, 24).expect("resize 应成功");
        assert_eq!(session.backend(), BackendKind::Local);
        assert!(!session.title().is_empty());

        session.kill();
        session.kill(); // 幂等：二次调用不应 panic
    }
}
