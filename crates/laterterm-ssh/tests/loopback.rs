//! 进程内回环集成测试：russh mock 服务器 + SshSession 全链路。
//!
//! 覆盖：密码认证 → PTY/shell 应答 → 双向数据泵 → resize → kill EOF，
//! TOFU 主机键「首连记录 / 换键拒绝」，以及跳板链 direct-tcpip 逐跳穿透
//! （两跳/三跳/跳板拒转发负例）。所有远端行为都在本进程内，
//! 不依赖外部 sshd。

use laterterm_core::{BackendKind, TerminalSession};
use laterterm_ssh::{
    AuthMethod, HostKeyPolicy, SessionEncoding, SshAuth, SshEndpoint, SshOptions, SshSession,
};
use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};
use russh::server::Server as _;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

/// 最小 mock 服务器：任意密码/公钥放行，PTY+shell 回 Success，
/// 数据原样加前缀回显——足够驱动 SshSession 的完整数据路径。
///
/// `forwarded` 为 `Some` 时实现 direct-tcpip 转发（等价 sshd 的
/// AllowTcpForwarding yes：accept 后连向目标地址双向泵字节），
/// 并对每次接受的转发请求计数；为 `None` 时不 accept（drop 后 russh
/// 自动拒绝），等价「跳板机关闭转发」，供负例使用。
/// 已接受的转发通道记录在 `forwarded_channels`：隧道原始字节绝不回显，
/// 否则 "Got " 前缀会污染经隧道转发的 SSH 握手流。
#[derive(Clone)]
struct MockHandler {
    accepted_key: Option<russh::keys::PublicKey>,
    forwarded: Option<Arc<AtomicUsize>>,
    forwarded_channels: Arc<Mutex<HashSet<russh::ChannelId>>>,
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
        drop(channel);
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
        eprintln!("[mock] data 回调收到 {} 字节", data.len());
        let reply = format!("Got data: {}\r\n", String::from_utf8_lossy(data));
        session.data(channel, reply.into_bytes())?;
        eprintln!("[mock] 已回显");
        Ok(())
    }
}

/// 独立线程 + 独立 tokio runtime 里的 mock 服务器；drop 即整体收场。
struct MockServer {
    runtime: Option<tokio::runtime::Runtime>,
    port: u16,
}

impl MockServer {
    /// 在 `port` 上起服务器（0 = 临时端口）；端口被占用时短暂重试，
    /// 以支持「同端口换主机键」的用例。转发默认关闭。
    fn start(port: u16) -> Self {
        Self::start_with_key(port, None)
    }

    fn start_with_key(port: u16, accepted_key: Option<russh::keys::PublicKey>) -> Self {
        Self::start_handler(
            port,
            MockHandler {
                accepted_key,
                forwarded: None,
                forwarded_channels: Arc::new(Mutex::new(HashSet::new())),
            },
        )
    }

    /// 起一台允许 direct-tcpip 转发的 mock；返回的计数器记录接受的转发请求数。
    fn start_forwarding(port: u16) -> (Self, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        (
            Self::start_handler(
                port,
                MockHandler {
                    accepted_key: None,
                    forwarded: Some(Arc::clone(&counter)),
                    forwarded_channels: Arc::new(Mutex::new(HashSet::new())),
                },
            ),
            counter,
        )
    }

    fn start_handler(port: u16, handler: MockHandler) -> Self {
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

        Self {
            runtime: Some(runtime),
            port: bound_port,
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
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
