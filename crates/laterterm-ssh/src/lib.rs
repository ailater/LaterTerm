//! LaterTerm SSH 会话后端。
//!
//! 实现 [`laterterm_core::TerminalSession`]：同一个 termview，换 backend。
//! 架构口径（docs/architecture.md）：**tokio 仅限网络层**——专用网络线程跑
//! current_thread runtime，SSH 读写经无界通道桥接为阻塞字节流，
//! 视图侧（termview）的读泵/写端完全不感知异步。
//!
//! 认证（M3）：密码 / 私钥文件（OpenSSH 格式，可带口令）/ ssh-agent
//! （`SSH_AUTH_SOCK`，逐身份尝试）。
//!
//! 主机密钥（M3）：`AcceptAll`（显式风险）或 `KnownHosts`——OpenSSH
//! `known_hosts` 兼容的 TOFU（首次学习、比对通过、变更拒绝），
//! 默认路径 `~/.ssh/known_hosts`，与系统既有信任直接共享。
//!
//! GBK 转码与 trzsz 仍是 M3 后续：本层透传字节，编码转换挂在
//! take_streams 返回的 reader/writer 装饰器上是既定演进位。

pub mod encoding;
pub mod forward;
pub mod sftp;

pub use encoding::SessionEncoding;
use forward::RemoteForwardRouter;
pub use forward::{PortForwardEvent, TunnelKind, TunnelSpec, TunnelState};
use laterterm_core::{BackendKind, SessionStreams, TerminalSession};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, decode_secret_key};
use std::io;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

/// 主机密钥校验策略。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// 接受任意主机密钥（**有中间人风险**，仅用于一次性/测试场景）。
    #[default]
    AcceptAll,
    /// OpenSSH known_hosts 兼容的 TOFU：
    /// 首次连接记录密钥；此后比对一致放行；**不一致拒绝连接**（密钥变更
    /// 视为潜在的中间人攻击）。`path` 为 `None` 用默认 `~/.ssh/known_hosts`。
    KnownHosts { path: Option<PathBuf> },
    /// 按主机/端口查询信任链；有记录即定案，不匹配或读取失败不得回退。
    KnownHostsWithFallback {
        path: PathBuf,
        fallback_paths: Vec<PathBuf>,
    },
}

/// 认证方式（兼容配置层的预加载私钥 API）。
#[derive(Clone)]
pub enum AuthMethod {
    Password {
        password: String,
    },
    KeyboardInteractive {
        secret: String,
    },
    PrivateKey {
        key_path: PathBuf,
        passphrase: Option<String>,
    },
    PreparedPrivateKey {
        keys: Vec<Arc<PrivateKey>>,
    },
    Agent,
}
impl std::fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent => f.write_str("Agent"),
            Self::Password { .. } => f.write_str("Password { password: [REDACTED] }"),
            Self::KeyboardInteractive { .. } => {
                f.write_str("KeyboardInteractive { secret: [REDACTED] }")
            }
            Self::PrivateKey { key_path, .. } => f
                .debug_struct("PrivateKey")
                .field("key_path", key_path)
                .field("passphrase", &"[REDACTED]")
                .finish(),
            Self::PreparedPrivateKey { keys } => f
                .debug_struct("PreparedPrivateKey")
                .field("keys", &format_args!("[{} keys]", keys.len()))
                .finish(),
        }
    }
}
/// 加密私钥缺少口令或口令错误。可从 `io::Error::get_ref()` downcast。
#[derive(Debug)]
pub struct PrivateKeyPassphraseRequired {
    pub key_path: PathBuf,
}

impl std::fmt::Display for PrivateKeyPassphraseRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("私钥需要正确的解密口令")
    }
}

impl std::error::Error for PrivateKeyPassphraseRequired {}

/// 在调用线程准备私钥；应由 app 后台线程调用，不访问密钥环。
///
/// 指定路径仅加载该文件，仅支持 `~/` 展开；未指定时严格按
/// `~/.ssh/id_ed25519`、`~/.ssh/id_rsa` 顺序加载存在的文件，不扫描目录。
/// 任一存在的候选无法加载时立即返回错误（不静默跳过加密/损坏文件）。
pub fn prepare_private_key(
    key_path: Option<&std::path::Path>,
    passphrase: Option<&str>,
) -> io::Result<AuthMethod> {
    // 绝对路径不依赖 HOME 是否存在。
    if let Some(path) = key_path
        && !path.starts_with("~/")
    {
        return Ok(AuthMethod::PreparedPrivateKey {
            keys: vec![Arc::new(read_private_key(path, passphrase)?)],
        });
    }
    let home = std::env::home_dir()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "无法确定用户主目录"))?;
    prepare_private_key_in(key_path, passphrase, &home)
}

fn prepare_private_key_in(
    key_path: Option<&std::path::Path>,
    passphrase: Option<&str>,
    home: &std::path::Path,
) -> io::Result<AuthMethod> {
    let paths = match key_path {
        Some(path) => vec![match path.strip_prefix("~/") {
            Ok(relative) => home.join(relative),
            Err(_) => path.to_path_buf(),
        }],
        None => vec![home.join(".ssh/id_ed25519"), home.join(".ssh/id_rsa")],
    };
    let mut keys = Vec::new();
    for path in paths {
        match read_private_key(&path, passphrase) {
            Ok(key) => keys.push(Arc::new(key)),
            Err(error) if key_path.is_none() && error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    if keys.is_empty() {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "未找到默认 SSH 私钥（id_ed25519 / id_rsa）",
        ));
    }
    Ok(AuthMethod::PreparedPrivateKey { keys })
}

fn read_private_key(path: &std::path::Path, passphrase: Option<&str>) -> io::Result<PrivateKey> {
    let secret = std::fs::read_to_string(path)?;
    // 先识别加密容器，避免损坏的普通私钥被误判为需要口令。
    let encrypted = PrivateKey::from_openssh(&secret)
        .map(|key| key.is_encrypted())
        .unwrap_or(false)
        || secret.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
        || secret.contains("Proc-Type: 4,ENCRYPTED")
        || (secret.starts_with("PuTTY-User-Key-File-")
            && secret
                .lines()
                .any(|line| line.starts_with("Encryption: ") && line != "Encryption: none"));
    decode_secret_key(&secret, passphrase).map_err(|_| {
        if encrypted {
            io::Error::new(
                ErrorKind::PermissionDenied,
                PrivateKeyPassphraseRequired {
                    key_path: path.to_path_buf(),
                },
            )
        } else {
            // 不传播解析器的错误文本，避免凭据内容进入日志。
            io::Error::new(ErrorKind::InvalidData, "读取私钥失败：格式无效或不支持")
        }
    })
}

