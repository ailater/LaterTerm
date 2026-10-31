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
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, load_secret_key};
use russh::{ChannelMsg, Disconnect};
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use stream::{StreamRx, StreamTx};
use tokio::runtime::Runtime;

/// 连接 + 认证 + 启动 shell 的总超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// keyboard-interactive 多轮问答上限（防远端循环追问卡死连接流程）。
const KBD_INT_MAX_ROUNDS: usize = 8;

/// SSH 认证方式。凭据由调用方从密钥环取出后传入，本 crate 不做任何持久化。
#[derive(Clone, Debug)]
pub enum AuthMethod {
    /// 密码认证。
    Password {
        password: String,
    },
    /// keyboard-interactive 认证（跳板机二次验证常见）：
    /// 远端的每个 prompt 都以 `secret` 应答。
    KeyboardInteractive {
        secret: String,
    },
    /// 私钥文件认证；`passphrase` 仅在私钥本体加密时需要。
    PrivateKey {
        key_path: PathBuf,
        passphrase: Option<String>,
    },
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
    pub fn new(host: impl Into<String>, port: u16, username: impl Into<String>, auth: AuthMethod) -> Self {
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
        let runtime = Runtime::new().map_err(|error| io::Error::other(format!("创建运行时失败: {error}")))?;
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
        let Established { handle, mut read_half, write_half } = established;

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
                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => data,
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
                        if let Err(error) = write_half.window_change(cols as u32, rows as u32, 0, 0).await {
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
    let mut handle: Handle<HostKeyPolicy> = client::connect(config, (options.host.as_str(), options.port), policy)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;

    authenticate(&mut handle, options).await?;

    let channel = handle.channel_open_session().await.map_err(io::Error::other)?;
    channel
        .request_pty(true, "xterm-256color", options.cols as u32, options.rows as u32, 0, 0, &[])
        .await
        .map_err(io::Error::other)?;
    channel.request_shell(true).await.map_err(io::Error::other)?;

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
                return Err(io::Error::other("远端拒绝启动 shell（检查账号权限或登录 shell 配置）"));
            }
            Some(_) => continue,
            None => return Err(io::Error::other("连接在 shell 就绪前被远端关闭")),
        }
    }
    let (read_half, write_half) = channel.split();
    Ok(Established { handle, read_half, write_half })
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
                            format!("keyboard-interactive 认证被远端拒绝（用户 {}）", options.username),
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
        AuthMethod::PrivateKey { key_path, passphrase } => {
            let key = load_secret_key(key_path, passphrase.as_deref())
                .map_err(|error| io::Error::new(ErrorKind::InvalidInput, format!("读取私钥失败: {error}")))?;
            // RSA 固定 sha256 签名（None 会退回 SHA-1 的 ssh-rsa，新服务器多已禁用）
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), Some(HashAlg::Sha256));
            match handle
                .authenticate_publickey(&options.username, key)
                .await
                .map_err(io::Error::other)?
            {
                AuthResult::Success => Ok(()),
                AuthResult::Failure { .. } => Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "公钥认证被远端拒绝（确认远端 authorized_keys 已收录对应公钥）",
                )),
            }
        }
    }
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
