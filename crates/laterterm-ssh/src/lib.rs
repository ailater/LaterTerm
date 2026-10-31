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

pub use encoding::SessionEncoding;
use laterterm_core::{BackendKind, SessionStreams, TerminalSession};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;

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
}

/// 认证方式。
#[derive(Clone, Debug)]
pub enum SshAuth {
    /// 密码认证。
    Password(String),
    /// 私钥文件（OpenSSH 格式；`passphrase` 为私钥口令，未加密传 `None`）。
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// ssh-agent（读 `SSH_AUTH_SOCK`；依次尝试 agent 持有的全部身份）。
    Agent,
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
    /// 跳板机（ProxyJump 语义：先连它，再经 direct-tcpip 通道到目标机；
    /// `None` = 直连）。
    pub jump: Option<Box<SshEndpoint>>,
    /// 初始列数。
    pub cols: u16,
    /// 初始行数。
    pub rows: u16,
}

impl SshOptions {
    /// 标签标题：目标机为主；经跳板机时后缀 ` ⇢ 跳板机`，一眼区分直连/穿透。
    pub fn title(&self) -> String {
        match &self.jump {
            None => self.endpoint.title(),
            Some(jump) => format!("{} ⇢ {}", self.endpoint.title(), jump.title()),
        }
    }
}

/// 网络线程内的控制指令（tokio 无界通道，同步侧 send 永不阻塞）。
enum SessionCmd {
    Resize { cols: u32, rows: u32 },
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
        let (ready_tx, ready_rx) = std_mpsc::channel::<SessionHandles>();

        let network = std::thread::Builder::new()
            .name("laterterm-ssh".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("构建 tokio runtime 失败");
                runtime.block_on(network_main(options, ready_tx));
            })
            .map_err(io::Error::other)?;

        let _ = network; // 线程自行退出，不需要 join
        let (writer_tx, cmd_tx, data_rx) = ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .map_err(|_| io::Error::other("SSH 连接超时（30s 无响应）"))??;

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
        })
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

/// 最小客户端 Handler：实现主机密钥策略。
struct ClientHandler {
    host_key_policy: HostKeyPolicy,
    host: String,
    port: u16,
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
                // 证书形态取底层公钥比对（known_hosts 记录的是公钥）
                let key_owned;
                let key = match server_public_key {
                    russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
                    russh::keys::PublicKeyOrCertificate::Certificate(cert) => {
                        key_owned = russh::keys::PublicKey::new(cert.public_key().clone(), "");
                        &key_owned
                    }
                };
                let path = path
                    .clone()
                    .or_else(|| std::env::home_dir().map(|home| home.join(".ssh/known_hosts")))
                    .ok_or_else(|| {
                        io::Error::other("无法定位 known_hosts（未提供路径且无 HOME）")
                    })?;
                let recorded =
                    russh::keys::known_hosts::known_host_keys_path(&self.host, self.port, &path)
                        .map_err(russh::Error::from)?;
                match verify_host_key(&recorded, key) {
                    HostKeyVerdict::Match => Ok(true),
                    HostKeyVerdict::Unknown => {
                        // 首次连接：学习后放行
                        russh::keys::known_hosts::learn_known_hosts_path(
                            &self.host, self.port, key, &path,
                        )
                        .map_err(russh::Error::from)?;
                        Ok(true)
                    }
                    HostKeyVerdict::Mismatch => {
                        // 密钥变更：按中间人攻击处置，硬拒绝（返回 false 让
                        // russh 中止握手），修复指引见日志
                        eprintln!(
                            "主机密钥校验失败（{}:{}, {}）：已记录 {} 条密钥均不匹配——\
                             若确认服务器确实换过密钥，请手工清理 known_hosts 中该主机的行",
                            self.host,
                            self.port,
                            path.display(),
                            recorded.len()
                        );
                        Ok(false)
                    }
                }
            }
        }
    }
}

/// 连接并认证一个端点（直连 TCP；跳板机与目标机的第一跳共用）。
async fn connect_and_auth(
    endpoint: &SshEndpoint,
) -> io::Result<russh::client::Handle<ClientHandler>> {
    let handler = ClientHandler {
        host_key_policy: endpoint.host_key_policy.clone(),
        host: endpoint.host.clone(),
        port: endpoint.port(),
    };
    let mut handle = russh::client::connect(
        std::sync::Arc::new(russh::client::Config::default()),
        (endpoint.host.as_str(), endpoint.port()),
        handler,
    )
    .await
    .map_err(|error| io::Error::other(format!("SSH 连接 {} 失败: {error}", endpoint.title())))?;
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
            let key =
                russh::keys::load_secret_key(path, passphrase.as_deref()).map_err(|error| {
                    io::Error::other(format!("加载私钥 {} 失败: {error}", path.display()))
                })?;
            let hash_alg = hash_alg_for(key.public_key());
            handle
                .authenticate_publickey(
                    &endpoint.username,
                    russh::keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), hash_alg),
                )
                .await
                .map_err(|error| io::Error::other(format!("SSH 认证通信失败: {error}")))?
                .success()
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
        return Err(io::Error::other(format!(
            "SSH 认证失败: {}（凭据不被服务器接受）",
            endpoint.title()
        )));
    }
    Ok(())
}

