//! LaterTerm SSH 远程会话：russh + 线程泵。
//!
//! 职责边界（docs/architecture.md）：连接、认证、通道管理全部收敛在本
//! crate；tokio 只服务网络层，对外仍是 [`TerminalSession`] 契约——字节流
//! 经 flume 桥接为同步读写端，视图层感知不到异步的存在。
//!
//! 数据路径（每会话 3 个异步泵 + 视图侧既有阻塞读线程）：
//!
//! ```text
//! 键盘 → StreamTx(flume) → 输入泵 → 通道写端
//! 通道读端 → 输出泵 → StreamRx(flume) → 视图读泵
//! UI resize/kill → 控制命令(flume) → 控制泵 → window_change/close（顺序保证）
//! ```
//!
//! 主机键校验采用 TOFU（trust-on-first-use）：首次连接把服务器主机键记入
//! known_hosts 文件，此后不一致即拒绝连接（防中间人，见 [`policy`]）。
//! 密码/口令等凭据由调用方从密钥环取出后传入，本 crate 不落盘。

mod policy;
mod stream;

use laterterm_core::{BackendKind, SessionStreams, TerminalSession};
use policy::HostKeyPolicy;
use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, decode_secret_key};
use russh::{ChannelMsg, Disconnect};
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stream::{StreamRx, StreamTx};
use tokio::runtime::Runtime;

/// 连接 + 认证 + 启动 shell 的总超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// keyboard-interactive 多轮问答上限（防远端循环追问卡死连接流程）。
const KBD_INT_MAX_ROUNDS: usize = 8;

/// SSH 认证方式。凭据由调用方从密钥环取出后传入，本 crate 不做任何持久化。
#[derive(Clone)]
pub enum AuthMethod {
    /// 密码认证。
    Password { password: String },
    /// keyboard-interactive 认证（跳板机二次验证常见）：
    /// 远端的每个 prompt 都以 `secret` 应答。
    KeyboardInteractive { secret: String },
    /// 私钥文件认证；`passphrase` 仅在私钥本体加密时需要。
    PrivateKey {
        key_path: PathBuf,
        passphrase: Option<String>,
    },
    /// 调用线程预加载的私钥，按顺序尝试；不再读取文件或解密。
    PreparedPrivateKey { keys: Vec<Arc<PrivateKey>> },
}

impl std::fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { .. } => f.write_str("Password { password: [REDACTED] }"),
            Self::KeyboardInteractive { .. } => {
                f.write_str("KeyboardInteractive { secret: [REDACTED] }")
            }
            Self::PrivateKey { .. } => {
                f.write_str("PrivateKey { key_path: [REDACTED], passphrase: [REDACTED] }")
            }
            Self::PreparedPrivateKey { .. } => {
                f.write_str("PreparedPrivateKey { keys: [REDACTED] }")
            }
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

/// SSH 会话启动参数。
#[derive(Clone, Debug)]
pub struct SshOptions {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub cols: u16,
    pub rows: u16,
    /// TOFU 主机键记录文件；缺省 `~/.config/laterterm/known_hosts`。
    pub known_hosts: PathBuf,
}

impl SshOptions {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        auth: AuthMethod,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            auth,
            cols: 110,
            rows: 30,
            known_hosts: default_known_hosts(),
        }
    }
}

/// 默认 TOFU 主机键记录文件。
pub fn default_known_hosts() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/laterterm/known_hosts")
}

/// 控制命令。resize 与 close 走同一通道，交由单一控制泵顺序执行。
enum ControlCommand {
    Resize { cols: u16, rows: u16 },
    Close,
}

/// SSH 远程会话。
///
/// `connect` 阻塞直至 shell 就绪（含 15s 总超时），调用方必须放在后台
/// 线程执行（GPUI 主线程禁止阻塞调用），完成后把会话交回 UI 线程。
pub struct SshSession {
    runtime: Option<Runtime>,
    control_tx: flume::Sender<ControlCommand>,
    /// 输出泵的终结信号：kill 时不依赖远端回包（russh 主动发起 close 后
    /// 会把通道移出会话表，远端的 CHANNEL_CLOSE 回包将被丢弃）。
    shutdown_tx: flume::Sender<()>,
    /// 连接句柄：Drop 兜底时用于优雅断连（kill 已走通道关闭时不再动它）。
    disconnect: Mutex<Option<Handle<HostKeyPolicy>>>,
    streams: Mutex<Option<SessionStreams>>,
    closing: AtomicBool,
    title: String,
}