#[cfg(test)]
mod private_key_tests {
    use super::*;
    use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};
    use std::path::Path;

    fn write_key(path: &Path, password: Option<&str>) -> PrivateKey {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let stored = match password {
            Some(password) => key.encrypt(&mut rand::rng(), password).unwrap(),
            None => key.clone(),
        };
        std::fs::write(path, stored.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        key
    }

    #[test]
    fn credentials_debug_is_redacted() {
        for auth in [
            AuthMethod::Password {
                password: "sensitive-password".into(),
            },
            AuthMethod::KeyboardInteractive {
                secret: "sensitive-password".into(),
            },
            AuthMethod::PrivateKey {
                key_path: "test-key".into(),
                passphrase: Some("sensitive-password".into()),
            },
        ] {
            assert!(!format!("{auth:?}").contains("sensitive-password"));
        }
    }

    #[test]
    fn discovery_expansion_explicit_and_missing_paths() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare_private_key_in(None, None, home.path())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        let first = write_key(&home.path().join(".ssh/id_ed25519"), None);
        let second = write_key(&home.path().join(".ssh/id_rsa"), None);
        // 不扫描任意名称，即使该文件不是有效私钥。
        std::fs::write(home.path().join(".ssh/other"), "invalid").unwrap();
        let AuthMethod::PreparedPrivateKey { keys } =
            prepare_private_key_in(None, None, home.path()).unwrap()
        else {
            panic!("应预加载")
        };
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].public_key(), first.public_key());
        assert_eq!(keys[1].public_key(), second.public_key());
        for path in [
            PathBuf::from("~/.ssh/id_rsa"),
            home.path().join(".ssh/id_rsa"),
        ] {
            let AuthMethod::PreparedPrivateKey { keys } =
                prepare_private_key_in(Some(&path), None, home.path()).unwrap()
            else {
                panic!("应预加载")
            };
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].public_key(), second.public_key());
        }
        for path in [
            "~/.ssh/missing",
            "$HOME/.ssh/id_ed25519",
            "~someone/.ssh/id_ed25519",
        ] {
            assert_eq!(
                prepare_private_key_in(Some(Path::new(path)), None, home.path())
                    .unwrap_err()
                    .kind(),
                ErrorKind::NotFound
            );
        }
    }

    #[test]
    fn encrypted_keys_require_correct_exact_passphrase() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("encrypted");
        let expected = write_key(&path, Some("  real secret  "));
        for password in [
            None,
            Some(""),
            Some("   "),
            Some("wrong-secret"),
            Some("real secret"),
        ] {
            let error = prepare_private_key_in(Some(&path), password, home.path()).unwrap_err();
            let required = error
                .get_ref()
                .unwrap()
                .downcast_ref::<PrivateKeyPassphraseRequired>()
                .expect("可识别口令错误");
            assert_eq!(required.key_path, path);
            assert!(!format!("{error:?}").contains("wrong-secret"));
            assert!(!error.to_string().contains("real secret"));
        }
        let auth =
            prepare_private_key_in(Some(&path), Some("  real secret  "), home.path()).unwrap();
        assert!(!format!("{auth:?}").contains("PRIVATE KEY"));
        let AuthMethod::PreparedPrivateKey { keys } = auth else {
            panic!("应预加载")
        };
        assert_eq!(keys[0].public_key(), expected.public_key());
        // 空白是合法口令，不能 trim。
        write_key(&path, Some("   "));
        assert!(prepare_private_key_in(Some(&path), Some("   "), home.path()).is_ok());
    }

    #[test]
    fn invalid_key_is_not_a_passphrase_prompt() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("invalid");
        std::fs::write(&path, "not a private key").unwrap();
        let error = prepare_private_key_in(Some(&path), None, home.path()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<PrivateKeyPassphraseRequired>()
                .is_none()
        );
    }
}

/// 认证方式。
#[derive(Clone)]
pub enum SshAuth {
    /// 密码认证。
    Password(String),
    /// 跳板机多轮口令认证（最多八轮）。
    KeyboardInteractive(String),
    /// 私钥文件（OpenSSH 格式；`passphrase` 为私钥口令，未加密传 `None`）。
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// 预加载私钥。
    PreparedPrivateKey { keys: Vec<Arc<PrivateKey>> },
    /// ssh-agent（读 `SSH_AUTH_SOCK`；依次尝试 agent 持有的全部身份）。
    Agent,
}

impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password(_) => f.write_str("Password([REDACTED])"),
            Self::KeyboardInteractive(_) => f.write_str("KeyboardInteractive([REDACTED])"),
            Self::KeyFile { path, .. } => f
                .debug_struct("KeyFile")
                .field("path", path)
                .field("passphrase", &"[REDACTED]")
                .finish(),
            Self::PreparedPrivateKey { keys } => f
                .debug_tuple("PreparedPrivateKey")
                .field(&format_args!("[{} keys]", keys.len()))
                .finish(),
            Self::Agent => f.write_str("Agent"),
        }
    }
}

/// RSA 密钥的签名散列：SHA-256（russh 对 RSA 传 `None` 会落回 SHA-1
/// 遗留算法，显式选 256 与 OpenSSH 默认一致）；其他算法忽略该参数。
fn hash_alg_for(key: &russh::keys::PublicKey) -> Option<russh::keys::HashAlg> {
    match key.algorithm() {
        russh::keys::Algorithm::Rsa { .. } => Some(russh::keys::HashAlg::Sha256),
        _ => None,
    }
}

/// TOFU 判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostKeyVerdict {
    /// 已记录且一致（放行）。
    Match,
    /// 该主机有记录但无一匹配（密钥变更，拒绝）。
    Mismatch,
    /// 无该主机的记录（首次连接，学习后放行）。
    Unknown,
}

/// TOFU 判定（纯逻辑）：按 OpenSSH 语义只比较 key_data（算法 + 密钥材料），
/// 注释不参与身份——russh 自带的 check 把注释计入相等，且其 learn 写注释、
/// 回读丢弃，带注释密钥 learn→check 必假红，故自行实现。
fn verify_host_key(
    recorded: &[(usize, russh::keys::PublicKey)],
    key: &russh::keys::PublicKey,
) -> HostKeyVerdict {
    if recorded.is_empty() {
        return HostKeyVerdict::Unknown;
    }
    if recorded
        .iter()
        .any(|(_, entry)| entry.key_data() == key.key_data())
    {
        HostKeyVerdict::Match
    } else {
        HostKeyVerdict::Mismatch
    }
}

/// SSH 会话启动参数。
/// 一个 SSH 端点（主机 + 凭据 + 主机密钥策略）：目标机与跳板机共用。
#[derive(Clone, Debug)]
pub struct SshEndpoint {
    /// 主机名或 IP。
    pub host: String,
    /// 端口（`None` = 默认 22）。
    pub port: Option<u16>,
    /// 登录用户名。
    pub username: String,
    /// 认证方式。
    pub auth: SshAuth,
    /// 主机密钥策略（跳板机与目标机各自独立校验）。
    pub host_key_policy: HostKeyPolicy,
    /// 会话字符编码（GBK/GB18030 遗留环境自动转 UTF-8；默认 UTF-8 直通）。
    pub encoding: SessionEncoding,
}

impl SshEndpoint {
    /// 端口补默认值。
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(22)
    }

    /// 端点标题：`user@host`（端口非 22 时带端口，与 ssh 命令行习惯一致）。
    pub fn title(&self) -> String {
        if self.port() == 22 {
            format!("{}@{}", self.username, self.host)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port())
        }
    }
}

/// SSH 会话启动参数。
pub struct SshOptions {
    /// 目标机（最终建立 shell 会话的主机）。
    pub endpoint: SshEndpoint,
    /// 跳板链（ProxyJump 语义，`ssh -J` 顺序）：从外到内，第 0 项最先直连；
    /// 空 = 直连。链上任一中间跳的 Handle 都必须保活到会话结束（drop 即断流）。
    pub jump: Vec<SshEndpoint>,
    /// 端口转发隧道（enabled 项；L/R 都打在目标机 Handle 上，跳板穿透自动
    /// 成立）。连接期任一条失败 = 整连硬失败（与 `ssh -L` 一致）。
    pub tunnels: Vec<TunnelSpec>,
    /// 初始列数。
    pub cols: u16,
    /// 初始行数。
    pub rows: u16,
}