/// 网络线程主循环：（可选跳板机）→ 目标机 → PTY/shell → 通知 spawn → 常驻收发。
///
/// 跳板机穿透 = ProxyJump 语义：先与跳板机建立普通 SSH 连接并认证，
/// 在其上开 direct-tcpip 通道指向目标机，把该通道当作目标机 SSH 会话的
/// 传输流（`connect_stream`）——目标机的加密/认证独立于跳板机。
///
/// 两个常驻任务：
/// - 收发任务：`select` 上 SSH 通道消息（Data/ExtendedData → std 通道）
///   与控制指令（Resize → window_change；Shutdown → 断连并退出）
/// - 写任务：tokio 通道的字节 → `make_writer` 异步写入
async fn network_main(options: SshOptions, ready_tx: std_mpsc::Sender<SessionHandles>) {
    let connect_result: io::Result<()> = async {
        // 第一跳：跳板机（可选）。它的 Handle 必须存活到会话结束——
        // drop 即断开转发通道，目标机连接随之消亡。
        let bastion_handle = match &options.jump {
            Some(jump) => {
                let bastion = connect_and_auth(jump).await?;
                Some(bastion)
            }
            None => None,
        };

        // 目标机：直连，或经跳板机的 direct-tcpip 通道穿透
        let handle = match &bastion_handle {
            Some(bastion) => {
                let endpoint = &options.endpoint;
                let forward = bastion
                    .channel_open_direct_tcpip(
                        endpoint.host.as_str(),
                        endpoint.port() as u32,
                        "127.0.0.1",
                        0,
                    )
                    .await
                    .map_err(|error| {
                        io::Error::other(format!(
                            "跳板机开转发通道到 {} 失败: {error}",
                            endpoint.title()
                        ))
                    })?;
                let stream = forward.into_stream();
                let handler = ClientHandler {
                    host_key_policy: endpoint.host_key_policy.clone(),
                    host: endpoint.host.clone(),
                    port: endpoint.port(),
                };
                let mut handle = russh::client::connect_stream(
                    std::sync::Arc::new(russh::client::Config::default()),
                    stream,
                    handler,
                )
                .await
                .map_err(|error| {
                    io::Error::other(format!("经跳板机连接 {} 失败: {error}", endpoint.title()))
                })?;
                authenticate(&mut handle, endpoint).await?;
                handle
            }
            None => connect_and_auth(&options.endpoint).await?,
        };

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

        // 三条桥：视图写 → 写任务；控制指令 → 收发任务；收发任务 → 视图读
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<SessionCmd>();
        let (data_tx, data_rx) = std_mpsc::channel::<Vec<u8>>();

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
        let _ = ready_tx.send(Ok((writer_tx, cmd_tx, data_rx)));

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
                    Some(_) => continue,
                    None => break, // 通道关闭（远程退出/断连）
                },
                cmd = cmd_rx.recv() => match cmd {
                    Some(SessionCmd::Resize { cols, rows }) => {
                        if channel.window_change(cols, rows, 0, 0).await.is_err() {
                            break;
                        }
                    }
                    Some(SessionCmd::Shutdown) | None => break,
                },
            }
        }
        // 退出路径统一：先断目标机，再断跳板机（尽力而为），
        // drop data_tx 让视图读端收 EOF
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
            .await;
        if let Some(bastion) = bastion_handle {
            let _ = bastion
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
            jump: None,
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

    /// 标签口径：直连只显示目标机；经跳板机穿透时后缀 ` ⇢ 跳板机`，
    /// 一眼区分「我登的是内网机哪台、走的哪条链」。
    #[test]
    fn title_marks_jump_chain() {
        let mut options = sample_options();
        options.endpoint.host = "intranet.internal".into();
        assert_eq!(options.title(), "ops@intranet.internal");

        let mut jump = sample_endpoint("bastion.example.com");
        jump.port = Some(2222);
        options.jump = Some(Box::new(jump));
        assert_eq!(
            options.title(),
            "ops@intranet.internal ⇢ ops@bastion.example.com:2222"
        );
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
    /// 穿透：再设 `LATERTERM_SSH_JUMP=user:pass@bastion[:port]`——目标机经跳板机 direct-tcpip 建立。
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

        let jump = std::env::var("LATERTERM_SSH_JUMP")
            .ok()
            .map(|jump_spec| Box::new(parse(&jump_spec)));

        let session = SshSession::spawn(SshOptions {
            endpoint: parse(&spec),
            jump,
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
