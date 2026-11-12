//! 进程内回环集成测试：russh mock 服务器 + SshSession/SftpHandle 全链路。
//!
//! 覆盖：密码认证 → PTY/shell 应答 → 双向数据泵 → resize → kill EOF，
//! TOFU 主机键「首连记录 / 换键拒绝」，跳板链 direct-tcpip 逐跳穿透
//! （两跳/三跳/跳板拒转发负例），以及 SFTP 通道（直连读写回环、经跳板
//! 穿透、逐传输取消标志）。所有远端行为都在本进程内，不依赖外部 sshd。

use laterterm_core::{BackendKind, TerminalSession};
use laterterm_ssh::{
    AuthMethod, HostKeyPolicy, PortForwardEvent, SessionEncoding, SshAuth, SshEndpoint, SshOptions,
    SshSession, TunnelKind, TunnelSpec,
};
use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};
use russh::server::Server as _;
use russh_sftp::protocol::{
    Attrs, Data, FileAttributes, Handle as HandlePacket, Name, OpenFlags, Status, StatusCode,
};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;

use laterterm_ssh::sftp::{CancelFlag, ProgressFn, SftpHandle};

/// 最小 mock 服务器：任意密码/公钥放行，PTY+shell 回 Success，
/// 数据原样加前缀回显——足够驱动 SshSession 的完整数据路径。
///
/// `forwarded` 为 `Some` 时实现 direct-tcpip 转发（等价 sshd 的
/// AllowTcpForwarding yes：accept 后连向目标地址双向泵字节），
/// 并对每次接受的转发请求计数；为 `None` 时不 accept（drop 后 russh
/// 自动拒绝），等价「跳板机关闭转发」，供负例使用。
/// 已接受的转发通道记录在 `forwarded_channels`：隧道原始字节绝不回显，
/// 否则 "Got " 前缀会污染经隧道转发的 SSH 握手流。
///
/// `remote_forwarding` 为 `Some` 时实现 R 转发（tcpip_forward，等价 sshd 的
/// GatewayPorts/AllowTcpForwarding 远向开启）：hook 内 bind 真实 listener
/// （echoserver.rs:127-138 范式扩展），accept 到「连入远端端口的用户连接」
/// 后经 `session.handle().channel_open_forwarded_tcpip` 主动开通道并双向泵；
/// 对每次接受的 tcpip_forward 计数。为 `None` 时返回 Ok(false) 拒绝，
/// 供「R 被拒硬失败」负例使用。`remote_bind_requests` 记录每次请求的
/// (bind_host, port)（断言客户端按 127.0.0.1 原样请求）；
/// `remote_listeners` 持有 accept 循环任务，cancel_tcpip_forward 时 abort。
///
/// `sftp_root` 为 `Some` 时支持 `sftp` 子系统：channel_open_session 把
/// Channel 暂存进共享 map，subsystem_request("sftp") 时取出并交给
/// russh-sftp 的 server::run，文件系统 Handler 以该目录扮演远端 fs。
/// mock 侧 R 转发 listener 任务表：键 = (bind_host, port)。
type RemoteListeners = Arc<Mutex<HashMap<(String, u32), tokio::task::JoinHandle<()>>>>;
/// mock 侧 R 转发 bind 请求记录（断言客户端按 127.0.0.1 原样请求）。
type RemoteBindRequests = Arc<Mutex<Vec<(String, u32)>>>;

#[derive(Clone)]
struct MockHandler {
    accepted_key: Option<russh::keys::PublicKey>,
    forwarded: Option<Arc<AtomicUsize>>,
    forwarded_channels: Arc<Mutex<HashSet<russh::ChannelId>>>,
    open_channels: Arc<Mutex<HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>>>,
    /// 已交给 russh-sftp server::run 的通道：二进制 SFTP 流绝不回显，
    /// 也不得走 data() 回调的 "Got data:" 回显路径。
    sftp_channels: Arc<Mutex<HashSet<russh::ChannelId>>>,
    sftp_root: Option<Arc<std::path::Path>>,
    remote_forwarding: Option<Arc<AtomicUsize>>,
    remote_listeners: RemoteListeners,
    remote_bind_requests: RemoteBindRequests,
}

impl russh::server::Server for MockHandler {
    type Handler = Self;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self {
        self.clone()
    }
}