/// 连接完成后、启动泵之前的中间产物。
struct Established {
    handle: Handle<HostKeyPolicy>,
    read_half: russh::ChannelReadHalf,
    write_half: russh::ChannelWriteHalf<russh::client::Msg>,
}

impl SshSession {
    /// 建立连接、认证并请求 PTY + shell。
    pub fn connect(options: SshOptions) -> io::Result<Self> {
        // multi_thread 必需：泵任务要在 block_on 返回之后持续运行
        let runtime =
            Runtime::new().map_err(|error| io::Error::other(format!("创建运行时失败: {error}")))?;
        let title = format!("{}@{}", options.username, options.host);

        let established = runtime.block_on(async {
            match tokio::time::timeout(CONNECT_TIMEOUT, establish(&options)).await {
                Ok(result) => result,
                Err(_) => Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("连接 {}:{} 超时（15s）", options.host, options.port),
                )),
            }
        })?;
        let Established {
            handle,
            mut read_half,
            write_half,
        } = established;

        // 输入泵：视图写端 → 通道。键盘流量极小，无界队列即可。
        let (in_tx, in_rx) = flume::unbounded::<Vec<u8>>();
        let mut input_writer = write_half.make_writer();
        runtime.spawn(async move {
            while let Ok(chunk) = in_rx.recv_async().await {
                use tokio::io::AsyncWriteExt;
                if let Err(error) = input_writer.write_all(&chunk).await {
                    eprintln!("SSH 输入泵退出: {error}");
                    break;
                }
            }
        });

        // 输出泵：通道 → 视图读端。有界队列保留背压（远端刷屏时压回 TCP）。
        let (out_tx, out_rx) = flume::bounded::<Vec<u8>>(128);
        let (shutdown_tx, shutdown_rx) = flume::bounded::<()>(1);
        runtime.spawn(async move {
            loop {
                // select 的两个分支都要等：远端数据（EOF/断连）与本地 kill 信号
                let message = tokio::select! {
                    message = read_half.wait() => message,
                    _ = shutdown_rx.recv_async() => break,
                };
                let chunk = match message {
                    Some(ChannelMsg::Data { data })
                    | Some(ChannelMsg::ExtendedData { data, .. }) => data,
                    // 远端 EOF/CLOSE/会话断开：读端自然结束
                    Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
                    Some(_) => continue,
                };
                if out_tx.send_async(chunk.to_vec()).await.is_err() {
                    break;
                }
            }
            // out_tx 在此 drop → 视图读端拿到 EOF，即会话结束信号
        });

        // 控制泵：顺序执行 resize / close。
        let (control_tx, control_rx) = flume::unbounded::<ControlCommand>();
        runtime.spawn(async move {
            while let Ok(command) = control_rx.recv_async().await {
                match command {
                    ControlCommand::Resize { cols, rows } => {
                        if let Err(error) = write_half
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await
                        {
                            eprintln!("SSH 窗口变更失败: {error}");
                        }
                    }
                    ControlCommand::Close => {
                        let _ = write_half.eof().await;
                        let _ = write_half.close().await;
                        break;
                    }
                }
            }
        });

        Ok(Self {
            runtime: Some(runtime),
            control_tx,
            shutdown_tx,
            disconnect: Mutex::new(Some(handle)),
            streams: Mutex::new(Some(SessionStreams {
                reader: Box::new(StreamRx::new(out_rx)),
                writer: Box::new(StreamTx::new(in_tx)),
            })),
            closing: AtomicBool::new(false),
            title,
        })
    }
}

/// 建立连接 → 认证 → 请求 PTY + shell → 等待就绪应答。
async fn establish(options: &SshOptions) -> io::Result<Established> {
    let config = Arc::new(client::Config {
        inactivity_timeout: None, // 终端会话允许长静默
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3, // 90s 无响应判定连接死亡
        nodelay: true,
        ..Default::default()
    });
    let policy = HostKeyPolicy {
        host: options.host.clone(),
        port: options.port,
        known_hosts: options.known_hosts.clone(),
    };
    let mut handle: Handle<HostKeyPolicy> =
        client::connect(config, (options.host.as_str(), options.port), policy)
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;

    authenticate(&mut handle, options).await?;

    let channel = handle
        .channel_open_session()
        .await
        .map_err(io::Error::other)?;
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
        .map_err(io::Error::other)?;
    channel
        .request_shell(true)
        .await
        .map_err(io::Error::other)?;

    // want_reply=true：逐个等待 Success，任一 Failure 立即报错
    let mut confirmed = 0;
    let mut channel = channel;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => {
                confirmed += 1;
                if confirmed == 2 {
                    break;
                }
            }
            Some(ChannelMsg::Failure) => {
                return Err(io::Error::other(
                    "远端拒绝启动 shell（检查账号权限或登录 shell 配置）",
                ));
            }
            Some(_) => continue,
            None => return Err(io::Error::other("连接在 shell 就绪前被远端关闭")),
        }
    }
    let (read_half, write_half) = channel.split();
    Ok(Established {
        handle,
        read_half,
        write_half,
    })
}

