//! LaterTerm 本地会话：portable-pty + 子进程回收线程。
//!
//! 职责边界（见 docs/architecture.md）：PTY 的开启/spawn/resize/终止全部
//! 收敛在本 crate，应用层只拿到 [`TerminalSession`] 契约；shell 子进程的
//! 回收线程在 spawn 时一并启动，调用方无需关心 wait。

use laterterm_core::{
    BackendKind, ProcessSnapshot, RunningProcess, SessionStreams, TerminalSession,
};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io;
use std::path::{Path, PathBuf};
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
    /// 子进程初始工作目录（新标签/分屏继承焦点窗格 cwd 的通道，来源为
    /// OSC 7 上报）；None = 沿用 portable-pty 默认（继承父进程 cwd）。
    /// 目录不存在时 portable-pty 0.9 静默回退 $HOME（不报错，行为良性）——
    /// 调用方仍应传真实存在的目录，别依赖回退。
    pub cwd: Option<PathBuf>,
    /// 初始列数。
    pub cols: u16,
    /// 初始行数。
    pub rows: u16,
}

/// bash 提示符集成钩子（OSC 133 D/A + OSC 7 cwd），追加进 `PROMPT_COMMAND`：
/// 每次显示提示符前依次发送——上一命令退出码（D）、新提示符开始（A）、
/// 当前目录（OSC 7，`file://` + `$PWD`，空 host + 绝对路径）。
/// `$?`/`$PWD` 保持字面：环境变量赋值不经展开，执行时才由 bash 求值。
const BASH_PROMPT_COMMAND_HOOK: &str = "printf '\\033]133;D;%s\\007' \"$?\"; \
     printf '\\033]133;A\\007'; \
     printf '\\033]7;file://%s\\007' \"$PWD\"";

/// 命令开始钩子（OSC 133 C）：bash 在读入命令、执行之前对 `PS0` 做 prompt
/// 展开——环境继承的 `PS0` 按普通变量取初值，正好用来在输出开始前发 C。
const BASH_PS0_HOOK: &str = "\\e]133;C\\a";

/// bash 发起端最小注入（v1，best-effort）。
///
/// 经 `LocalOptions.env` 注入 `PROMPT_COMMAND`/`PS0`：bash 对环境继承的这两个
/// 变量按普通变量取初值，零 rc 文件改动、随 PTY 子进程生灭无残留。**边界**：
/// 用户 `~/.bashrc` 自行重赋值 `PROMPT_COMMAND`/`PS0` 会覆盖注入，功能静默
/// 失效；zsh/fish 不注入（kitty 式 ZDOTDIR/XDG_DATA_DIRS 接管属 v1 过度设计，
/// 自行安装片段见 README）。
///
/// `parent_prompt_command` 为父进程已有的 `PROMPT_COMMAND`：非空时字面拼接
/// 保底（`原值; 钩子`——bash 以分号顺序执行字符串中的多条命令）。
/// 非 bash 返回空表。
pub fn shell_integration_env(
    shell: &str,
    parent_prompt_command: Option<&str>,
) -> Vec<(String, String)> {
    if !is_bash(shell) {
        return Vec::new();
    }
    let prompt_command = match parent_prompt_command.map(str::trim) {
        Some(parent) if !parent.is_empty() => format!("{parent}; {BASH_PROMPT_COMMAND_HOOK}"),
        _ => BASH_PROMPT_COMMAND_HOOK.to_string(),
    };
    vec![
        ("PROMPT_COMMAND".to_owned(), prompt_command),
        ("PS0".to_owned(), BASH_PS0_HOOK.to_owned()),
    ]
}

/// bash 判定：取路径最后一段与 `bash` 精确比较（覆盖 /bin/bash、
/// /usr/local/bin/bash 等安装位置；`resolve_shell` 已保证无引号/控制字符）。
fn is_bash(shell: &str) -> bool {
    Path::new(shell)
        .file_name()
        .is_some_and(|name| name == "bash")
}