impl russh::server::Handler for MockHandler {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        _: &str,
        _: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        Ok(russh::server::Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<russh::server::Auth, Self::Error> {
        if self
            .accepted_key
            .as_ref()
            .is_none_or(|accepted| accepted == key.public_key)
        {
            Ok(russh::server::Auth::Accept)
        } else {
            Ok(russh::server::Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        // 暂存 Channel 供 subsystem_request 取用（shell 路径不取也无害）
        self.open_channels
            .lock()
            .unwrap()
            .insert(channel.id(), channel);
        Ok(())
    }

    /// `sftp` 子系统：取出暂存的 Channel → channel_success → 交给
    /// russh-sftp server::run，以 `sftp_root` 扮演远端文件系统。
    async fn subsystem_request(
        &mut self,
        channel: russh::ChannelId,
        name: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            return Ok(());
        }
        let Some(root) = self.sftp_root.clone() else {
            let _ = session.channel_failure(channel);
            return Ok(());
        };
        let Some(stashed) = self.open_channels.lock().unwrap().remove(&channel) else {
            let _ = session.channel_failure(channel);
            return Ok(());
        };
        let _ = session.channel_success(channel);
        self.sftp_channels.lock().unwrap().insert(channel);
        tokio::spawn(async move {
            russh_sftp::server::run(
                stashed.into_stream(),
                MockFsHandler {
                    root,
                    next_handle: 0,
                    open: HashMap::new(),
                },
            )
            .await;
        });
        Ok(())
    }

    /// direct-tcpip（本地转发）：accept 后连向 (host, port) 双向泵字节。
    /// 转发关闭时直接返回（不 accept），russh 对 drop 的 reply 自动拒绝。
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let Some(counter) = &self.forwarded else {
            eprintln!(
                "[mock] 转发已关闭，拒绝 direct-tcpip 到 {host_to_connect}:{port_to_connect}"
            );
            return Ok(());
        };
        counter.fetch_add(1, Ordering::SeqCst);
        self.forwarded_channels.lock().unwrap().insert(channel.id());
        reply.accept().await;
        let target = format!("{host_to_connect}:{port_to_connect}");
        tokio::spawn(async move {
            let mut tcp = match tokio::net::TcpStream::connect(target.as_str()).await {
                Ok(tcp) => tcp,
                Err(error) => {
                    eprintln!("[mock] 转发连 {target} 失败: {error}");
                    return;
                }
            };
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
            eprintln!("[mock] 转发 {target} 连接结束");
        });
        Ok(())
    }

    /// R 转发（tcpip_forward）：hook 内 bind 真实 listener，accept 到「连入
    /// 远端端口的用户连接」后经 session.handle() 主动开 forwarded-tcpip 通道
    /// 并双向泵字节。关闭时返回 Ok(false)，客户端收到 RequestDenied 硬失败。
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        let Some(counter) = &self.remote_forwarding else {
            eprintln!("[mock] 远程转发已关闭，拒绝 tcpip_forward {address}:{port}");
            return Ok(false);
        };
        counter.fetch_add(1, Ordering::SeqCst);
        self.remote_bind_requests
            .lock()
            .unwrap()
            .push((address.to_string(), *port));
        let handle = session.handle();
        let address = address.to_string();
        let port = *port;
        let listeners = Arc::clone(&self.remote_listeners);
        let bind_key = (address.clone(), port);
        // accept 循环独立成任务，句柄入表供 cancel_tcpip_forward abort
        let task = tokio::spawn(async move {
            let listener = match TcpListener::bind((address.as_str(), port as u16)).await {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("[mock] tcpip_forward bind {address}:{port} 失败: {error}");
                    return;
                }
            };
            loop {
                let Ok((tcp, peer)) = listener.accept().await else {
                    break;
                };
                let channel = match handle
                    .channel_open_forwarded_tcpip(
                        address.as_str(),
                        port,
                        peer.ip().to_string(),
                        peer.port() as u32,
                    )
                    .await
                {
                    Ok(channel) => channel,
                    Err(error) => {
                        eprintln!("[mock] 开 forwarded-tcpip 到客户端失败: {error}");
                        break;
                    }
                };
                let mut tcp = tcp;
                let _ = tokio::io::copy_bidirectional(&mut channel.into_stream(), &mut tcp).await;
                eprintln!("[mock] R 转发 {address}:{port} 一路连接结束");
            }
        });
        listeners.lock().unwrap().insert(bind_key, task);
        Ok(true)
    }

    /// 撤销 R 转发：abort accept 循环（drop 即关 listener）；未登记返回
    /// false（客户端按 RequestDenied 处理，与 sshd 行为一致）。
    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        let task = self
            .remote_listeners
            .lock()
            .unwrap()
            .remove(&(address.to_string(), port));
        match task {
            Some(task) => {
                task.abort();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_success(channel);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_success(channel);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: russh::ChannelId,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        eprintln!("[mock] 收到 EOF channel={channel:?}");
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: russh::ChannelId,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        eprintln!("[mock] 收到 CLOSE channel={channel:?}");
        Ok(())
    }

    async fn data(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        // 转发通道的字节属于隧道原始流（可能是内层 SSH 握手），交给泵任务，不回显
        if self.forwarded_channels.lock().unwrap().contains(&channel) {
            return Ok(());
        }
        // SFTP 通道的二进制流已由 russh-sftp server::run 消费（russh 会同时
        // 投递给通道缓冲与本回调），回显会污染 SFTP 报文流。
        if self.sftp_channels.lock().unwrap().contains(&channel) {
            return Ok(());
        }
        eprintln!("[mock] data 回调收到 {} 字节", data.len());
        let reply = format!("Got data: {}\r\n", String::from_utf8_lossy(data));
        session.data(channel, reply.into_bytes())?;
        eprintln!("[mock] 已回显");
        Ok(())
    }
}

/// 独立线程 + 独立 tokio runtime 里的 mock 服务器；drop 即整体收场。
/// `sftp_dir` 持有 SFTP 模式下扮演远端 fs 的临时目录（晚于 runtime 收场），
/// 只为续命而持有，从不读取。
struct MockServer {
    runtime: Option<tokio::runtime::Runtime>,
    port: u16,
    #[allow(dead_code)] // 续命锚点
    sftp_dir: Option<tempfile::TempDir>,
}

impl MockServer {
    /// 在 `port` 上起服务器（0 = 临时端口）；端口被占用时短暂重试，
    /// 以支持「同端口换主机键」的用例。转发默认关闭。
    fn start(port: u16) -> Self {
        Self::start_with_key(port, None)
    }

    fn start_with_key(port: u16, accepted_key: Option<russh::keys::PublicKey>) -> Self {
        Self {
            runtime: None,
            port: 0,
            sftp_dir: None,
        }
        .into_started(
            port,
            MockHandler {
                accepted_key,
                forwarded: None,
                forwarded_channels: Arc::new(Mutex::new(HashSet::new())),
                open_channels: Arc::new(Mutex::new(HashMap::new())),
                sftp_channels: Arc::new(Mutex::new(HashSet::new())),
                sftp_root: None,
                remote_forwarding: None,
                remote_listeners: Arc::new(Mutex::new(HashMap::new())),
                remote_bind_requests: Arc::new(Mutex::new(Vec::new())),
            },
        )
    }

    /// 起一台允许 direct-tcpip 转发的 mock；返回的计数器记录接受的转发请求数。
    fn start_forwarding(port: u16) -> (Self, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let server = Self {
            runtime: None,
            port: 0,
            sftp_dir: None,
        }
        .into_started(
            port,
            MockHandler {
                accepted_key: None,
                forwarded: Some(Arc::clone(&counter)),
                forwarded_channels: Arc::new(Mutex::new(HashSet::new())),
                open_channels: Arc::new(Mutex::new(HashMap::new())),
                sftp_channels: Arc::new(Mutex::new(HashSet::new())),
                sftp_root: None,
                remote_forwarding: None,
                remote_listeners: Arc::new(Mutex::new(HashMap::new())),
                remote_bind_requests: Arc::new(Mutex::new(Vec::new())),
            },
        );
        (server, counter)
    }

    /// 起一台带 SFTP 子系统的 mock；远端 fs 根目录为临时目录，
    /// 返回其路径供测试播种/核对文件。转发默认关闭。
    fn start_sftp(port: u16) -> (Self, Arc<std::path::Path>) {
        let dir = tempfile::Builder::new()
            .prefix("laterterm-sftp-mock-")
            .tempdir()
            .expect("sftp 根临时目录");
        let root: Arc<std::path::Path> = Arc::from(dir.path().to_path_buf());
        let mut server = Self {
            runtime: None,
            port: 0,
            sftp_dir: Some(dir),
        };
        server = server.into_started(
            port,
            MockHandler {
                accepted_key: None,
                forwarded: None,
                forwarded_channels: Arc::new(Mutex::new(HashSet::new())),
                open_channels: Arc::new(Mutex::new(HashMap::new())),
                sftp_channels: Arc::new(Mutex::new(HashSet::new())),
                sftp_root: Some(Arc::clone(&root)),
                remote_forwarding: None,
                remote_listeners: Arc::new(Mutex::new(HashMap::new())),
                remote_bind_requests: Arc::new(Mutex::new(Vec::new())),
            },
        );
        (server, root)
    }