impl SshOptions {
    /// 标签标题：目标机为主；经跳板机时按链路顺序后缀 ` ⇢ 跳1 ⇢ 跳2`，
    /// 一眼区分直连/穿透与走的是哪条链。
    pub fn title(&self) -> String {
        let mut title = self.endpoint.title();
        for jump in &self.jump {
            title = format!("{title} ⇢ {}", jump.title());
        }
        title
    }
}

/// 连接超时秒数：基础 15s，每多一跳 +10s（多跳握手逐跳串行）。
/// 语义是「建连+认证+PTY/shell」——隧道建立不在此段（独立预算见
/// [`tunnel_budget_secs`]，新阶段独立定价，不动本公式与既有测试）。
/// spawn 侧 `recv_timeout` 必须用「本公式 + 隧道段 + 1s」，两处联动否则
/// 多跳必超时假失败。
fn connect_timeout_secs(jump_hops: usize) -> u64 {
    15 + 10 * jump_hops as u64
}

/// 隧道建立预算：5s 起步盖 L 本地 bind 开销（毫秒级），每条 +2s——R 每条
/// 1 次全局请求往返，对 1s RTT 链路留 2 倍余量。spawn 侧总预算 =
/// [`connect_timeout_secs`] + 本公式 + 1s，漏改任一侧会让长隧道配置下
/// spawn 先超时、网络线程真实原因（隧道建立超时）被吞。
fn tunnel_budget_secs(tunnels: usize) -> u64 {
    5 + 2 * tunnels as u64
}

/// spawn 侧总预算（含收尾 +1s 让网络线程先报真实原因）：
/// 连接段按跳数缩放；**仅当配置了隧道才加隧道段**——无隧道时与旧公式
/// 逐秒一致（无隧道不进入隧道建立阶段，自然不该多付 5s 起步价）。
fn spawn_timeout_secs(jump_hops: usize, tunnels: usize) -> u64 {
    let tunnel_stage = (tunnels > 0).then(|| tunnel_budget_secs(tunnels));
    connect_timeout_secs(jump_hops) + tunnel_stage.unwrap_or(0) + 1
}

/// 网络线程内的控制指令（tokio 无界通道，同步侧 send 永不阻塞）。
enum SessionCmd {
    Resize {
        cols: u32,
        rows: u32,
    },
    /// 已连接会话内启停隧道（hub 按 spec 差异增删；v1 UI 未接入运行时编辑，
    /// 改动默认下次连接生效——本指令是运行时启停的既有入口，见 roadmap）。
    SetTunnels(Vec<TunnelSpec>),
    Shutdown,
}

/// SSH 远程会话。
///
/// 生命周期：`spawn` 起网络线程（连接/认证/PTY/shell 一次完成）；
/// 视图 `take_streams` 取走阻塞字节流；`resize`/`kill` 经控制通道转发；
/// 网络线程在通道关闭或断连指令后自行退出（reader 端读到 EOF）。
pub struct SshSession {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SessionCmd>,
    streams: Mutex<Option<SessionStreams>>,
    title: String,
    closing: AtomicBool,
    /// 非预期退出标记：网络线程在连接层死亡路径置位（channel wait None /
    /// window_change 失败），供上层断线自动重连判别（`exit_unexpected`）。
    /// 用户 exit（Eof|Close）与用户关闭（Shutdown）不置位。
    unexpected_exit: Arc<AtomicBool>,
    /// 端口转发事件缓冲：网络线程的 ForwardHub 投递，UI 侧每秒 drain
    /// （`take_port_forward_events`，take_streams 同款一次性语义）。
    port_forward_events: Arc<Mutex<Vec<PortForwardEvent>>>,
}

/// 网络线程就绪后移交给 `spawn` 侧的三件套：写桥发送端、控制桥发送端、
/// 读桥接收端（`Err` = 连接/认证/开通道任一步失败的原因）。
type SessionHandles = io::Result<(
    tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    tokio::sync::mpsc::UnboundedSender<SessionCmd>,
    std_mpsc::Receiver<Vec<u8>>,
)>;

impl SshSession {
    /// 连接并启动远程 shell。
    ///
    /// 连接、认证、开 PTY、请求 shell 在网络线程内一次完成；任何一步
    /// 失败都返回错误，网络线程随之退出（不会泄漏半开连接）。
    pub fn spawn(options: SshOptions) -> io::Result<Self> {
        let title = options.title();
        // 编码在 options move 进网络线程前取出（UTF-8 直通零开销）
        let encoding = options.endpoint.encoding;
        // 超时预算（单一事实源 spawn_timeout_secs：连接段 + 隧道段 + 1s）；
        // 无隧道时不加隧道段，与旧公式逐秒一致。两段公式与网络线程内独立
        // timeout 块联动；+1s 让网络线程先报真实原因（「SSH 连接超时」vs
        // 「隧道建立超时」）
        let timeout_secs = spawn_timeout_secs(options.jump.len(), options.tunnels.len());
        let tunnel_count = options.tunnels.len();
        let (ready_tx, ready_rx) = std_mpsc::channel::<SessionHandles>();
        // 非预期退出标记：spawn 侧持有、clone 进网络线程（两处连接层死亡
        // break 前置位），SshSession 构造后经 exit_unexpected 读出
        let unexpected_exit = Arc::new(AtomicBool::new(false));
        // 隧道事件缓冲：spawn 侧创建、clone 进网络线程的 ForwardHub 投递
        let port_forward_events: Arc<Mutex<Vec<PortForwardEvent>>> = Arc::default();

        let network = std::thread::Builder::new()
            .name("laterterm-ssh".into())
            .spawn({
                let unexpected_exit = Arc::clone(&unexpected_exit);
                let port_forward_events = Arc::clone(&port_forward_events);
                move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("构建 tokio runtime 失败");
                    runtime.block_on(network_main(
                        options,
                        ready_tx,
                        unexpected_exit,
                        port_forward_events,
                    ));
                }
            })
            .map_err(io::Error::other)?;

        let _ = network; // 线程自行退出，不需要 join
        let (writer_tx, cmd_tx, data_rx) = ready_rx
            .recv_timeout(std::time::Duration::from_secs(timeout_secs))
            .map_err(|_| {
                // 有隧道时总预算含隧道段，如实标注防误判成建连慢
                let label = if tunnel_count > 0 {
                    "（含隧道建立）"
                } else {
                    ""
                };
                io::Error::new(
                    ErrorKind::TimedOut,
                    format!("SSH 连接超时（{timeout_secs}s）{label}"),
                )
            })??;

        let streams = SessionStreams {
            reader: Box::new(ChannelReader {
                rx: data_rx,
                pending: Vec::new(),
            }),
            writer: Box::new(ChannelWriter { tx: writer_tx }),
        };
        Ok(Self {
            cmd_tx,
            streams: Mutex::new(Some(encoding.apply(streams))),
            title,
            closing: AtomicBool::new(false),
            unexpected_exit,
            port_forward_events,
        })
    }

    /// 已连接会话内启停隧道（v1 UI 未接入运行时编辑：改动默认下次连接生效，
    /// 本入口供后续版本/调试使用）。diff 的多次往返在 select 臂 spawn 执行，
    /// 不阻塞会话消息循环。
    pub fn set_tunnels(&self, tunnels: Vec<TunnelSpec>) -> io::Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Ok(());
        }
        self.cmd_tx
            .send(SessionCmd::SetTunnels(tunnels))
            .map_err(|_| io::Error::other("SSH 会话已结束"))
    }

    /// 一次性取走全部端口转发事件（take_streams 同款语义）：窗格 watcher
    /// 每秒调用，hub 侧只追加不读。
    pub fn take_port_forward_events(&self) -> Vec<PortForwardEvent> {
        std::mem::take(&mut self.port_forward_events.lock().unwrap())
    }
}