/// 本地 PTY 会话。
pub struct LocalSession {
    master: Mutex<Box<dyn MasterPty + Send>>,
    streams: Mutex<Option<SessionStreams>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    closing: AtomicBool,
    title: String,
    /// shell 主进程 pid（spawn 时 `child.process_id()`，move 进回收线程前取）。
    /// 关闭确认探测的三级判据都以它为锚：pre_exec 已 setsid()+TIOCSCTTY，
    /// 故 shell_pid == pgid == sid。None 仅在平台拿不到 pid（防御口径）。
    /// 非 Linux 的探测降级分支不读它，防 cfg 差异 dead_code 告警。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    shell_pid: Option<u32>,
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
        if let Some(dir) = &options.cwd {
            // 初始工作目录（cwd 继承）：portable-pty 0.9 as_command 对非目录
            // 静默回退 $HOME（不报错），见 LocalOptions.cwd 字段文档
            command.cwd(dir);
        }
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;

        // shell pid 必须在 child move 进回收线程前取走（探测判据的锚点）。
        let shell_pid = child.process_id();
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
            shell_pid,
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

    /// 关闭二次确认探测（仅 Linux 实现，其余平台降级为不确认）。
    ///
    /// 探测时机在关闭请求时同步执行：判据 a/b1 微秒级，判据 b2 走共享
    /// 快照槽——由调用方（一次关闭请求）持有，首个走到 b2 的窗格建表
    /// 一次，后续窗格复用，全表扫描每请求至多一遍。
    fn probe_running(&self, snapshot: &mut Option<ProcessSnapshot>) -> Option<RunningProcess> {
        #[cfg(target_os = "linux")]
        {
            let shell_pid = self.shell_pid?;
            probe::probe_running(
                &self.master,
                shell_pid,
                &self.title,
                snapshot,
                probe::scan_proc_snapshot,
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS/Windows 无 /proc 的低成本等价物（sysctl kinfo_proc /
            // Toolhelp32 快照成本不成比例），按需求定调降级为不确认。
            // tcgetpgrp 本身 POSIX 通用，macOS 未来放开判据 a 的 cfg 即可。
            let _ = (self, snapshot);
            None
        }
    }
}

/// 关闭二次确认的进程探测（Linux /proc 口径）。
///
/// 「运行中」= 三级判据短路组合：
/// - a：前台进程组——`tcgetpgrp(master)` 与 shell_pid 比较（主场景
///   「跑着前台命令」微秒级命中，不触发任何 /proc 扫描）；
/// - b1：exec 替换——shell 被别的程序替换（comm 变了）但 pgid 没变；
/// - b2：子树兜底——后台 job / `sh -c "sleep N; echo"` 类真子进程。
#[cfg(target_os = "linux")]
mod probe {
    use laterterm_core::{ProcEntry, ProcessSnapshot, RunningProcess};
    use portable_pty::MasterPty;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    /// 判据 b1 的常见 shell comm 豁免集合。
    ///
    /// 误报面：交互 shell 里 `exec zsh`/`exec fish` 换 shell 后 comm 不再是
    /// spawn 时的 shell 名，若不豁免则该窗格每次关闭都会误弹确认（直至会话
    /// 结束）；实测 `exec sleep` 后 comm="sleep"（真实程序不在集合，判定力
    /// 不损）。自定义 REPL 不在集合内 → 多弹一次确认，保守方向可接受
    /// （弹窗会如实显示进程名）。spawn 时记录的 shell 名另在
    /// [`is_exempt_shell`] 里兜底（自定义安装路径的 shell）。
    pub(super) const SHELL_COMM: &[&str] = &[
        "sh", "bash", "zsh", "fish", "dash", "ksh", "csh", "tcsh", "nu", "elvish", "pwsh",
        "busybox",
    ];

    /// /proc 全表扫描上限（每请求口径：共享快照至多扫一次，上限随共享）。
    /// 宿主 pid 命名空间共享的容器等极端机器超限时整体放弃判据 b2，
    /// 视为不确认——宁可漏弹不可卡 UI。
    pub(super) const PROC_SCAN_LIMIT: usize = 8192;