    /// 起一台允许 R 转发（tcpip_forward）的 mock：返回接受计数与 bind 请求
    /// 记录（断言「绑定 127.0.0.1」用）。
    fn start_remote_forwarding(port: u16) -> (Self, Arc<AtomicUsize>, RemoteBindRequests) {
        let counter = Arc::new(AtomicUsize::new(0));
        let binds = Arc::new(Mutex::new(Vec::new()));
        let server = Self {
            runtime: None,
            port: 0,
            sftp_dir: None,
        }
        .into_started(
            port,
            MockHandler {
                accepted_key: None,
                forwarded: None,
                forwarded_channels: Arc::new(Mutex::new(HashSet::new())),
                open_channels: Arc::new(Mutex::new(HashMap::new())),
                sftp_channels: Arc::new(Mutex::new(HashSet::new())),
                sftp_root: None,
                remote_forwarding: Some(Arc::clone(&counter)),
                remote_listeners: Arc::new(Mutex::new(HashMap::new())),
                remote_bind_requests: Arc::clone(&binds),
            },
        );
        (server, counter, binds)
    }

    /// 把自身从占位结构补全为已启动服务器（保留 sftp_dir 的持有权）。
    fn into_started(mut self, port: u16, handler: MockHandler) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("mock runtime");
        let host_key =
            PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成主机键");
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::ZERO,
            ..Default::default()
        });

        // listener 被 run_on_socket 借用，绑定与服务循环必须同住一个任务
        let (port_tx, port_rx) = flume::bounded(1);
        runtime.spawn(async move {
            let mut attempts = 0;
            let listener = loop {
                match TcpListener::bind(("127.0.0.1", port)).await {
                    Ok(listener) => break listener,
                    // 临时端口（0）不会冲突，失败即致命
                    Err(_) if port == 0 => panic!("mock 服务器绑定临时端口失败"),
                    Err(_) if attempts < 20 => {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(error) => panic!("mock 服务器绑定 {port} 失败: {error}"),
                }
            };
            if port_tx.send(listener.local_addr().unwrap().port()).is_err() {
                return; // 启动方已放弃等待
            }
            let mut handler = handler;
            let running = handler.run_on_socket(Arc::clone(&config), &listener);
            let _ = running.await;
        });

        let bound_port = port_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("mock 服务器启动超时");

        self.port = bound_port;
        self.runtime = Some(runtime);
        self
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

// ---------- SFTP 服务端 mock：tempdir 扮演远端 fs ----------

/// 打开的句柄表条目：文件（读写位置由句柄内 File 自持）或目录（一次性枚举）。
enum MockOpen {
    File(std::fs::File),
    Dir {
        files: Vec<russh_sftp::protocol::File>,
        sent: bool,
    },
}

/// russh-sftp 服务端 Handler：把客户端路径约束在根目录内（拒绝 `..` 穿越），
/// open/read/write/opendir/readdir/mkdir/remove/rmdir/rename/stat/realpath
/// 全部落到 std::fs——客户端侧 SftpHandle 的每条指令都有真实对端语义。
struct MockFsHandler {
    root: Arc<std::path::Path>,
    next_handle: u64,
    open: HashMap<String, MockOpen>,
}

impl MockFsHandler {
    /// 解析客户端路径：恒视为相对根的绝对路径（SFTP 语义），拒绝 `..` 穿越。
    fn resolve(&self, path: &str) -> Result<std::path::PathBuf, StatusCode> {
        let relative = path.trim_start_matches('/');
        if relative.split('/').any(|part| part == "..") {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(self.root.join(relative))
    }

    fn alloc_handle(&mut self) -> String {
        self.next_handle += 1;
        format!("h{}", self.next_handle)
    }

    fn attrs_of(path: &std::path::Path) -> Result<FileAttributes, StatusCode> {
        std::fs::metadata(path)
            .map(|metadata| FileAttributes::from(&metadata))
            .map_err(|_| StatusCode::NoSuchFile)
    }

    fn list_dir(path: &std::path::Path) -> Result<Vec<russh_sftp::protocol::File>, StatusCode> {
        let entries = std::fs::read_dir(path).map_err(|_| StatusCode::NoSuchFile)?;
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            files.push(russh_sftp::protocol::File::new(
                entry.file_name().to_string_lossy().to_string(),
                FileAttributes::from(&metadata),
            ));
        }
        Ok(files)
    }
}

impl russh_sftp::server::Handler for MockFsHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<HandlePacket, Self::Error> {
        let path = self.resolve(&filename)?;
        let file = if pflags.contains(OpenFlags::CREATE) {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(pflags.contains(OpenFlags::TRUNCATE))
                .write(true)
                .read(pflags.contains(OpenFlags::READ))
                .open(path)
                .map_err(|_| StatusCode::PermissionDenied)?
        } else if pflags.contains(OpenFlags::WRITE) {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|_| StatusCode::PermissionDenied)?
        } else {
            std::fs::File::open(path).map_err(|_| StatusCode::NoSuchFile)?
        };
        let handle = self.alloc_handle();
        self.open.insert(handle.clone(), MockOpen::File(file));
        Ok(HandlePacket { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.open.remove(&handle);
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        use std::io::{Seek, SeekFrom};
        let MockOpen::File(file) = self.open.get_mut(&handle).ok_or(StatusCode::Failure)? else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        let mut buffer = vec![0u8; len as usize];
        let n = file.read(&mut buffer).map_err(|_| StatusCode::Failure)?;
        buffer.truncate(n);
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        Ok(Data { id, data: buffer })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        use std::io::{Seek, SeekFrom};
        let MockOpen::File(file) = self.open.get_mut(&handle).ok_or(StatusCode::Failure)? else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        file.write_all(&data).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<HandlePacket, Self::Error> {
        let resolved = self.resolve(&path)?;
        let files = Self::list_dir(&resolved)?;
        let handle = self.alloc_handle();
        self.open
            .insert(handle.clone(), MockOpen::Dir { files, sent: false });
        Ok(HandlePacket { id, handle })
    }

    /// 客户端循环读取直到 Eof：首次返回全量条目，其后返回 Eof。
    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let MockOpen::Dir { files, sent } =
            self.open.get_mut(&handle).ok_or(StatusCode::Failure)?
        else {
            return Err(StatusCode::Failure);
        };
        if *sent {
            return Err(StatusCode::Eof);
        }
        *sent = true;
        Ok(Name {
            id,
            files: std::mem::take(files),
        })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let resolved = self.resolve(&path)?;
        std::fs::create_dir(resolved).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let resolved = self.resolve(&filename)?;
        std::fs::remove_file(resolved).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let resolved = self.resolve(&path)?;
        std::fs::remove_dir(resolved).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let from = self.resolve(&oldpath)?;
        let to = self.resolve(&newpath)?;
        std::fs::rename(from, to).map_err(|_| StatusCode::Failure)?;
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let resolved = self.resolve(&path)?;
        Ok(Attrs {
            id,
            attrs: Self::attrs_of(&resolved)?,
        })
    }

    /// mock 命名空间以根为准：`.`/`` → `/`，其余路径回显其绝对形态。
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let resolved = match path.trim_start_matches('/') {
            "" | "." => "/".to_string(),
            relative => format!("/{relative}"),
        };
        Ok(Name {
            id,
            files: vec![russh_sftp::protocol::File::dummy(resolved)],
        })
    }
}