impl TerminalSession for SshSession {
    // 关闭二次确认探测（probe_running）刻意不覆写：远程进程表在服务器侧，
    // 本地无低成本探测手段，走 trait 默认 None = 远程会话一律不确认直接
    // 关闭。远程探测留后续里程碑（需经 SSH 通道查询远端前台/子进程）。

    fn backend(&self) -> BackendKind {
        BackendKind::Ssh
    }

    /// 非预期退出（连接层死亡）判别：网络线程在 wait None / window_change
    /// 失败两处置位（见 select 循环注记）；用户 exit / Shutdown 不置位。
    fn exit_unexpected(&self) -> bool {
        self.unexpected_exit.load(Ordering::Acquire)
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
        if self.closing.load(Ordering::Acquire) {
            return Ok(());
        }
        self.cmd_tx
            .send(SessionCmd::Resize {
                cols: cols as u32,
                rows: rows as u32,
            })
            .map_err(|_| io::Error::other("SSH 会话已结束"))
    }

    fn kill(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        // 网络线程可能已退出，忽略发送失败
        let _ = self.cmd_tx.send(SessionCmd::Shutdown);
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 读桥：tokio 侧推字节 → std 通道 → 阻塞 [`Read`]。
///
/// `pending` 是上一批未读完的残余；发送端全部 drop 后 `recv` 返回
/// `Disconnected`，即会话结束 → `Read` 返回 `Ok(0)`（EOF），
/// termview 的读泵据此触发 Exit。
struct ChannelReader {
    rx: std_mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.pending.is_empty() {
                let n = self.pending.len().min(buf.len());
                buf[..n].copy_from_slice(&self.pending[..n]);
                self.pending.drain(..n);
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(chunk) => self.pending = chunk,
                // 所有发送端（网络线程的读任务）已退出：会话结束
                Err(_) => return Ok(0),
            }
        }
    }
}

/// 写桥：阻塞 [`Write`] → tokio 无界通道 → 网络线程转发到 SSH 通道。
struct ChannelWriter {
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

impl io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx
            .send(buf.to_vec())
            .map_err(|_| io::Error::other("SSH 会话已结束"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 最小客户端 Handler：实现主机密钥策略 + R 转发路由。
struct ClientHandler {
    host_key_policy: HostKeyPolicy,
    host: String,
    port: u16,
    /// R 转发路由（仅目标机连接注入，跳板机 None）：服务端连入监听端口时
    /// 主动开 forwarded-tcpip 通道，据此把通道接到本机目标。
    router: Option<Arc<RemoteForwardRouter>>,
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match &self.host_key_policy {
            HostKeyPolicy::AcceptAll => Ok(true),
            HostKeyPolicy::KnownHosts { path } => {
                let path = path
                    .clone()
                    .or_else(|| std::env::home_dir().map(|h| h.join(".ssh/known_hosts")))
                    .ok_or_else(|| io::Error::other("无法定位 known_hosts"))?;
                check_known_hosts(&self.host, self.port, server_public_key, &path, &[])
                    .map_err(Into::into)
            }
            HostKeyPolicy::KnownHostsWithFallback {
                path,
                fallback_paths,
            } => check_known_hosts(
                &self.host,
                self.port,
                server_public_key,
                path,
                fallback_paths,
            )
            .map_err(Into::into),
        }
    }

    /// R 转发连入：russh 在**会话消息循环内联 await** 本回调（russh 0.65
    /// encrypted.rs:925-936 `.await?` 实锤）——并发纪律写死：回调只做
    /// 路由查表 + accept + `tokio::spawn` 泵任务 + 立即返回 Ok；在这里连接
    /// 本机目标或等待双向泵会冻结整个会话（shell 数据流/resize/其他隧道
    /// 全堵）。未命中路由 → 丢弃 reply（russh 语义 = 自动拒绝通道）。
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::client::ChannelOpenHandle,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        let Some(router) = self.router.as_ref() else {
            return Ok(()); // 未注入路由（跳板机）：拒绝连入
        };
        let Some(spec) = router.accept_incoming(connected_address, connected_port) else {
            eprintln!("远程转发连入未命中路由，已拒绝: {connected_address}:{connected_port}");
            return Ok(()); // reply drop → 拒绝
        };
        reply.accept().await;
        let router = Arc::clone(router);
        let key = (connected_address.to_string(), connected_port);
        tokio::spawn(async move {
            let connect =
                tokio::net::TcpStream::connect((spec.target_host.as_str(), spec.target_port)).await;
            match connect {
                Ok(tcp) => forward::pump(channel.into_stream(), tcp).await,
                Err(error) => eprintln!("隧道 {} 本机目标连接失败: {error}", spec.label()),
            }
            router.connection_closed(&key.0, key.1);
        });
        Ok(())
    }
}

/// fallback 只读；所有来源均无该主机记录才向 primary 学习。
fn check_known_hosts(
    host: &str,
    port: u16,
    server_key: &russh::keys::PublicKeyOrCertificate,
    primary: &std::path::Path,
    fallback: &[PathBuf],
) -> io::Result<bool> {
    let owned;
    let key = match server_key {
        russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
        russh::keys::PublicKeyOrCertificate::Certificate(cert) => {
            owned = russh::keys::PublicKey::new(cert.public_key().clone(), "");
            &owned
        }
    };
    for path in std::iter::once(primary).chain(fallback.iter().map(PathBuf::as_path)) {
        let recorded = russh::keys::known_hosts::known_host_keys_path(host, port, path)
            .map_err(io::Error::other)?;
        match verify_host_key(&recorded, key) {
            HostKeyVerdict::Match => return Ok(true),
            HostKeyVerdict::Mismatch => return Ok(false),
            HostKeyVerdict::Unknown => {}
        }
    }
    russh::keys::known_hosts::learn_known_hosts_path(host, port, key, primary)
        .map_err(io::Error::other)?;
    Ok(true)
}

/// 客户端连接参数（跳板机与目标机共用同一套 keepalive/无延迟策略）。
fn client_config() -> std::sync::Arc<russh::client::Config> {
    std::sync::Arc::new(russh::client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    })
}

/// 连接并认证一个端点（直连 TCP；跳板机与目标机的第一跳共用）。
/// `router` 仅目标机连接注入（直连会话的第一跳即目标机；有跳板链时第一跳
/// 是外层跳板，传 None）。
async fn connect_and_auth(
    endpoint: &SshEndpoint,
    router: Option<Arc<RemoteForwardRouter>>,
) -> io::Result<russh::client::Handle<ClientHandler>> {
    let handler = ClientHandler {
        host_key_policy: endpoint.host_key_policy.clone(),
        host: endpoint.host.clone(),
        port: endpoint.port(),
        router,
    };
    let mut handle = russh::client::connect(
        client_config(),
        (endpoint.host.as_str(), endpoint.port()),
        handler,
    )
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "SSH 连接 {} 失败: {error}（如为主机键变更，请核对 known_hosts）",
            endpoint.title()
        ))
    })?;
    authenticate(&mut handle, endpoint).await?;
    Ok(handle)
}

