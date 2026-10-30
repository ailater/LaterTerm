//! LaterTerm SSH 会话后端。
//!
//! 实现 [`laterterm_core::TerminalSession`]：同一个 termview，换 backend。
//! 架构口径（docs/architecture.md）：**tokio 仅限网络层**——专用网络线程跑
//! current_thread runtime，SSH 读写经无界通道桥接为阻塞字节流，
//! 视图侧（termview）的读泵/写端完全不感知异步。
//!
//! MVP 范围（M3 第一步）：
//! - 密码认证（跳板机「密码 + 动态码」验收主路径；密钥文件/ssh-agent 紧随）
//! - PTY + shell 通道、窗口变更（resize）、优雅断连（kill）
//! - 主机密钥策略目前仅 `AcceptAll`（显式选项 + 风险注释），known_hosts/TOFU
//!   校验是 M3 后续项——见 docs/roadmap.md
//!
//! GBK 转码与 trzsz 同样在 M3 后续：本层透传字节，编码转换挂在
//! take_streams 返回的 reader/writer 装饰器上是既定演进位。

use laterterm_core::{BackendKind, SessionStreams, TerminalSession};
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;

/// 主机密钥校验策略。
///
/// `AcceptAll` 有中间人风险，仅作为 MVP 显式默认——等 known_hosts/TOFU
/// 落地（M3）后移除或改为非默认。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// 接受任意主机密钥（不安全：中间人可冒充主机；MVP 显式选择）。
    #[default]
    AcceptAll,
}

/// 认证方式。MVP 仅密码；密钥文件/ssh-agent 为 M3 后续项。
#[derive(Clone, Debug)]
pub enum SshAuth {
    Password(String),
}

/// SSH 会话启动参数。
pub struct SshOptions {
    /// 主机名或 IP。
    pub host: String,
    /// 端口（默认 22 由 [`SshOptions::normalized`] 补齐）。
    pub port: Option<u16>,
    /// 登录用户名。
    pub username: String,
    /// 认证方式。
    pub auth: SshAuth,
    /// 初始列数。
    pub cols: u16,
    /// 初始行数。
    pub rows: u16,
    /// 主机密钥策略。
    pub host_key_policy: HostKeyPolicy,
}

impl SshOptions {
    /// 端口补默认值。
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(22)
    }

    /// 标签标题：`user@host`（端口非 22 时带端口，与 ssh 命令行习惯一致）。
    pub fn title(&self) -> String {
        if self.port() == 22 {
            format!("{}@{}", self.username, self.host)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port())
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

        Ok(Self {
            cmd_tx,
            streams: Mutex::new(Some(SessionStreams {
                reader: Box::new(ChannelReader {
                    rx: data_rx,
                    pending: Vec::new(),
                }),
                writer: Box::new(ChannelWriter { tx: writer_tx }),
            })),
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

/// 最小客户端 Handler：只消费主机密钥回调。
struct ClientHandler {
    host_key_policy: HostKeyPolicy,
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match self.host_key_policy {
            HostKeyPolicy::AcceptAll => Ok(true),
        }
    }
}

/// 网络线程主循环：连接 → 认证 → PTY/shell → 通知 spawn → 常驻收发。
///
/// 两个常驻任务：
/// - 收发任务：`select` 上 SSH 通道消息（Data/ExtendedData → std 通道）
///   与控制指令（Resize → window_change；Shutdown → 断连并退出）
/// - 写任务：tokio 通道的字节 → `make_writer` 异步写入
async fn network_main(options: SshOptions, ready_tx: std_mpsc::Sender<SessionHandles>) {
    let config = std::sync::Arc::new(russh::client::Config::default());
    let handler = ClientHandler {
        host_key_policy: options.host_key_policy,
    };
    let addr = (options.host.as_str(), options.port());

    let connect_result: io::Result<()> = async {
        let mut handle = russh::client::connect(config, addr, handler)
            .await
            .map_err(|error| io::Error::other(format!("SSH 连接失败: {error}")))?;

        let auth_result = match &options.auth {
            SshAuth::Password(password) => handle
                .authenticate_password(&options.username, password)
                .await
                .map_err(|error| io::Error::other(format!("SSH 认证通信失败: {error}")))?,
        };
        if !auth_result.success() {
            return Err(io::Error::other("SSH 认证失败（用户名或密码错误）"));
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
        // 退出路径统一：先断连（尽力而为），drop data_tx 让视图读端收 EOF
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
            .await;
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

    fn sample_options() -> SshOptions {
        SshOptions {
            host: "bastion.example.com".into(),
            port: None,
            username: "ops".into(),
            auth: SshAuth::Password("secret".into()),
            cols: 110,
            rows: 30,
            host_key_policy: HostKeyPolicy::AcceptAll,
        }
    }

    #[test]
    fn title_follows_ssh_convention() {
        let options = sample_options();
        assert_eq!(options.title(), "ops@bastion.example.com");
        assert_eq!(options.port(), 22);

        let mut options = sample_options();
        options.port = Some(2222);
        assert_eq!(options.title(), "ops@bastion.example.com:2222");
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
    /// `LATERTERM_SSH_TEST=user:pass@host[:port] cargo test -p laterterm-ssh -- --ignored`
    #[test]
    #[ignore = "需要真实 SSH 主机，经 LATERTERM_SSH_TEST 环境变量注入"]
    fn connect_shell_roundtrip_against_real_host() {
        let spec = std::env::var("LATERTERM_SSH_TEST").expect("缺 LATERTERM_SSH_TEST");
        // user:pass@host[:port]
        let (userinfo, hostport) = spec.rsplit_once('@').expect("格式 user:pass@host[:port]");
        let (user, pass) = userinfo
            .split_once(':')
            .expect("格式 user:pass@host[:port]");
        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port.parse().ok()),
            None => (hostport.to_string(), None),
        };

        let session = SshSession::spawn(SshOptions {
            host,
            port,
            username: user.into(),
            auth: SshAuth::Password(pass.into()),
            cols: 80,
            rows: 24,
            host_key_policy: HostKeyPolicy::AcceptAll,
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