/// 连接参数（known_hosts 落在独立临时目录下的文件）。
fn options_for(port: u16, auth: SshAuth, dir: &std::path::Path) -> SshOptions {
    SshOptions {
        endpoint: SshEndpoint {
            host: "127.0.0.1".into(),
            port: Some(port),
            username: "tester".into(),
            auth,
            host_key_policy: HostKeyPolicy::KnownHosts {
                path: Some(dir.join("known_hosts")),
            },
            encoding: SessionEncoding::Utf8,
        },
        jump: Vec::new(),
        tunnels: Vec::new(),
        cols: 110,
        rows: 30,
    }
}

/// 回环端点（known_hosts 策略与 options_for 相同），供跳板链逐跳拼装。
fn endpoint_for(port: u16, dir: &std::path::Path) -> SshEndpoint {
    SshEndpoint {
        host: "127.0.0.1".into(),
        port: Some(port),
        username: "tester".into(),
        auth: SshAuth::Password("secret".into()),
        host_key_policy: HostKeyPolicy::KnownHosts {
            path: Some(dir.join("known_hosts")),
        },
        encoding: SessionEncoding::Utf8,
    }
}

fn temp_known_hosts(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("laterterm-ssh-{tag}-"))
        .tempdir()
        .expect("临时目录")
}

/// 阻塞读到包含 `needle` 或超时。
fn read_until(streams: &mut laterterm_core::SessionStreams, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut buffer = [0u8; 512];
    let mut received = String::new();
    while std::time::Instant::now() < deadline {
        match streams.reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => received.push_str(&String::from_utf8_lossy(&buffer[..read])),
            Err(error) => panic!("读会话流失败: {error}"),
        }
        if received.contains(needle) {
            return received;
        }
    }
    panic!("10s 内未等到 {needle:?}，实际收到: {received:?}");
}

/// 阻塞等 reader 到 EOF（kill 之后的会话结束信号）。
fn wait_eof(streams: &mut laterterm_core::SessionStreams) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut buffer = [0u8; 512];
    while std::time::Instant::now() < deadline {
        match streams.reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(error) => panic!("等待 EOF 时读失败: {error}"),
        }
    }
    panic!("10s 内未等到 EOF");
}

/// 全链路：密码认证 → shell 就绪 → 写入回显 → resize → kill → EOF。
#[test]
fn password_auth_echo_resize_and_kill_eof() {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .is_test(true)
        .try_init();
    let server = MockServer::start(0);
    let known_hosts = temp_known_hosts("echo");

    let session = SshSession::spawn(options_for(
        server.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    ))
    .expect("连接 mock 服务器");

    assert_eq!(session.backend(), BackendKind::Ssh);
    assert_eq!(session.title(), format!("tester@127.0.0.1:{}", server.port));

    let mut streams = session.take_streams().expect("首次 take_streams");
    assert!(session.take_streams().is_err(), "二次 take_streams 应报错");

    eprintln!("[test] 连接就绪，写入探测数据");
    streams.writer.write_all(b"ping-laterterm\n").expect("写入");
    eprintln!("[test] 已写入，等待回显");
    read_until(&mut streams, "Got data: ping-laterterm");

    session.resize(40, 12).expect("resize 应成功");
    session.kill();
    session.kill(); // 幂等
    wait_eof(&mut streams);

    // kill 之后 resize 静默成功（关闭路径视图仍会触发 resize）
    session.resize(80, 24).expect("kill 后 resize 应静默成功");
    drop(session);
}

/// 公钥认证全链路：临时生成客户端密钥文件，服务器放行任意公钥。
#[test]
fn publickey_auth_reaches_shell() {
    let server = MockServer::start(0);
    let known_hosts = temp_known_hosts("pubkey");
    let key_dir = tempfile::Builder::new()
        .prefix("laterterm-key-")
        .tempdir()
        .expect("密钥临时目录");
    let key_path = key_dir.path().join("id_ed25519");
    let client_key =
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成客户端密钥");
    std::fs::write(
        &key_path,
        client_key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .expect("写密钥文件");

    let session = SshSession::spawn(options_for(
        server.port,
        SshAuth::KeyFile {
            path: key_path,
            passphrase: None,
        },
        known_hosts.path(),
    ))
    .expect("公钥认证连接");

    let mut streams = session.take_streams().expect("take_streams");
    streams.writer.write_all(b"via-key\n").expect("写入");
    read_until(&mut streams, "Got data: via-key");
    session.kill();
    wait_eof(&mut streams);
}

/// 已准备的密钥文件删除后仍可认证；第一把被拒必须继续第二把。
#[test]
fn prepared_keys_try_next_after_rejection_and_survive_file_removal() {
    let dir = temp_known_hosts("prepared");
    let rejected = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let accepted = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let path = dir.path().join("client-key");
    std::fs::write(
        &path,
        accepted.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    let auth = laterterm_ssh::prepare_private_key(Some(&path), None).unwrap();
    std::fs::remove_file(&path).unwrap();
    let AuthMethod::PreparedPrivateKey { mut keys } = auth else {
        panic!("应预加载")
    };
    keys.insert(0, Arc::new(rejected));
    let server = MockServer::start_with_key(0, Some(accepted.public_key().clone()));
    let session = SshSession::spawn(options_for(
        server.port,
        SshAuth::PreparedPrivateKey { keys },
        dir.path(),
    ))
    .expect("第二把密钥应通过");
    session.kill();
}

#[test]
fn prepared_publickey_rejection_is_not_passphrase_error() {
    let dir = temp_known_hosts("rejected");
    let rejected = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let accepted = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let server = MockServer::start_with_key(0, Some(accepted.public_key().clone()));
    let error = SshSession::spawn(options_for(
        server.port,
        SshAuth::PreparedPrivateKey {
            keys: vec![Arc::new(rejected)],
        },
        dir.path(),
    ))
    .err()
    .expect("必须拒绝");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("公钥认证"));
    assert!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<laterterm_ssh::PrivateKeyPassphraseRequired>()
            .is_none()
    );
}

/// TOFU：首连记录主机键；同端口换键后被拒，错误文案可操作。
#[test]
fn tofu_learns_first_key_and_rejects_changed_key() {
    let server = MockServer::start(0);
    let port = server.port;
    let known_hosts = temp_known_hosts("tofu");

    let first = SshSession::spawn(options_for(
        port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    ))
    .expect("首连应通过 TOFU");
    first.kill();
    drop(first);
    drop(server);

    // 同端口、不同主机键的第二个服务器
    let impostor = MockServer::start(port);
    assert_eq!(impostor.port, port, "换键用例要求复用同一端口");

    let error = match SshSession::spawn(options_for(
        port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    )) {
        Err(error) => error,
        Ok(_) => panic!("换键后必须拒绝连接"),
    };
    let message = error.to_string();
    assert!(
        message.contains("主机键"),
        "错误应提示主机键问题: {message}"
    );
    assert!(
        message.contains("known_hosts"),
        "错误应指明处置路径: {message}"
    );

    // known_hosts 文件里应恰好记录了一行首连主机键（russh 追加时会先补一个换行）
    let recorded = std::fs::read_to_string(known_hosts.path().join("known_hosts")).expect("读记录");
    let entries = recorded
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(entries, 1, "应只记录首连一次: {recorded:?}");
}