/// 经上一跳的 direct-tcpip 通道建立下一跳的独立加密会话并认证。
///
/// `prev` 的 Handle 必须由调用方持有保活（drop 即断流，本跳连接随之消亡）。
/// `prev_label` 描述隧道穿过的机器（如「跳板[1/2] ops@x」），只用于错误文案。
async fn open_through(
    prev: &russh::client::Handle<ClientHandler>,
    prev_label: &str,
    endpoint: &SshEndpoint,
    router: Option<Arc<RemoteForwardRouter>>,
) -> io::Result<russh::client::Handle<ClientHandler>> {
    let forward = prev
        .channel_open_direct_tcpip(
            endpoint.host.as_str(),
            endpoint.port() as u32,
            "127.0.0.1",
            0,
        )
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "经{prev_label}建立到 {} 的转发通道失败: {error}",
                endpoint.title()
            ))
        })?;
    let stream = forward.into_stream();
    let handler = ClientHandler {
        host_key_policy: endpoint.host_key_policy.clone(),
        host: endpoint.host.clone(),
        port: endpoint.port(),
        router,
    };
    let mut handle = russh::client::connect_stream(client_config(), stream, handler)
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "经{prev_label}连接 {} 失败: {error}",
                endpoint.title()
            ))
        })?;
    authenticate(&mut handle, endpoint).await?;
    Ok(handle)
}

/// 认证阶段（三种方式，见 [`SshAuth`]）。
async fn authenticate(
    handle: &mut russh::client::Handle<ClientHandler>,
    endpoint: &SshEndpoint,
) -> io::Result<()> {
    let authenticated = match &endpoint.auth {
        SshAuth::Password(password) => handle
            .authenticate_password(&endpoint.username, password)
            .await
            .map_err(|error| io::Error::other(format!("SSH 认证通信失败: {error}")))?
            .success(),
        SshAuth::KeyFile { path, passphrase } => {
            let key = russh::keys::load_secret_key(path, passphrase.as_deref()).map_err(|_| {
                io::Error::new(
                    ErrorKind::PermissionDenied,
                    PrivateKeyPassphraseRequired {
                        key_path: path.clone(),
                    },
                )
            })?;
            let hash_alg = hash_alg_for(key.public_key());
            handle
                .authenticate_publickey(
                    &endpoint.username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                )
                .await
                .map_err(|error| io::Error::other(format!("SSH 认证通信失败: {error}")))?
                .success()
        }
        SshAuth::PreparedPrivateKey { keys } => {
            let mut success = false;
            for key in keys {
                let result = handle
                    .authenticate_publickey(
                        &endpoint.username,
                        PrivateKeyWithHashAlg::new(Arc::clone(key), Some(HashAlg::Sha256)),
                    )
                    .await
                    .map_err(|error| io::Error::other(format!("SSH 认证通信失败: {error}")))?;
                if result.success() {
                    success = true;
                    break;
                }
            }
            success
        }
        SshAuth::KeyboardInteractive(secret) => {
            use russh::client::KeyboardInteractiveAuthResponse;
            let mut response = handle
                .authenticate_keyboard_interactive_start(&endpoint.username, None)
                .await
                .map_err(io::Error::other)?;
            let mut success = false;
            for _ in 0..8 {
                response = match response {
                    KeyboardInteractiveAuthResponse::Success => {
                        success = true;
                        break;
                    }
                    KeyboardInteractiveAuthResponse::Failure { .. } => break,
                    KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => handle
                        .authenticate_keyboard_interactive_respond(vec![
                            secret.clone();
                            prompts.len()
                        ])
                        .await
                        .map_err(io::Error::other)?,
                };
            }
            success || matches!(response, KeyboardInteractiveAuthResponse::Success)
        }
        SshAuth::Agent => {
            // 连接 ssh-agent（SSH_AUTH_SOCK），逐身份尝试，成功即止
            let mut agent = russh::keys::agent::client::AgentClient::connect_env()
                .await
                .map_err(|error| io::Error::other(format!("连接 ssh-agent 失败: {error}")))?;
            let identities = agent
                .request_identities()
                .await
                .map_err(|error| io::Error::other(format!("ssh-agent 列举身份失败: {error}")))?;
            let mut success = false;
            for identity in &identities {
                let public_key = identity.public_key().into_owned();
                let result = handle
                    .authenticate_publickey_with(
                        &endpoint.username,
                        public_key,
                        hash_alg_for(identity.public_key().as_ref()),
                        &mut agent,
                    )
                    .await;
                if matches!(result, Ok(auth) if auth.success()) {
                    success = true;
                    break;
                }
            }
            if !success {
                return Err(io::Error::other(
                    "ssh-agent 认证失败（agent 内无该服务器接受的密钥）",
                ));
            }
            true
        }
    };
    if !authenticated {
        return Err(io::Error::new(
            if matches!(endpoint.auth, SshAuth::PreparedPrivateKey { .. }) {
                ErrorKind::PermissionDenied
            } else {
                ErrorKind::Other
            },
            if matches!(endpoint.auth, SshAuth::PreparedPrivateKey { .. }) {
                "公钥认证被远端拒绝（确认远端 authorized_keys 已收录对应公钥）".to_owned()
            } else {
                format!("SSH 认证失败: {}（凭据不被服务器接受）", endpoint.title())
            },
        ));
    }
    Ok(())
}