    /// 三级判据探测。`scan` 参数化全表扫描函数，测试注入计数闭包验证
    /// 「共享快照每请求只扫一次」。
    pub(super) fn probe_running(
        master: &Mutex<Box<dyn MasterPty + Send>>,
        shell_pid: u32,
        shell_name: &str,
        snapshot: &mut Option<ProcessSnapshot>,
        scan: impl Fn() -> Option<ProcessSnapshot>,
    ) -> Option<RunningProcess> {
        // 判据 a：前台进程组。portable-pty pre_exec 已 setsid()+TIOCSCTTY，
        // shell_pid == pgid；tcgetpgrp 返回别的 pgid = 有前台进程组接管终端
        //（sleep/vim/tmux client 等），前台组长 pid == pgid。
        // None（tcgetpgrp<=0：shell 将死/EOF/EIO）不参与「不等于 shell_pid」
        // 比较，直接落判据 b1——杜绝拿垃圾 pgid 读 /proc/<pgid>/comm。
        if let Ok(master) = master.lock()
            && let Some(pgid) = master.process_group_leader()
            && pgid as u32 != shell_pid
        {
            // 前台组长 pid == pgid；comm 读不到（进程刚退出等）回落占位名
            let name = read_comm(pgid as u32).unwrap_or_else(|| "未知进程".into());
            return Some(RunningProcess { name });
        }
        // 判据 b1：exec 替换。`sh -c "sleep 30"` 里 shell 被 sleep 替换，
        // tcgetpgrp == shell_pid 但 comm 已非 shell 名 → 命中；comm 仍在
        // 豁免集合视为「仍是 shell」（`exec zsh` 换 shell 不该每次关闭都弹）
        // → 落 b2。读不到 comm（shell 已退出/僵尸间隙，僵尸 comm 恒为
        // shell 名无误报）不命中，继续 b2。
        if let Some(comm) = read_comm(shell_pid)
            && !is_exempt_shell(&comm, shell_name)
        {
            return Some(RunningProcess { name: comm });
        }
        // 判据 b2：子树兜底。`sh -c "sleep 30; echo"` 的 sleep 是真子进程、
        // 后台 job 同理——a/b1 都看不出差别。共享快照槽惰性建表（每请求
        // 至多一次全表扫描），以 shell_pid 为根按 ppid 可达、排僵尸。
        // 扫描失败/超限 → 空表，子树判定不命中（= 不确认），与「跳过 b2」
        // 同效且不重试。
        let table = snapshot.get_or_insert_with(|| scan().unwrap_or_default());
        find_running_descendant(table, shell_pid, |comm| is_exempt_shell(comm, shell_name))
            .map(|name| RunningProcess { name })
    }

    /// 「仍是 shell」判定：comm 在常见 shell 豁免集合，或等于本会话 spawn
    /// 的 shell 名。sleep/vim 等真实程序不在集合内，判定力不受损。
    pub(super) fn is_exempt_shell(comm: &str, spawn_shell: &str) -> bool {
        SHELL_COMM.contains(&comm) || comm == spawn_shell
    }

    /// b2 子树判定：以 `root` 为根按 ppid 向下可达的后代（不含根、排僵尸）
    /// 中，返回第一个 comm 不在豁免集合内的进程名。
    /// visited 防 ppid 成环异常数据死循环。
    pub(super) fn find_running_descendant(
        snapshot: &ProcessSnapshot,
        root: u32,
        is_exempt: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let mut pending = vec![root];
        let mut visited = BTreeSet::from([root]);
        while let Some(parent) = pending.pop() {
            for entry in &snapshot.entries {
                if entry.ppid != parent || entry.pid == root || entry.state == 'Z' {
                    continue;
                }
                if !is_exempt(&entry.comm) {
                    return Some(entry.comm.clone());
                }
                if visited.insert(entry.pid) {
                    pending.push(entry.pid);
                }
            }
        }
        None
    }