/// 两跳穿透（ProxyJump）：目标机直连不可达参数由跳板 mock 转发达成——
/// shell 回显走通 + 跳板端口恰好接受 1 次转发请求。
#[test]
fn jump_chain_two_hops_forwards_and_reaches_shell() {
    let target = MockServer::start(0);
    let (bastion, forward_count) = MockServer::start_forwarding(0);
    let known_hosts = temp_known_hosts("jump2");

    let mut options = options_for(
        target.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.jump = vec![endpoint_for(bastion.port, known_hosts.path())];

    let session = SshSession::spawn(options).expect("经跳板连接 mock 服务器");
    assert_eq!(
        session.title(),
        format!(
            "tester@127.0.0.1:{} ⇢ tester@127.0.0.1:{}",
            target.port, bastion.port
        ),
        "标题应标注跳板链"
    );

    let mut streams = session.take_streams().expect("take_streams");
    streams.writer.write_all(b"via-jump\n").expect("写入");
    read_until(&mut streams, "Got data: via-jump");
    assert_eq!(
        forward_count.load(Ordering::SeqCst),
        1,
        "跳板应恰好接受一次 direct-tcpip 转发"
    );
    session.kill();
    wait_eof(&mut streams);
}

/// 三跳链：外→中→目标逐跳穿透；中间跳 Handle 必须保活到会话结束，
/// 每台跳板各接受恰好 1 次转发。
#[test]
fn jump_chain_three_hops_keeps_tunnels_alive() {
    let target = MockServer::start(0);
    let (middle, middle_count) = MockServer::start_forwarding(0);
    let (outer, outer_count) = MockServer::start_forwarding(0);
    let known_hosts = temp_known_hosts("jump3");

    let mut options = options_for(
        target.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    // 由外到内：先连 outer，经它到 middle，再经 middle 到目标机
    options.jump = vec![
        endpoint_for(outer.port, known_hosts.path()),
        endpoint_for(middle.port, known_hosts.path()),
    ];

    let session = SshSession::spawn(options).expect("经两台跳板连接 mock 服务器");
    let mut streams = session.take_streams().expect("take_streams");
    streams.writer.write_all(b"via-chain\n").expect("写入");
    read_until(&mut streams, "Got data: via-chain");
    session.resize(60, 20).expect("三跳会话 resize 应正常");
    assert_eq!(
        outer_count.load(Ordering::SeqCst),
        1,
        "外层跳板应接受 1 次转发"
    );
    assert_eq!(
        middle_count.load(Ordering::SeqCst),
        1,
        "中层跳板应接受 1 次转发"
    );
    session.kill();
    wait_eof(&mut streams);
}

/// 负例：跳板机关闭转发（不实现 direct-tcpip accept）时必须失败，
/// 错误文案含「转发通道」与目标端点名，可据此定位是哪一跳出了问题。
#[test]
fn jump_chain_fails_with_actionable_error_when_forwarding_rejected() {
    let target = MockServer::start(0);
    let bastion = MockServer::start(0); // forwarded: None → 拒绝 direct-tcpip
    let known_hosts = temp_known_hosts("jump-reject");

    let mut options = options_for(
        target.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.jump = vec![endpoint_for(bastion.port, known_hosts.path())];

    let error = SshSession::spawn(options)
        .err()
        .expect("跳板拒转发必须失败");
    let message = error.to_string();
    assert!(
        message.contains("转发通道"),
        "错误应提示转发通道问题: {message}"
    );
    assert!(
        message.contains(&format!("tester@127.0.0.1:{}", target.port)),
        "错误应含目标端点名: {message}"
    );
    assert!(
        message.contains(&"跳板[1/1]".to_string()),
        "错误应标注跳板位置: {message}"
    );
}

// ---------- SFTP 通道回环 ----------

/// 构造指向回环 mock 的 SFTP 端点（known_hosts 与 shell 用例同策略）。
fn sftp_endpoint_for(port: u16, dir: &std::path::Path) -> SshEndpoint {
    endpoint_for(port, dir)
}

/// 播种测试内容：`len` 字节的确定性伪随机模式（跨块边界可校验字节级一致）。
fn pattern_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// 直连 SFTP 全回环：列目录 / 上传 / 下载字节级一致 / 进度回调触发 /
/// stat 与 realpath / 新建目录与重命名与删除。
#[test]
fn sftp_direct_roundtrip_upload_download_stat_realpath() {
    let (server, root) = MockServer::start_sftp(0);
    let known_hosts = temp_known_hosts("sftp-direct");
    let handle = SftpHandle::spawn(
        sftp_endpoint_for(server.port, known_hosts.path()),
        Vec::new(),
    )
    .expect("连接 SFTP mock");

    // 上传：300KiB + 零头，跨多个 64KiB 分块
    let payload = pattern_bytes(3 * 64 * 1024 + 12345);
    let src_dir = tempfile::Builder::new()
        .prefix("sftp-src-")
        .tempdir()
        .expect("本地临时目录");
    let local_src = src_dir.path().join("数据 文件.bin");
    std::fs::write(&local_src, &payload).expect("写本地源文件");

    // 进度回调：累计字节单调不减，且被真实触发
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_cb = Arc::clone(&seen);
    let progress: ProgressFn = Arc::new(move |total| {
        seen_cb.store(total as usize, Ordering::SeqCst);
    });
    let uploaded = handle
        .upload(&local_src, "/数据 文件.bin", Some(progress), None)
        .expect("上传");
    assert_eq!(uploaded as usize, payload.len(), "上传返回总字节数");
    assert_eq!(
        seen.load(Ordering::SeqCst),
        payload.len(),
        "进度回调最终累计到文件大小"
    );
    assert_eq!(
        std::fs::read(root.join("数据 文件.bin")).expect("mock 远端落盘"),
        payload,
        "上传字节级一致"
    );

    // 下载：原路读回并逐字节比对
    let local_dst = src_dir.path().join("下载回来.bin");
    let downloaded = handle
        .download("/数据 文件.bin", &local_dst, None, None)
        .expect("下载");
    assert_eq!(downloaded as usize, payload.len());
    assert_eq!(std::fs::read(&local_dst).expect("读本地"), payload);

    // 目录操作 + 列目录：新建目录、重命名、条目在列
    handle.create_dir("/子目录").expect("新建目录");
    handle
        .rename("/数据 文件.bin", "/改名后.bin")
        .expect("重命名");
    let entries = handle.read_dir("/").expect("列目录");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"改名后.bin"), "{names:?}");
    assert!(names.contains(&"子目录"), "{names:?}");
    assert!(!names.contains(&"数据 文件.bin"), "旧名不应在列: {names:?}");
    let dir_entry = entries
        .iter()
        .find(|e| e.name == "子目录")
        .expect("目录条目");
    assert!(dir_entry.is_dir, "目录权限位应判为目录");
    let file_entry = entries
        .iter()
        .find(|e| e.name == "改名后.bin")
        .expect("文件条目");
    assert!(!file_entry.is_dir);
    assert_eq!(
        file_entry.size as usize,
        payload.len(),
        "stat 尺寸来自目录属性"
    );

    // stat 单文件：大小与名称；realpath(".") 规范化为根
    let stat = handle.stat("/改名后.bin").expect("stat 文件");
    assert_eq!(stat.name, "改名后.bin");
    assert_eq!(stat.size as usize, payload.len());
    assert!(!stat.is_dir);
    assert_eq!(
        handle.realpath(".").expect("realpath"),
        "/",
        "mock 命名空间以根为准"
    );

    // 清理语义：删文件 + 删空目录
    handle.remove_file("/改名后.bin").expect("删文件");
    handle.remove_dir("/子目录").expect("删目录");
    assert!(!root.join("改名后.bin").exists());

    handle.close();
    handle.close(); // 幂等
}

/// 跳板穿透到 SFTP 目标：单跳（旧 Option 单跳语义在 Vec 下的等价形态）
/// 与两跳链各走一遍上传/下载回环，跳板各恰好接受一次转发。
#[test]
fn sftp_via_jump_chain_reaches_sftp_target() {
    let (server_target, _sftp_root_unused) = MockServer::start_sftp(0);
    let target = &server_target;
    let (bastion, bastion_count) = MockServer::start_forwarding(0);
    let (middle, middle_count) = MockServer::start_forwarding(0);
    let known_hosts = temp_known_hosts("sftp-jump");

    let payload = pattern_bytes(128 * 1024 + 7);
    let src_dir = tempfile::tempdir().expect("本地临时目录");
    let local_src = src_dir.path().join("via-jump.bin");
    std::fs::write(&local_src, &payload).expect("写本地源文件");

    // 单跳：等价于旧「Option<Box<SshEndpoint>>」单跳语义
    let handle = SftpHandle::spawn(
        sftp_endpoint_for(target.port, known_hosts.path()),
        vec![sftp_endpoint_for(bastion.port, known_hosts.path())],
    )
    .expect("经单跳跳板连 SFTP");
    handle
        .upload(&local_src, "/one-hop.bin", None, None)
        .expect("单跳上传");
    handle.close();
    assert_eq!(bastion_count.load(Ordering::SeqCst), 1, "跳板恰好 1 次转发");

    // 两跳链：外→中→目标逐跳穿透（与 shell 会话 tunnels 保活同语义），
    // 上传 + 下载回环字节级一致；每台跳板各恰好接受 1 次转发。
    let handle = SftpHandle::spawn(
        sftp_endpoint_for(target.port, known_hosts.path()),
        vec![
            sftp_endpoint_for(bastion.port, known_hosts.path()),
            sftp_endpoint_for(middle.port, known_hosts.path()),
        ],
    )
    .expect("经两跳跳板连 SFTP");
    let local_dst = src_dir.path().join("两跳下载.bin");
    handle
        .upload(&local_src, "/两跳文件.bin", None, None)
        .expect("两跳上传");
    let downloaded = handle
        .download("/两跳文件.bin", &local_dst, None, None)
        .expect("两跳下载");
    assert_eq!(downloaded as usize, payload.len());
    assert_eq!(std::fs::read(&local_dst).expect("读本地"), payload);
    handle.close();
    assert_eq!(
        bastion_count.load(Ordering::SeqCst),
        2,
        "外层跳板共 2 次转发"
    );
    assert_eq!(
        middle_count.load(Ordering::SeqCst),
        1,
        "中层跳板恰好 1 次转发"
    );
}

/// 取消语义：cancel 标志在进度回调里置位后，上传循环在下一次分块检查
/// 以 Interrupted 返回；远端留半成品（< 源大小）且可被 remove_file 清理。
#[test]
fn sftp_upload_cancel_flag_stops_mid_transfer_and_partial_is_cleanable() {
    let (server, root) = MockServer::start_sftp(0);
    let known_hosts = temp_known_hosts("sftp-cancel");
    let handle = SftpHandle::spawn(
        sftp_endpoint_for(server.port, known_hosts.path()),
        Vec::new(),
    )
    .expect("连接 SFTP mock");

    let payload = pattern_bytes(4 * 1024 * 1024);
    let src_dir = tempfile::tempdir().expect("本地临时目录");
    let local_src = src_dir.path().join("big.bin");
    std::fs::write(&local_src, &payload).expect("写本地源文件");

    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let cancel_cb = Arc::clone(&cancel);
    // 进度回调跑在网络线程的传输循环里：置位后下一次分块检查必然看到，
    // 取消时机因此是确定性的（不靠竞态睡眠）。
    let progress: ProgressFn = Arc::new(move |total| {
        if total >= 64 * 1024 {
            cancel_cb.store(true, Ordering::Relaxed);
        }
    });
    let error = handle
        .upload(
            &local_src,
            "/big.bin",
            Some(progress),
            Some(Arc::clone(&cancel)),
        )
        .expect_err("取消必须以错误返回");
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted, "{error}");
    assert_eq!(error.to_string(), "传输已取消");

    // 半成品存在且小于源文件；随后可正常删除（不残留半开句柄）
    let partial = std::fs::metadata(root.join("big.bin"))
        .expect("远端应留半成品")
        .len() as usize;
    assert!(
        partial < payload.len(),
        "半成品 {partial} 应小于源 {}",
        payload.len()
    );
    handle.remove_file("/big.bin").expect("半成品可清理");
    assert!(!root.join("big.bin").exists());

    // 取消后同通道仍可继续执行指令（取消只终结传输，不终结通道）
    handle.create_dir("/still-alive").expect("取消后通道仍可用");
    handle.remove_dir("/still-alive").expect("清理");
    handle.close();
}