/// 网络线程主循环：（跳板链逐跳穿透）→ 目标机 → PTY/shell → 通知 spawn → 常驻收发。
///
/// 跳板链 = ProxyJump 语义（`ssh -J`）：先与外层跳板建立普通 SSH 连接并认证，
/// 在其上开 direct-tcpip 通道指向下一跳，把该通道当作下一跳 SSH 会话的
/// 传输流（`connect_stream`）——每一跳的加密/认证相互独立，逐跳串行建立。
///
/// 中间跳 Handle 全部存入 `tunnels` 保活到会话结束：链上任一中间 Handle 被
/// drop 都会切断后续所有跳的传输流。退出时先断目标机，再逆序断跳板链。
///
/// 两个常驻任务：
/// - 收发任务：`select` 上 SSH 通道消息（Data/ExtendedData → std 通道）
///   与控制指令（Resize → window_change；Shutdown → 断连并退出）
/// - 写任务：tokio 通道的字节 → `make_writer` 异步写入
async fn network_main(
    options: SshOptions,
    ready_tx: std_mpsc::Sender<SessionHandles>,
    unexpected_exit: Arc<AtomicBool>,
    port_forward_events: forward::EventSink,
) {
    // R 转发路由：仅当配置了隧道才存在；经跳板链时 R 的 forwarded-tcpip 回调
    // 打在目标机连接的 Handler 上（链上中间跳一律 None）
    let router = (!options.tunnels.is_empty()).then(|| {
        Arc::new(forward::RemoteForwardRouter::new(Arc::clone(
            &port_forward_events,
        )))
    });
    let connect_result: io::Result<()> = async {
        let (handle, mut channel, tunnels) = tokio::time::timeout(
            std::time::Duration::from_secs(connect_timeout_secs(options.jump.len())),
            async {
                // 第一跳：外层跳板直连；无跳板链时第一跳就是目标机（注入路由）
                let mut handle = match options.jump.first() {
                    Some(first) => connect_and_auth(first, None).await?,
                    None => connect_and_auth(&options.endpoint, router.clone()).await?,
                };
                // 后续跳：跳板链剩余跳 + 目标机，都经上一跳的 direct-tcpip 穿透；
                // 中间跳 Handle 存入 tunnels 保活（drop 即断流）。
                let mut tunnels: Vec<russh::client::Handle<ClientHandler>> = Vec::new();
                if !options.jump.is_empty() {
                    let total = options.jump.len();
                    let chain: Vec<&SshEndpoint> = options.jump[1..]
                        .iter()
                        .chain(std::iter::once(&options.endpoint))
                        .collect();
                    for (index, endpoint) in chain.iter().enumerate() {
                        // 正在穿过的机器是第 index+1 跳（1 起），如「跳板[1/2] ops@x」
                        let through = &options.jump[index];
                        let label = format!("跳板[{}/{}] {}", index + 1, total, through.title());
                        // 最后一项是目标机：注入 R 转发路由
                        let target_router =
                            (index == chain.len() - 1).then(|| router.clone()).flatten();
                        let next = open_through(&handle, &label, endpoint, target_router).await?;
                        tunnels.push(handle);
                        handle = next;
                    }
                }

                let mut channel = handle
                    .channel_open_session()
                    .await
                    .map_err(|error| io::Error::other(format!("打开 SSH 通道失败: {error}")))?;
                channel
                    .request_pty(
                        true,
                        "xterm-256color",
                        options.cols as u32,
                        options.rows as u32,
                        0,
                        0,
                        &[],
                    )
                    .await
                    .map_err(|error| io::Error::other(format!("请求 PTY 失败: {error}")))?;
                channel
                    .request_shell(true)
                    .await
                    .map_err(|error| io::Error::other(format!("请求 shell 失败: {error}")))?;

                let mut confirmed = 0;
                while confirmed < 2 {
                    match channel.wait().await {
                        Some(russh::ChannelMsg::Success) => confirmed += 1,
                        Some(russh::ChannelMsg::Failure) => {
                            return Err(io::Error::other("远端拒绝 PTY 或 shell"));
                        }
                        None | Some(russh::ChannelMsg::Eof | russh::ChannelMsg::Close) => {
                            return Err(io::Error::other("shell 就绪前连接关闭"));
                        }
                        Some(_) => continue,
                    }
                }

                Ok::<_, io::Error>((handle, channel, tunnels))
            },
        )
        .await
        .map_err(|_| {
            io::Error::new(
                ErrorKind::TimedOut,
                format!(
                    "SSH 连接超时（{}s）",
                    connect_timeout_secs(options.jump.len())
                ),
            )
        })??;

        // 三条桥：视图写 → 写任务；控制指令 → 收发任务；收发任务 → 视图读
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<SessionCmd>();
        let (data_tx, data_rx) = std_mpsc::channel::<Vec<u8>>();

        // 目标机 Handle 包 Arc：ForwardHub 的常驻泵任务需要 'static 共享句柄
        // （russh Handle 不可 Clone——receiver/join 独占——但全部公开方法走
        // &self，Arc 共享所有权即可，Drop 本就 no-op）。
        let handle = Arc::new(handle);

        // 隧道建立（评审意见 5：挪出既有 timeout 块，独立预算——既有公式语义
        // 是「建连+认证+PTY/shell」，塞入 16 条隧道往返会误报）。放在
        // PTY/shell 就绪之后、ready 之前：任一隧道 bind/R 失败 = 连接期硬失败
        // （与 ssh -L 一致），错误文案点名隧道（如「端口被占用」）。
        let hub = if options.tunnels.is_empty() {
            None
        } else {
            let budget_secs = tunnel_budget_secs(options.tunnels.len());
            let hub = Arc::new(forward::ForwardHub::new(
                Arc::clone(&handle),
                Arc::clone(&port_forward_events),
                router.expect("有隧道必有路由（上方 then 分支）"),
            ));
            let established = tokio::time::timeout(
                std::time::Duration::from_secs(budget_secs),
                hub.establish(options.tunnels.clone()),
            )
            .await;
            match established {
                Ok(Ok(())) => Some(hub),
                Ok(Err(error)) => return Err(error), // establish 内部已撤已完成部分
                Err(_) => {
                    // 建立超时：先撤本地已登记部分（L 监听/R cancel）再硬失败；
                    // 网络侧残留随整连失败自行消亡
                    hub.shutdown().await;
                    return Err(io::Error::new(
                        ErrorKind::TimedOut,
                        format!(
                            "隧道建立超时（{} 条，{budget_secs}s）",
                            options.tunnels.len()
                        ),
                    ));
                }
            }
        };

        let mut ssh_writer = channel.make_writer();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(bytes) = writer_rx.recv().await {
                if ssh_writer.write_all(&bytes).await.is_err() {
                    break;
                }
                let _ = ssh_writer.flush().await;
            }
        });

        // spawn 侧就绪：取走三条桥的句柄
        if ready_tx.send(Ok((writer_tx, cmd_tx, data_rx))).is_err() {
            // 调用方已超时或取消，释放连接，不能留下无人持有的 shell。
            if let Some(hub) = &hub {
                hub.shutdown().await;
            }
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "connect cancelled", "en")
                .await;
            return Ok(());
        }

        loop {
            tokio::select! {
                msg = channel.wait() => match msg {
                    Some(russh::ChannelMsg::Data { data }) => {
                        if data_tx.send(data.to_vec()).is_err() {
                            break; // 视图已释放
                        }
                    }
                    Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                        // stderr 并入主流：终端语义本就合并显示
                        if data_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(russh::ChannelMsg::Eof | russh::ChannelMsg::Close) => break,
                    Some(_) => continue,
                    // 连接层死亡（russh 0.65：wait None = 通道/连接已关闭），
                    // 区别于 Eof|Close 的远端 shell 正常退出——置位供重连判别。
                    // 写任务失败不经此判别（只自行退出，由本路径迟到兜住，
                    // keepalive 30s×3 把检测期封顶约 90s）。
                    None => {
                        unexpected_exit.store(true, Ordering::Release);
                        break;
                    }
                },
                cmd = cmd_rx.recv() => match cmd {
                    Some(SessionCmd::Resize { cols, rows }) => {
                        if channel.window_change(cols, rows, 0, 0).await.is_err() {
                            // window_change 失败 = 连接层死亡，同 wait None 置位
                            unexpected_exit.store(true, Ordering::Release);
                            break;
                        }
                    }
                    Some(SessionCmd::SetTunnels(specs)) => {
                        // 并发纪律：diff 是多次 tcpip_forward/cancel 全局请求
                        // 往返，绝不内联 await——select 臂只投递指令，diff 在
                        // 独立任务执行（16 条 × 高 RTT 内联展开必卡 Shutdown）；
                        // 失败只发 Failed 事件（运行期软失败，不杀 shell）。
                        if let Some(hub) = &hub {
                            let hub = Arc::clone(hub);
                            tokio::spawn(async move { hub.apply_diff(specs).await; });
                        }
                    }
                    Some(SessionCmd::Shutdown) | None => break,
                },
            }
        }
        // 退出路径统一：先撤隧道（cancel R → 停 L），再断目标机，最后按建立
        // 顺序逆序断跳板链（尽力而为），drop data_tx 让视图读端收 EOF
        if let Some(hub) = &hub {
            hub.shutdown().await;
        }
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
            .await;
        for tunnel in tunnels.iter().rev() {
            let _ = tunnel
                .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
                .await;
        }
        Ok(())
    }
    .await;

    // ready_tx 已消费（成功路径在内部 send）；失败路径这里补发错误
    if let Err(error) = connect_result {
        let _ = ready_tx.send(Err(error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// 一次性 ed25519 测试密钥对（专为测试生成，与任何真实主机无关）。
    const TEST_ED25519_PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMM7EdDz4EBMGoDkjYWf0lCdnlZwIwWvXrNM6TP/eU9S laterterm-test";
    const TEST_ED25519_PRIVATE: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACDDOxHQ8+BATBqA5I2Fn9JQnZ5WcCMFr16zTOkz/3lPUgAAAJhXNAu2VzQL\ntgAAAAtzc2gtZWQyNTUxOQAAACDDOxHQ8+BATBqA5I2Fn9JQnZ5WcCMFr16zTOkz/3lPUg\nAAAEAxto8E1r+xSL8gIGqp5XQE73hsgtCD04sXirPd/KEQCcM7EdDz4EBMGoDkjYWf0lCd\nnlZwIwWvXrNM6TP/eU9SAAAADmxhdGVydGVybS10ZXN0AQIDBAUGBw==\n-----END OPENSSH PRIVATE KEY-----";
    /// 另一把 ed25519 公钥（扮演「变更后的密钥」/无关密钥）。
    const TEST_OTHER_PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAII5BrvKWR7S5LfqRVGzTcbNhs1dWfVcX8fXmC4xNfa6v other-key";
    /// RSA 公钥（验证 hash_alg 选择）。
    const TEST_RSA_PUBLIC: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC8zPKxr3FIpKLfwr51MenF56LCHXRorzZ8xFrT0eIvnLqmZpThn8VFjv9LoLB6lobfIKXmRadbLvdo6efVBIWy5BwYkb2lP9lL4COqCWxRu+1rdQ8gdM1EkoTUS/y3/JhumeNoxFO5ytTJO6QmCD5GBQ5fELEbrfkG+4I6L0ueo60yBqa5FrgRqTA+u9fRZRHZy7VYbfvsa341jWigwvrqwNRt/GkOS1Bz1NfFYckg7qBSH7//dIngSBPmCrHqpMxM9PIgagPhdUIiGmTi+rtwxRZKw0/gfYRHx19msiJqqOm+IjSGg2VDYfEJ8kt233/c9bJJbGOeOjexiOupAnIf laterterm-test-rsa";

    fn parse_public(line: &str) -> russh::keys::PublicKey {
        russh::keys::PublicKey::from_openssh(line).expect("解析测试公钥")
    }

    fn sample_endpoint(host: &str) -> SshEndpoint {
        SshEndpoint {
            host: host.into(),
            port: None,
            username: "ops".into(),
            auth: SshAuth::Password("secret".into()),
            host_key_policy: HostKeyPolicy::AcceptAll,
            encoding: SessionEncoding::Utf8,
        }
    }

    fn sample_options() -> SshOptions {
        SshOptions {
            endpoint: sample_endpoint("bastion.example.com"),
            jump: Vec::new(),
            tunnels: Vec::new(),
            cols: 110,
            rows: 30,
        }
    }

    /// TOFU 三段语义（本 crate 的判定 + russh 的落盘件）：无记录 Unknown →
    /// 学习 → 一致 Match；**变更 Mismatch**——中间人防线，不静默放行。
    /// 注释不参与身份：密钥带注释学习后回读（注释被丢弃）仍须 Match。
    #[test]
    fn known_hosts_tofu_semantics() {
        let dir = tempfile::tempdir().expect("临时目录");
        let known_hosts = dir.path().join("known_hosts");
        let key = parse_public(TEST_ED25519_PUBLIC);
        let load = |host: &str, port: u16| {
            russh::keys::known_hosts::known_host_keys_path(host, port, &known_hosts)
                .expect("读 known_hosts")
        };

        // ① 无记录：Unknown（TOFU 待学习）
        assert_eq!(
            verify_host_key(&load("bastion.example.com", 22), &key),
            HostKeyVerdict::Unknown
        );

        // ② 学习后：一致 Match（注释在回读中丢失也不影响身份判定）
        russh::keys::known_hosts::learn_known_hosts_path(
            "bastion.example.com",
            22,
            &key,
            &known_hosts,
        )
        .expect("学习（落盘）");
        assert_eq!(
            verify_host_key(&load("bastion.example.com", 22), &key),
            HostKeyVerdict::Match
        );

        // ③ 变更拒绝：同一主机的不同密钥 Mismatch
        let other = parse_public(TEST_OTHER_PUBLIC);
        assert_eq!(
            verify_host_key(&load("bastion.example.com", 22), &other),
            HostKeyVerdict::Mismatch
        );

        // ④ 不同主机/端口不受既有记录影响
        assert_eq!(
            verify_host_key(&load("other.example.com", 2222), &key),
            HostKeyVerdict::Unknown
        );
    }

    #[test]
    fn known_hosts_fallback_is_host_scoped_and_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("primary");
        let fallback = dir.path().join("fallback");
        let key = parse_public(TEST_ED25519_PUBLIC);
        let other = parse_public(TEST_OTHER_PUBLIC);
        let wrapped = russh::keys::PublicKeyOrCertificate::from(key.clone());
        russh::keys::known_hosts::learn_known_hosts_path("pinned", 2222, &key, &fallback).unwrap();
        assert!(
            check_known_hosts(
                "pinned",
                2222,
                &wrapped,
                &primary,
                std::slice::from_ref(&fallback)
            )
            .unwrap()
        );
        assert!(!primary.exists(), "fallback 匹配不得写 primary");
        let changed = russh::keys::PublicKeyOrCertificate::from(other.clone());
        assert!(
            !check_known_hosts(
                "pinned",
                2222,
                &changed,
                &primary,
                std::slice::from_ref(&fallback)
            )
            .unwrap()
        );
        russh::keys::known_hosts::learn_known_hosts_path("pinned", 2222, &other, &primary).unwrap();
        assert!(
            !check_known_hosts(
                "pinned",
                2222,
                &wrapped,
                &primary,
                std::slice::from_ref(&fallback)
            )
            .unwrap(),
            "primary mismatch 不得跳到 fallback"
        );
        let missing = dir.path().join("new-primary");
        assert!(
            check_known_hosts(
                "new-host",
                22,
                &wrapped,
                &missing,
                std::slice::from_ref(&fallback)
            )
            .unwrap()
        );
        assert!(missing.exists());
        let unreadable = dir.path().join("directory");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(
            check_known_hosts(
                "pinned",
                2222,
                &wrapped,
                &unreadable,
                std::slice::from_ref(&fallback)
            )
            .is_err()
        );
        assert!(check_known_hosts("unseen", 22, &wrapped, &primary, &[unreadable]).is_err());
    }

    /// 私钥文件加载：OpenSSH 格式无口令私钥可解析，公钥与文件内嵌对一致。
    #[test]
    fn keyfile_loads_and_public_key_matches() {
        let dir = tempfile::tempdir().expect("临时目录");
        let key_path = dir.path().join("id_ed25519");
        std::fs::write(&key_path, TEST_ED25519_PRIVATE).expect("写私钥文件");

        let key = russh::keys::load_secret_key(&key_path, None).expect("加载无口令私钥");
        assert_eq!(
            key.public_key().to_openssh().expect("序列化公钥").trim(),
            TEST_ED25519_PUBLIC
        );
    }

    /// RSA 走 SHA-256 签名散列（ russh 对 RSA 传 None 会落回 SHA-1 遗留算法），
    /// ed25519 忽略该参数（None）。
    #[test]
    fn hash_alg_prefers_sha256_for_rsa_only() {
        assert_eq!(
            hash_alg_for(&parse_public(TEST_RSA_PUBLIC)),
            Some(russh::keys::HashAlg::Sha256)
        );
        assert_eq!(hash_alg_for(&parse_public(TEST_ED25519_PUBLIC)), None);
    }

    #[test]
    fn title_follows_ssh_convention() {
        let endpoint = sample_endpoint("bastion.example.com");
        assert_eq!(endpoint.title(), "ops@bastion.example.com");
        assert_eq!(endpoint.port(), 22);

        let mut endpoint = sample_endpoint("bastion.example.com");
        endpoint.port = Some(2222);
        assert_eq!(endpoint.title(), "ops@bastion.example.com:2222");
    }

    /// 标签口径：直连只显示目标机；经跳板链穿透时按连接顺序后缀 ` ⇢ 跳1 ⇢ 跳2`，
    /// 一眼区分「我登的是内网机哪台、走的哪条链」。
    #[test]
    fn title_marks_jump_chain() {
        let mut options = sample_options();
        options.endpoint.host = "intranet.internal".into();
        assert_eq!(options.title(), "ops@intranet.internal");

        let mut jump = sample_endpoint("bastion.example.com");
        jump.port = Some(2222);
        options.jump = vec![jump];
        assert_eq!(
            options.title(),
            "ops@intranet.internal ⇢ ops@bastion.example.com:2222"
        );

        // 两跳链：按连接顺序（外→内）排列
        options.jump.push(sample_endpoint("inner.example.com"));
        assert_eq!(
            options.title(),
            "ops@intranet.internal ⇢ ops@bastion.example.com:2222 ⇢ ops@inner.example.com"
        );
    }

    /// 连接超时按跳数缩放：0 跳 15s，每多一跳 +10s；spawn 侧 = 网络侧 + 1s。
    #[test]
    fn connect_timeout_scales_with_jump_hops() {
        assert_eq!(connect_timeout_secs(0), 15);
        assert_eq!(connect_timeout_secs(1), 25);
        assert_eq!(connect_timeout_secs(3), 45);
    }

    /// 隧道预算独立定价：5s 起步 + 每条 2s（不动既有连接公式，既有测试
    /// connect_timeout_scales_with_jump_hops 逐值不变）。
    #[test]
    fn tunnel_budget_is_priced_per_tunnel() {
        assert_eq!(tunnel_budget_secs(0), 5);
        assert_eq!(tunnel_budget_secs(1), 7);
        assert_eq!(tunnel_budget_secs(16), 37);
    }

    /// spawn 侧总预算公式 = 连接段 + 隧道段 + 1s（评审意见 5 联动：网络侧
    /// 两段 timeout 与本公式必须同 diff 改，漏改任一侧会让长隧道配置下
    /// spawn 先超时、网络线程真实原因被吞）。直接调用 spawn_timeout_secs
    /// （单一事实源），不复刻公式——无隧道时不得计隧道段起步价。
    #[test]
    fn spawn_budget_sums_connect_and_tunnel_stages() {
        assert_eq!(spawn_timeout_secs(0, 0), 16);
        assert_eq!(spawn_timeout_secs(1, 0), 26, "无隧道时与旧公式一致");
        assert_eq!(spawn_timeout_secs(1, 16), 26 + 37, "跳段与隧道段相加");
    }

    /// 读桥契约：分块送达 + 发送端全退后 EOF（termview 的 Exit 触发源）。
    #[test]
    fn reader_bridge_concatenates_chunks_then_eof() {
        let (tx, rx) = std_mpsc::channel();
        let mut reader = ChannelReader {
            rx,
            pending: Vec::new(),
        };

        tx.send(vec![b'h', b'e']).unwrap();
        tx.send(b"llo".to_vec()).unwrap();
        drop(tx);

        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello");
    }

    /// 读桥契约：buf 比待读块小时分批拷贝，残余保留到下一次 read。
    #[test]
    fn reader_bridge_partial_drains_pending() {
        let (tx, rx) = std_mpsc::channel();
        let mut reader = ChannelReader {
            rx,
            pending: Vec::new(),
        };
        tx.send(vec![1, 2, 3, 4, 5]).unwrap();
        drop(tx);

        let mut small = [0u8; 2];
        assert_eq!(reader.read(&mut small).unwrap(), 2);
        assert_eq!(small, [1, 2]);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, vec![3, 4, 5]);
    }

    /// 写桥契约：字节原序抵达 tokio 侧；接收端退出后 write 报错。
    #[tokio::test]
    async fn writer_bridge_preserves_order_and_reports_close() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let mut writer = ChannelWriter { tx };

        writer.write_all(b"ssh-").unwrap();
        writer.write_all(b"ok").unwrap();
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(first, b"ssh-".to_vec());
        assert_eq!(second, b"ok".to_vec());

        drop(rx);
        assert!(writer.write_all(b"late").is_err());
    }

    /// 真实主机集成测试（默认忽略）：
    /// 直连：`LATERTERM_SSH_TEST=user:pass@host[:port] cargo test -p laterterm-ssh -- --ignored`
    /// 穿透：再设 `LATERTERM_SSH_JUMP=user:pass@bastion[:port]`（多跳用逗号分隔，
    /// 按连接顺序外→内）——目标机经跳板链 direct-tcpip 逐跳建立。
    #[test]
    #[ignore = "需要真实 SSH 主机，经 LATERTERM_SSH_TEST 环境变量注入"]
    fn connect_shell_roundtrip_against_real_host() {
        let spec = std::env::var("LATERTERM_SSH_TEST").expect("缺 LATERTERM_SSH_TEST");
        // user:pass@host[:port]
        let parse = |spec: &str| -> SshEndpoint {
            let (userinfo, hostport) = spec.rsplit_once('@').expect("格式 user:pass@host[:port]");
            let (user, pass) = userinfo
                .split_once(':')
                .expect("格式 user:pass@host[:port]");
            let (host, port) = match hostport.rsplit_once(':') {
                Some((host, port)) => (host.to_string(), port.parse().ok()),
                None => (hostport.to_string(), None),
            };
            SshEndpoint {
                host,
                port,
                username: user.into(),
                auth: SshAuth::Password(pass.into()),
                host_key_policy: HostKeyPolicy::AcceptAll,
                encoding: SessionEncoding::Utf8,
            }
        };

        let jump: Vec<SshEndpoint> = std::env::var("LATERTERM_SSH_JUMP")
            .map(|specs| {
                specs
                    .split(',')
                    .map(str::trim)
                    .filter(|spec| !spec.is_empty())
                    .map(parse)
                    .collect()
            })
            .unwrap_or_default();

        let session = SshSession::spawn(SshOptions {
            endpoint: parse(&spec),
            jump,
            tunnels: Vec::new(),
            cols: 80,
            rows: 24,
        })
        .expect("连接失败");

        let mut streams = session.take_streams().expect("take_streams");
        streams
            .writer
            .write_all(b"echo laterterm-ssh-ok\r\n")
            .unwrap();

        let mut output = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let mut chunk = [0u8; 4096];
            match streams.reader.read(&mut chunk) {
                Ok(0) => panic!("会话提前结束"),
                Ok(n) => {
                    output.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if output.contains("laterterm-ssh-ok") {
                        break;
                    }
                }
                Err(error) => panic!("读取失败: {error}"),
            }
            assert!(std::time::Instant::now() < deadline, "20s 内未见回显");
        }

        session.kill();
        session.kill(); // 幂等
    }
}