    /// 读 /proc/<pid>/comm（判据 a/b1 用；内核截 15 字符）。
    fn read_comm(pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|text| text.trim_end().to_string())
            .filter(|text| !text.is_empty())
    }

    /// 单遍扫 /proc/[0-9]+/stat 建全表快照；条目数超 [`PROC_SCAN_LIMIT`]
    /// 返回 None（判据 b2 整体放弃）。单条读取/解析失败（进程瞬时退出、
    /// 假 pid 目录）静默跳过。
    pub(super) fn scan_proc_snapshot() -> Option<ProcessSnapshot> {
        let dir = std::fs::read_dir("/proc").ok()?;
        let mut entries = Vec::new();
        for item in dir.flatten() {
            let Some(pid) = item
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue; // 非 pid 条目（/proc/cpuinfo 等）
            };
            let Some(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok() else {
                continue;
            };
            if let Some(entry) = parse_proc_stat(&stat) {
                entries.push(entry);
            }
            if entries.len() > PROC_SCAN_LIMIT {
                return None;
            }
        }
        Some(ProcessSnapshot { entries })
    }

    /// 解析 /proc/<pid>/stat 单行 → [`ProcEntry`]。
    ///
    /// comm 可含空格与括号（如 `systemd:(sd-)`、`Weird Name (x)`），必须
    /// `rfind(')')` 定位 comm 结束再切后续字段——按空格盲 split 会把 ppid
    /// 取错，树判定整个打歪。字段序：`pid (comm) state ppid pgrp ...`。
    pub(super) fn parse_proc_stat(stat: &str) -> Option<ProcEntry> {
        let close = stat.rfind(')')?;
        let open = stat.find('(')?;
        let pid = stat[..open].trim().parse::<u32>().ok()?;
        let mut fields = stat[close + 1..].split_whitespace();
        let state = fields.next()?.chars().next()?;
        let ppid = fields.next()?.parse::<u32>().ok()?;
        Some(ProcEntry {
            pid,
            ppid,
            state,
            comm: stat[open + 1..close].to_string(),
        })
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

    // ───────── shell 集成环境注入（v1 bash best-effort） ─────────

    /// 非 bash 不注入（zsh/fish/sh/dash 全部落空；kitty 式接管属 v1 过度设计）。
    #[test]
    fn shell_integration_env_only_targets_bash() {
        for shell in ["/usr/bin/zsh", "/bin/sh", "/bin/dash", "/usr/bin/fish", ""] {
            assert!(
                shell_integration_env(shell, None).is_empty(),
                "{shell} 不应注入"
            );
        }
        // 任意安装位置的 bash 都认（按文件名最后一段判定）
        let env = shell_integration_env("/usr/local/bin/bash", None);
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].0, "PROMPT_COMMAND");
        assert_eq!(env[1].0, "PS0");
    }

    /// 钩子字符串逐字钉死：$?/$PWD 必须字面保留（执行时才由 bash 求值）、
    /// PS0 是纯 prompt 转义形态、D/A/C 顺序覆盖 OSC 133 主流程 + OSC 7 cwd。
    #[test]
    fn shell_integration_env_hook_strings_are_exact() {
        let env = shell_integration_env("/bin/bash", None);
        assert_eq!(
            env[0].1,
            "printf '\\033]133;D;%s\\007' \"$?\"; \
             printf '\\033]133;A\\007'; \
             printf '\\033]7;file://%s\\007' \"$PWD\""
        );
        // $ 转义边界：注入值里只有字面 $，无任何被展开/转义掉的痕迹
        assert!(
            env[0].1.contains("\"$?\""),
            "退出码必须是执行期求值: {}",
            env[0].1
        );
        assert!(
            env[0].1.contains("\"$PWD\""),
            "cwd 必须是执行期求值: {}",
            env[0].1
        );
        assert_eq!(env[1].1, "\\e]133;C\\a", "PS0 走 prompt 转义形态");
    }

    /// 父进程已有 PROMPT_COMMAND 时字面拼接保底（`原值; 钩子`）；空/纯空白
    /// 父值视同无值，不产生 `; ;` 空命令。
    #[test]
    fn shell_integration_env_concatenates_parent_prompt_command() {
        let env = shell_integration_env("/bin/bash", Some("echo before"));
        assert!(
            env[0].1.starts_with("echo before; "),
            "父值在前、钩子在后: {}",
            env[0].1
        );
        assert!(env[0].1.contains("133;D"), "钩子本体保留: {}", env[0].1);
        assert_eq!(env[1].1, "\\e]133;C\\a", "PS0 不受父值影响");

        for parent in ["", "   "] {
            let env = shell_integration_env("/bin/bash", Some(parent));
            assert!(
                env[0].1.starts_with("printf"),
                "空白父值不拼接: {}",
                env[0].1
            );
        }
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
            cwd: None,
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
            cwd: None,
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

    /// cwd 字段生效：spawn 传入初始目录，shell 的 `pwd` 应落在该目录
    ///（新标签/分屏继承焦点窗格 cwd 的通道）。目录名带空格与中文，
    /// 钉「路径原样透传、不经 shell 再解析」的口径。
    #[cfg(unix)]
    #[test]
    fn cwd_option_sets_initial_working_directory() {
        // 无 tempfile 依赖：temp_dir 下按 pid 建唯一子目录，测试尾清理。
        // canonicalize 对齐内核 chdir 后的物理路径（temp_dir 可能含符号链接，
        // shell pwd 打印的是 getcwd 的规范路径）
        let dir = std::env::temp_dir().join(format!(
            "laterterm-cwd-test-{}-{}",
            std::process::id(),
            "中文 目录"
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let dir = std::fs::canonicalize(&dir).expect("规范化临时目录");
        let session = LocalSession::spawn(LocalOptions {
            shell: shell_path(),
            args: shell_args("pwd"),
            env: vec![],
            cwd: Some(dir.clone()),
            cols: 20,
            rows: 5,
        })
        .expect("spawn 失败");

        let mut streams = session.take_streams().expect("take_streams");
        let mut output = String::new();
        streams
            .reader
            .read_to_string(&mut output)
            .expect("读到 EOF");
        assert_eq!(
            output.trim_end(),
            dir.to_str().expect("临时目录应为 UTF-8"),
            "pwd 应落在传入的初始目录（含空格与中文）"
        );

        session.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关闭确认探测（三级判据）。真实 PTY 时序敏感（shell exec 完成前后
    /// comm 是父进程名），断言一律走轮询而非固定 sleep；纯函数用合成串。
    #[cfg(target_os = "linux")]
    mod probe_tests {
        use super::super::probe::{
            self, SHELL_COMM, find_running_descendant, is_exempt_shell, parse_proc_stat,
        };
        use super::*;
        use std::cell::Cell;
        use std::thread::sleep;
        use std::time::{Duration, Instant};

        /// 轮询探测直至 `want`（超时 panic）。shell 启动竞态窗口内探测结果
        /// 不稳定（exec 未完成时 comm 是测试二进制名），必须等收敛。
        fn probe_until(
            session: &LocalSession,
            want: impl Fn(Option<String>) -> bool + Copy,
            context: &str,
        ) -> Option<String> {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                let mut snapshot = None;
                let found = session.probe_running(&mut snapshot).map(|p| p.name);
                if want(found.clone()) {
                    return found;
                }
                assert!(
                    Instant::now() < deadline,
                    "探测未收敛（{context}），最后结果: {found:?}"
                );
                sleep(Duration::from_millis(100));
            }
        }

        /// 等 shell 的 comm 稳定为 `expect`（exec 完成的可靠信号）。
        fn wait_comm(pid: u32, expect: &str) {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map(|text| text.trim_end().to_string());
                if comm.as_deref().is_ok_and(|comm| comm == expect) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "shell comm 未就绪，最后: {comm:?}"
                );
                sleep(Duration::from_millis(100));
            }
        }

        fn spawn_shell(args: &[&str]) -> LocalSession {
            LocalSession::spawn(LocalOptions {
                shell: "/bin/sh".into(),
                args: args.iter().map(|a| (*a).to_string()).collect(),
                env: vec![],
                cwd: None,
                cols: 20,
                rows: 5,
            })
            .expect("spawn 失败")
        }

        /// 判据 a 主场景：交互 shell 跑前台命令（sleep），tcgetpgrp ≠ shell
        /// pid，进程组名即「sleep」。这里借 `sh -c "sleep 30"`（exec 优化后
        /// 走 b1、未优化走 b2，两条路径的结论一致）钉「运行中必命中」。
        #[test]
        fn running_sleep_is_detected() {
            let session = spawn_shell(&["-c", "sleep 30"]);
            let found = probe_until(
                &session,
                |found| found.as_deref() == Some("sleep"),
                "sleep 应命中",
            );
            assert_eq!(found.as_deref(), Some("sleep"));
            session.kill();
        }

        /// 空闲交互 shell（无前台命令、无子进程）不应确认。
        /// 先等 exec 完成（comm=父进程名 → sh），再断言探测 None——绕开
        /// 启动竞态窗口（该窗口内 b1 会拿测试二进制名误命中）。
        #[test]
        fn idle_interactive_shell_is_not_running() {
            let session = spawn_shell(&[]);
            let pid = session.shell_pid.expect("shell pid");
            wait_comm(pid, "sh");
            probe_until(
                &session,
                |found| found.is_none(),
                "空闲 shell 应探测为无运行中",
            );
            session.kill();
        }

        /// `exec 同款豁免 shell` 后不确认：换 shell（comm 仍在豁免集合）
        /// 不该让每次关闭都弹窗，直至会话结束。
        #[test]
        fn exec_into_exempt_shell_is_not_running() {
            let session = spawn_shell(&["-c", "exec sh"]);
            let pid = session.shell_pid.expect("shell pid");
            // exec 前后 comm 都是 sh：等就绪即可（exec 完成 + 新 shell 交互
            // 初始化），随后的 None 收敛由轮询兜住初始化瞬态
            wait_comm(pid, "sh");
            sleep(Duration::from_millis(300));
            probe_until(
                &session,
                |found| found.is_none(),
                "exec 豁免 shell 应探测为无运行中",
            );
            session.kill();
        }

        /// shell 退出后（进程消失）不再确认：探测放行关闭。
        #[test]
        fn exited_shell_is_not_running() {
            let session = spawn_shell(&["-c", "sleep 0.05"]);
            probe_until(
                &session,
                |found| found.is_none(),
                "shell 退出后应探测为无运行中",
            );
            // 稳定复测一次（防退出过渡态偶然通过）
            sleep(Duration::from_millis(200));
            let mut snapshot = None;
            assert!(session.probe_running(&mut snapshot).is_none());
        }

        /// 共享快照：同一请求（同一 snapshot 槽）内两会话探测，全表扫描
        /// 只发生一次（第二个会话复用），且两者都能命中各自的 sleep 子进程。
        #[test]
        fn shared_snapshot_scans_once_across_sessions() {
            let first = spawn_shell(&["-c", "sleep 30; echo done"]);
            let second = spawn_shell(&["-c", "sleep 30; echo done"]);
            // 等两个 sleep 子进程都就位（b2 命中前置条件）
            probe_until(
                &first,
                |found| found.as_deref() == Some("sleep"),
                "首个会话 sleep 就位",
            );
            probe_until(
                &second,
                |found| found.as_deref() == Some("sleep"),
                "第二个会话 sleep 就位",
            );

            // 正式请求：一个槽、两次探测；计数扫描闭包经 Cell 捕获保持 Copy
            let mut snapshot = None;
            let scans = Cell::new(0usize);
            let found_first = probe::probe_running(
                &first.master,
                first.shell_pid.expect("shell pid"),
                &first.title,
                &mut snapshot,
                || {
                    scans.set(scans.get() + 1);
                    probe::scan_proc_snapshot()
                },
            );
            let found_second = probe::probe_running(
                &second.master,
                second.shell_pid.expect("shell pid"),
                &second.title,
                &mut snapshot,
                || {
                    scans.set(scans.get() + 1);
                    probe::scan_proc_snapshot()
                },
            );
            assert_eq!(found_first.map(|p| p.name), Some("sleep".into()));
            assert_eq!(found_second.map(|p| p.name), Some("sleep".into()));
            assert_eq!(scans.get(), 1, "共享快照槽应只扫描一次");
            first.kill();
            second.kill();
        }

        /// stat 解析：comm 含空格/括号时 rfind(')') 口径不歪字段。
        #[test]
        fn parse_proc_stat_handles_commas_spaces_and_parens() {
            let entry = parse_proc_stat("42 (weird proc) S 1 7 1").unwrap();
            assert_eq!(entry.pid, 42);
            assert_eq!(entry.comm, "weird proc");
            assert_eq!(entry.state, 'S');
            assert_eq!(entry.ppid, 1);

            // comm 自带括号（systemd 用户实例真实形态）
            let entry = parse_proc_stat("7 (systemd:(sd-)) S 1 7 1").unwrap();
            assert_eq!(entry.comm, "systemd:(sd-)");
            assert_eq!(entry.state, 'S');
            assert_eq!(entry.ppid, 1);

            // 非法串（空/缺字段/pid 非数字）一律 None
            assert!(parse_proc_stat("").is_none());
            assert!(parse_proc_stat("(no pid) S 1").is_none());
            assert!(parse_proc_stat("x (bad) S 1").is_none());
            assert!(parse_proc_stat("9 (truncated) S").is_none());
        }

        /// 豁免判定：常见 shell 与 spawn 名豁免；真实程序不豁免。
        #[test]
        fn exempt_shell_names_vs_real_programs() {
            for comm in SHELL_COMM {
                assert!(is_exempt_shell(comm, "bash"), "{comm} 应豁免");
            }
            assert!(is_exempt_shell("my-custom-shell", "my-custom-shell"));
            for comm in ["sleep", "vim", "python3", "my-custom-shell"] {
                assert!(!is_exempt_shell(comm, "bash"), "{comm} 不应豁免");
            }
        }

        /// 子树判定：真子进程命中、豁免后代穿透到孙代、僵尸排除、环数据不死循环。
        #[test]
        fn subtree_detection_walks_descendants() {
            use laterterm_core::{ProcEntry, ProcessSnapshot};
            let snapshot = |entries: &[(u32, u32, char, &str)]| ProcessSnapshot {
                entries: entries
                    .iter()
                    .map(|&(pid, ppid, state, comm)| ProcEntry {
                        pid,
                        ppid,
                        state,
                        comm: comm.into(),
                    })
                    .collect(),
            };
            let exempt = |comm: &str| is_exempt_shell(comm, "sh");

            // 直接子进程是真实程序 → 命中
            let table = snapshot(&[(2, 1, 'S', "sleep")]);
            assert_eq!(
                find_running_descendant(&table, 1, exempt),
                Some("sleep".into())
            );
            // 子进程是 shell（豁免）且无后代 → 不命中
            let table = snapshot(&[(2, 1, 'S', "bash")]);
            assert_eq!(find_running_descendant(&table, 1, exempt), None);
            // 豁免子进程的后代有真实程序 → 穿透命中孙代
            let table = snapshot(&[(2, 1, 'S', "bash"), (3, 2, 'S', "python3")]);
            assert_eq!(
                find_running_descendant(&table, 1, exempt),
                Some("python3".into())
            );
            // 僵尸后代不算运行中
            let table = snapshot(&[(2, 1, 'Z', "sleep")]);
            assert_eq!(find_running_descendant(&table, 1, exempt), None);
            // ppid 成环异常数据不死循环
            let table = snapshot(&[(2, 3, 'S', "bash"), (3, 2, 'S', "bash")]);
            assert_eq!(find_running_descendant(&table, 1, exempt), None);
        }
    }
}