/// 按配置执行认证；失败时给出可操作的错误信息。
async fn authenticate(handle: &mut Handle<HostKeyPolicy>, options: &SshOptions) -> io::Result<()> {
    match &options.auth {
        AuthMethod::Password { password } => match handle
            .authenticate_password(&options.username, password)
            .await
            .map_err(io::Error::other)?
        {
            AuthResult::Success => Ok(()),
            AuthResult::Failure { .. } => Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("密码认证被远端拒绝（用户 {}）", options.username),
            )),
        },
        AuthMethod::KeyboardInteractive { secret } => {
            let mut response = handle
                .authenticate_keyboard_interactive_start(&options.username, None)
                .await
                .map_err(io::Error::other)?;
            for _ in 0..KBD_INT_MAX_ROUNDS {
                response = match response {
                    KeyboardInteractiveAuthResponse::Success => return Ok(()),
                    KeyboardInteractiveAuthResponse::Failure { .. } => {
                        return Err(io::Error::new(
                            ErrorKind::PermissionDenied,
                            format!(
                                "keyboard-interactive 认证被远端拒绝（用户 {}）",
                                options.username
                            ),
                        ));
                    }
                    KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                        // 跳板机场景 prompt 通常只有一个口令位；每个 prompt 都以 secret 应答
                        let responses = vec![secret.clone(); prompts.len()];
                        handle
                            .authenticate_keyboard_interactive_respond(responses)
                            .await
                            .map_err(io::Error::other)?
                    }
                };
            }
            Err(io::Error::other(format!(
                "keyboard-interactive 超过 {KBD_INT_MAX_ROUNDS} 轮仍未通过"
            )))
        }
        AuthMethod::PrivateKey {
            key_path,
            passphrase,
        } => {
            let auth = prepare_private_key(Some(key_path), passphrase.as_deref())?;
            let AuthMethod::PreparedPrivateKey { keys } = auth else {
                unreachable!()
            };
            authenticate_keys(handle, &options.username, &keys).await
        }
        AuthMethod::PreparedPrivateKey { keys } => {
            authenticate_keys(handle, &options.username, keys).await
        }
    }
}

async fn authenticate_keys(
    handle: &mut Handle<HostKeyPolicy>,
    username: &str,
    keys: &[Arc<PrivateKey>],
) -> io::Result<()> {
    for key in keys {
        // RSA 固定 sha256 签名，拒绝后仍尝试下一把默认私钥。
        let key = PrivateKeyWithHashAlg::new(Arc::clone(key), Some(HashAlg::Sha256));
        if let AuthResult::Success = handle
            .authenticate_publickey(username, key)
            .await
            .map_err(io::Error::other)?
        {
            return Ok(());
        }
    }
    Err(io::Error::new(
        ErrorKind::PermissionDenied,
        "公钥认证被远端拒绝（确认远端 authorized_keys 已收录对应公钥）",
    ))
}

impl TerminalSession for SshSession {
    fn backend(&self) -> BackendKind {
        BackendKind::Ssh
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
        // 本地 kill 后视图仍可能触发 resize：静默成功即可
        if self.closing.load(Ordering::Acquire) {
            return Ok(());
        }
        self.control_tx
            .send(ControlCommand::Resize { cols, rows })
            .map_err(|_| io::Error::new(ErrorKind::NotConnected, "SSH 会话已关闭"))
    }

    fn kill(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        // 先终结输出泵（视图立即读到 EOF），再走控制泵做 eof+close 的
        // 优雅收尾；远端回包不可依赖（见 shutdown_tx 注释），双轨并行
        let _ = self.shutdown_tx.send(());
        let _ = self.control_tx.send(ControlCommand::Close);
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // kill 未被调用过的路径（如视图先退出）：发 DISCONNECT 让远端
            // 尽快回收 shell，再限时收场，避免半开连接拖住 UI 线程
            if !self.closing.load(Ordering::Acquire)
                && let Some(handle) = self.disconnect.lock().unwrap().take()
            {
                runtime.spawn(async move {
                    let _ = handle
                        .disconnect(Disconnect::ByApplication, "laterterm 关闭会话", "zh-CN")
                        .await;
                });
            }
            runtime.shutdown_timeout(Duration::from_secs(1));
        }
    }
}
