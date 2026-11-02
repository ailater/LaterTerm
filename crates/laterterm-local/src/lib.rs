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

/// 返回当前平台的默认 shell 可执行文件。
///
/// 仅接受绝对路径且不含控制字符或引号，避免把环境变量误当作带参数的
/// shell 命令拼接执行。路径中的空格是合法的，环境变量读取集中在这里，调用方无需修改进程环境。
pub fn default_shell() -> String {
    #[cfg(unix)]
    {
        default_shell_from_env(std::env::var("SHELL").ok().as_deref())
    }
    #[cfg(windows)]
    {
        default_shell_from_env(
            std::env::var("COMSPEC").ok().as_deref(),
            std::env::var("SystemRoot").ok().as_deref(),
        )
    }
}

#[cfg(unix)]
fn default_shell_from_env(shell: Option<&str>) -> String {
    shell
        .filter(|value| is_valid_shell_path(value))
        .unwrap_or("/bin/bash")
        .to_owned()
}

#[cfg(unix)]
fn is_valid_shell_path(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\'' | '"'))
        && Path::new(value).is_absolute()
}

#[cfg(windows)]
fn default_shell_from_env(comspec: Option<&str>, system_root: Option<&str>) -> String {
    if comspec.is_some_and(is_valid_shell_path) {
        return comspec.unwrap().to_owned();
    }
    if let Some(root) = system_root.filter(|value| is_valid_shell_path(value)) {
        return Path::new(root)
            .join("System32")
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned();
    }
    "cmd.exe".to_owned()
}

#[cfg(windows)]
fn is_valid_shell_path(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\'' | '"'))
        && Path::new(value).is_absolute()
}

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

    #[cfg(unix)]
    fn shell_path() -> String {
        "/bin/sh".into()
    }

    #[cfg(windows)]
    fn shell_path() -> String {
        default_shell()
    }

    fn shell_args(command: &str) -> Vec<String> {
        #[cfg(unix)]
        {
            vec!["-c".into(), command.into()]
        }
        #[cfg(windows)]
        {
            vec!["/D".into(), "/S".into(), "/C".into(), command.into()]
        }
    }

    #[cfg(unix)]
    fn echo_command() -> &'static str {
        "printf laterterm-ok"
    }

    #[cfg(windows)]
    fn echo_command() -> &'static str {
        "echo laterterm-ok"
    }

    #[cfg(unix)]
    fn long_command() -> &'static str {
        "sleep 30"
    }

    #[cfg(windows)]
    fn long_command() -> &'static str {
        "ping -n 30 127.0.0.1 >NUL"
    }

    #[cfg(unix)]
    #[test]
    fn default_shell_keeps_valid_absolute_shell() {
        assert_eq!(default_shell_from_env(Some("/bin/sh")), "/bin/sh");
        assert_eq!(
            default_shell_from_env(Some("/opt/My Shell/bin/sh")),
            "/opt/My Shell/bin/sh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn default_shell_rejects_invalid_values() {
        assert_eq!(default_shell_from_env(None), "/bin/bash");
        assert_eq!(default_shell_from_env(Some("bash -l")), "/bin/bash");
        assert_eq!(default_shell_from_env(Some("relative-shell")), "/bin/bash");
    }

    #[cfg(windows)]
    #[test]
    fn default_shell_uses_comspec_then_system_root() {
        assert_eq!(
            default_shell_from_env(Some(r"C:\custom\cmd.exe"), Some(r"C:\Windows")),
            r"C:\custom\cmd.exe"
        );
        assert_eq!(
            default_shell_from_env(None, Some(r"C:\Windows")),
            r"C:\Windows\System32\cmd.exe"
        );
        assert_eq!(default_shell_from_env(Some(""), None), "cmd.exe");
        assert_eq!(
            default_shell_from_env(Some(r"C:\Program Files\Shell\cmd.exe"), None),
            r"C:\Program Files\Shell\cmd.exe"
        );
    }

    /// 会话契约：echo 一次输出 → 读端拿到字节；二次 take_streams 报错。
    #[test]
    fn echo_output_reaches_reader_and_streams_are_take_once() {
        let session = LocalSession::spawn(LocalOptions {
            shell: shell_path(),
            args: shell_args(echo_command()),
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
            args: shell_args(long_command()),
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