/// close() 幂等且不能打断在途传输（Shutdown 只是排队）；网络线程退出后
/// 后续指令以「SFTP 网络线程已退出」可靠报错（russh 0.65 CHANNEL_CLOSE
/// 缺口注记见 roadmap——关闭依赖本地信号终结，不依赖远端回包）。
#[test]
fn sftp_close_is_idempotent_and_later_calls_fail_reliably() {
    let (server, _root) = MockServer::start_sftp(0);
    let known_hosts = temp_known_hosts("sftp-close");
    let handle = SftpHandle::spawn(
        sftp_endpoint_for(server.port, known_hosts.path()),
        Vec::new(),
    )
    .expect("连接 SFTP mock");

    handle.close();
    handle.close(); // 幂等

    // 网络线程处理 Shutdown 需要一点时间；此后指令可靠失败（轮询至多 5s）
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match handle.read_dir("/") {
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("SFTP 网络线程已退出"),
                    "错误应说明通道已退出: {message}"
                );
                break;
            }
            Ok(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "close 后网络线程应退出并让后续指令失败"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// 负例：跳板拒转发时 SFTP spawn 必须失败，错误文案定位到跳板与目标。
#[test]
fn sftp_via_jump_rejected_forwarding_fails_actionably() {
    let (server_target, _sftp_root_unused) = MockServer::start_sftp(0);
    let target = &server_target;
    let bastion = MockServer::start(0); // forwarded: None → 拒绝 direct-tcpip
    let known_hosts = temp_known_hosts("sftp-jump-reject");

    let error = SftpHandle::spawn(
        sftp_endpoint_for(target.port, known_hosts.path()),
        vec![sftp_endpoint_for(bastion.port, known_hosts.path())],
    )
    .err()
    .expect("跳板拒转发时 SFTP 必须失败");
    let message = error.to_string();
    assert!(message.contains("转发通道"), "{message}");
    assert!(
        message.contains(&format!("tester@127.0.0.1:{}", target.port)),
        "错误应含目标端点名: {message}"
    );
}

// ---------- 端口转发（隧道）回环 ----------

/// 隧道测试的目标端 echo 服务：独立线程收什么回什么；返回端口与停止标记
/// （测试持有 `_echo` 即存活，drop 后线程退出）。
fn spawn_echo_server() -> (u16, Arc<AtomicBool>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("echo bind");
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        listener.set_nonblocking(true).expect("echo 非阻塞");
        while !stop_flag.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    std::thread::spawn(move || {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .expect("echo 读超时");
                        let mut buffer = [0u8; 4096];
                        loop {
                            match socket.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => {
                                    if socket.write_all(&buffer[..read]).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (port, stop)
}

/// 探测一个空闲 TCP 端口（bind(0) 后立即释放；理论竞争窗口由连接重试兜底，
/// 与 MockServer::start 的重试先例同口径）。
fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("探测空闲端口")
        .local_addr()
        .unwrap()
        .port()
}

fn local_spec(bind_port: u16, target_port: u16) -> TunnelSpec {
    TunnelSpec {
        kind: TunnelKind::Local,
        bind_host: "127.0.0.1".into(),
        bind_port,
        target_host: "127.0.0.1".into(),
        target_port,
    }
}

fn remote_spec(bind_port: u16, target_port: u16) -> TunnelSpec {
    TunnelSpec {
        kind: TunnelKind::Remote,
        bind_host: "127.0.0.1".into(),
        bind_port,
        target_host: "127.0.0.1".into(),
        target_port,
    }
}

/// 连接隧道本地监听口（R 转发的 mock listener 在 tcpip_forward 往返后才
/// 绑定，轮询至多 5s 防 bind 未完成的竞态）。
fn connect_tunnel_port(port: u16) -> std::net::TcpStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(socket) => return socket,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("连隧道端口 {port} 失败: {error}"),
        }
    }
}

/// 经隧道写一笔并读回（echo 目标端语义：收什么回什么）。
fn tunnel_roundtrip(socket: &mut std::net::TcpStream, payload: &[u8]) {
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("读超时");
    socket.write_all(payload).expect("写隧道");
    let mut buffer = vec![0u8; payload.len()];
    let mut filled = 0;
    while filled < buffer.len() {
        let read = socket.read(&mut buffer[filled..]).expect("读隧道回显");
        assert!(read > 0, "隧道对端提前关闭");
        filled += read;
    }
    assert_eq!(buffer, payload, "隧道回显应逐字节一致");
}

/// L 直连回环：本地监听 → 目标机 direct-tcpip → echo 目标端，字节级往返；
/// shell 通道不受隧道影响。
#[test]
fn local_forward_direct_loopback_pumps_bytes() {
    let (server, _direct_count) = MockServer::start_forwarding(0);
    let (echo_port, _echo) = spawn_echo_server();
    let known_hosts = temp_known_hosts("l-direct");
    let bind_port = free_tcp_port();

    let mut options = options_for(
        server.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.tunnels = vec![local_spec(bind_port, echo_port)];

    let session = SshSession::spawn(options).expect("带 L 隧道连接 mock 服务器");
    let mut streams = session.take_streams().expect("take_streams");
    streams.writer.write_all(b"shell-alive\n").expect("写入");
    read_until(&mut streams, "Got data: shell-alive");

    let mut tunnel = connect_tunnel_port(bind_port);
    tunnel_roundtrip(&mut tunnel, b"ping-through-l");

    session.kill();
    wait_eof(&mut streams);
}

/// L 单跳穿透：隧道 direct-tcpip 打在目标机 Handle 上——跳板仅会话穿透的
/// 1 次（恰 1，不因隧道多开转发），目标机恰好为隧道开 1 次 direct-tcpip。
#[test]
fn local_forward_via_jump_rides_target_handle() {
    let (target, target_count) = MockServer::start_forwarding(0);
    let (bastion, bastion_count) = MockServer::start_forwarding(0);
    let (echo_port, _echo) = spawn_echo_server();
    let known_hosts = temp_known_hosts("l-jump");
    let bind_port = free_tcp_port();

    let mut options = options_for(
        target.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.jump = vec![endpoint_for(bastion.port, known_hosts.path())];
    options.tunnels = vec![local_spec(bind_port, echo_port)];

    let session = SshSession::spawn(options).expect("经跳板带 L 隧道连接");
    let mut streams = session.take_streams().expect("take_streams");
    streams.writer.write_all(b"via-jump-shell\n").expect("写入");
    read_until(&mut streams, "Got data: via-jump-shell");

    assert_eq!(
        bastion_count.load(Ordering::SeqCst),
        1,
        "跳板应恰好 1 次转发（会话穿透；L 隧道打在目标机 Handle 上）"
    );

    let mut tunnel = connect_tunnel_port(bind_port);
    tunnel_roundtrip(&mut tunnel, b"ping-through-jump");

    // direct-tcpip 是惰性通道（客户端连隧道端口才发起）——计数断言必须
    // 在隧道往返之后，此刻通道必然已建立且字节已走通
    assert_eq!(
        target_count.load(Ordering::SeqCst),
        1,
        "目标机应为 L 隧道恰好开 1 次 direct-tcpip"
    );

    session.kill();
    wait_eof(&mut streams);
}

/// R 回环：目标机侧监听 127.0.0.1:N（bind 请求原样到达 mock），连入连接经
/// forwarded-tcpip 通道回到本机 echo 目标端；bind 请求按 127.0.0.1 断言。
#[test]
fn remote_forward_loopback_reaches_local_target() {
    let (server, remote_count, bind_requests) = MockServer::start_remote_forwarding(0);
    let (echo_port, _echo) = spawn_echo_server();
    let known_hosts = temp_known_hosts("r-loop");
    let bind_port = free_tcp_port();

    let mut options = options_for(
        server.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.tunnels = vec![remote_spec(bind_port, echo_port)];

    let session = SshSession::spawn(options).expect("带 R 隧道连接 mock 服务器");
    let mut streams = session.take_streams().expect("take_streams");

    let mut tunnel = connect_tunnel_port(bind_port);
    tunnel_roundtrip(&mut tunnel, b"ping-through-r");

    assert_eq!(
        *bind_requests.lock().unwrap(),
        vec![("127.0.0.1".into(), bind_port as u32)],
        "tcpip_forward 应按配置的 127.0.0.1 原样请求"
    );
    assert_eq!(
        remote_count.load(Ordering::SeqCst),
        1,
        "应恰好 1 次 tcpip_forward"
    );

    session.kill();
    wait_eof(&mut streams);
}

/// 负例（连接期硬失败语义）：L 隧道 bind 端口被占用 → 整连失败，文案点名
/// 隧道与「端口被占用」。
#[test]
fn local_forward_bind_conflict_fails_whole_connection() {
    let server = MockServer::start(0);
    let (echo_port, _echo) = spawn_echo_server();
    let known_hosts = temp_known_hosts("l-conflict");
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("占位 listener");
    let bind_port = occupied.local_addr().unwrap().port();

    let mut options = options_for(
        server.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.tunnels = vec![local_spec(bind_port, echo_port)];

    let error = SshSession::spawn(options)
        .err()
        .expect("端口占用必须硬失败");
    let message = error.to_string();
    assert!(message.contains("端口被占用"), "{message}");
    assert!(
        message.contains(&format!("L 127.0.0.1:{bind_port}")),
        "错误应点名隧道: {message}"
    );
}

/// 负例（连接期硬失败语义）：目标机拒绝 tcpip_forward → 整连失败，
/// 文案区分于端口占用。
#[test]
fn remote_forward_rejected_fails_whole_connection() {
    let server = MockServer::start(0); // remote_forwarding: None → 拒绝
    let (echo_port, _echo) = spawn_echo_server();
    let known_hosts = temp_known_hosts("r-reject");
    let bind_port = free_tcp_port();

    let mut options = options_for(
        server.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.tunnels = vec![remote_spec(bind_port, echo_port)];

    let error = SshSession::spawn(options).err().expect("R 被拒必须硬失败");
    let message = error.to_string();
    assert!(message.contains("远端拒绝"), "{message}");
    assert!(
        message.contains(&format!("R 127.0.0.1:{bind_port}")),
        "错误应点名隧道: {message}"
    );
}

/// 并发回归（钉住「R 回调内 spawn」约束）：≥3 条连接同时泵期间，shell 通道
/// 回显不阻塞——若回归成回调内联泵，本用例的 shell 往返必然超时假红。
#[test]
fn remote_forward_concurrent_pumps_keep_shell_responsive() {
    let (server, _count, _binds) = MockServer::start_remote_forwarding(0);
    let (echo_port, _echo) = spawn_echo_server();
    let known_hosts = temp_known_hosts("r-concurrent");
    let bind_port = free_tcp_port();

    let mut options = options_for(
        server.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.tunnels = vec![remote_spec(bind_port, echo_port)];

    let session = SshSession::spawn(options).expect("带 R 隧道连接 mock 服务器");
    let mut streams = session.take_streams().expect("take_streams");

    // 3 条并发连入各自独立线程回显（mock 端 accept → forwarded-tcpip →
    // 客户端回调 spawn 泵，全程无串行点）
    let writers: Vec<_> = (0..3)
        .map(|index| {
            let payload = format!("并发-{index}");
            std::thread::spawn(move || {
                let mut tunnel = connect_tunnel_port(bind_port);
                tunnel_roundtrip(&mut tunnel, payload.as_bytes());
            })
        })
        .collect();

    // 并发泵期间 shell 回显保持响应（内联泵实现会卡死在这里）
    for round in 0..5 {
        let probe = format!("shell-{round}");
        streams.writer.write_all(probe.as_bytes()).expect("写入");
        read_until(&mut streams, &format!("Got data: {probe}"));
    }

    for writer in writers {
        writer.join().expect("并发隧道连接应完成");
    }

    session.kill();
    wait_eof(&mut streams);
}

/// diff 撤销回归（钉住 stop_tunnel 本地分支的锁纪律）：set_tunnels([]) 撤销
/// 已建立的 L 隧道——Stopped 事件可达、监听口关闭、shell 通道不冻结、kill
/// 后 EOF 可达。修复前 stop_tunnel 在同一 let 表达式内二次加锁 self.inner
/// （MutexGuard 语句级临时存活到分号），apply_diff 任务在此自死锁：Stopped
/// 永不到达、后续 shutdown 卡死、wait_eof 撞死线——本用例即该路径的哨兵。
#[test]
fn set_tunnels_revokes_local_tunnel_and_shell_survives() {
    let (server, _count) = MockServer::start_forwarding(0);
    let (echo_port, _echo) = spawn_echo_server();
    let known_hosts = temp_known_hosts("l-revoke");
    let bind_port = free_tcp_port();

    let mut options = options_for(
        server.port,
        SshAuth::Password("secret".into()),
        known_hosts.path(),
    );
    options.tunnels = vec![local_spec(bind_port, echo_port)];

    let session = SshSession::spawn(options).expect("带 L 隧道连接 mock 服务器");
    let mut streams = session.take_streams().expect("take_streams");

    // 建立期：shell 与隧道都通
    streams.writer.write_all(b"before-revoke\n").expect("写入");
    read_until(&mut streams, "Got data: before-revoke");
    let mut tunnel = connect_tunnel_port(bind_port);
    tunnel_roundtrip(&mut tunnel, b"alive-before-revoke");
    drop(tunnel);

    // 撤销全部隧道（触发 apply_diff → stop_tunnel 本地分支）
    session.set_tunnels(Vec::new()).expect("发送 SetTunnels");

    // Stopped 事件可达（死锁路径下该事件永远不出现）
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let stopped = loop {
        let events = session.take_port_forward_events();
        if events
            .iter()
            .any(|event| matches!(event, PortForwardEvent::Stopped { .. }))
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        stopped,
        "5s 内未收到隧道 Stopped 事件（stop_tunnel 疑似死锁）"
    );

    // 监听口应随 accept 任务 abort 而关闭（轮询至多 5s：socket 关闭有延迟）
    let closed = loop {
        match std::net::TcpStream::connect(("127.0.0.1", bind_port)) {
            Err(_) => break true,
            Ok(socket) => drop(socket),
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(closed, "撤销后监听口仍可连入（accept 任务未被 abort）");

    // 会话未冻结：撤销后 shell 回显照常
    streams.writer.write_all(b"after-revoke\n").expect("写入");
    read_until(&mut streams, "Got data: after-revoke");

    // kill 走 hub.shutdown（同样要拿 self.inner 锁）——死锁残留会卡死 EOF
    session.kill();
    wait_eof(&mut streams);
}
