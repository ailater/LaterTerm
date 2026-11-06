//! SFTP 文件通道（M3）：与 shell 会话同一套端点/认证/跳板机语义。
//!
//! 形态：请求-应答式阻塞 API（[`SftpHandle`]）——每个操作经无界通道发给
//! 网络线程（tokio 限网络层的既定口径），调用方在工作线程上同步等待应答，
//! UI 层拿到的是普通 `io::Result`。进度回调按累计字节触发，适合直连进度条。
//!
//! 文件传输语义：下载 `open(READ)` → 64KiB 循环 → 本地 BufWriter 落盘；
//! 上传 `create(CREATE|TRUNCATE|WRITE)` → 同款循环。远端文件句柄用毕即
//! `close()`（SFTP 句柄数是服务器配额，不关会泄漏）。
//!
//! 跳板链：`spawn(endpoint, jump)` 与 shell 会话的 `SshOptions.jump` 同语义
//! （ProxyJump，由外到内）：外层跳板直连，其余跳 + 目标机逐跳 direct-tcpip
//! 穿透（[`crate::open_through`]），中间跳 Handle 存入 tunnels 保活，退出时
//! 先断目标机、再按建立顺序逆序断跳板链。连接超时与 shell 同公式
//! [`crate::connect_timeout_secs`]（15s + 10s×跳数），spawn 侧再 +1s 联动。
//!
//! 取消语义：网络线程逐条串行处理指令，在途传输**不可被 close() 打断**——
//! `close()` 只是排队一条 Shutdown，会等传输跑完才被看到。逐传输取消走
//! `download`/`upload` 的 `cancel` 标志（`Arc<AtomicBool>`）：传输循环每块
//! 检查一次，置位后返回 `ErrorKind::Interrupted`（「传输已取消」），调用方
//! 负责清理半成品。`close()` 仅用于整体关闭通道（幂等；网络线程随之退出，
//! 之后在途的阻塞调用以「SFTP 网络线程已退出」报错）。
//!
//! 编码：SFTP 协议规范路径恒 UTF-8；[`crate::SshEndpoint::encoding`] 只被
//! shell 会话消费，在 SFTP 路径不生效——GBK 老服务器的 SFTP 子系统若返回
//! 非 UTF-8 名会原样透传（不做转码，russh-sftp 无字节级出口）。

use crate::{ClientHandler, connect_and_auth, open_through};
use russh::client::Handle;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;

/// 远程目录条目（read_dir 的应答单元）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 修改时间（Unix 秒；服务器未提供则 None）。
    pub modified: Option<u64>,
}

/// POSIX 权限位判目录（S_IFDIR = 0o040000；S_IFMT = 0o170000）。
fn is_dir_from_permissions(permissions: Option<u32>) -> bool {
    permissions
        .map(|bits| bits & 0o170000 == 0o040000)
        .unwrap_or(false)
}

/// 进度回调：参数为累计已传输字节数。
pub type ProgressFn = Arc<dyn Fn(u64) + Send + Sync>;

/// 逐传输取消标志：传输循环每块检查一次，置位即以 Interrupted 返回。
pub type CancelFlag = Arc<AtomicBool>;

/// 取消错误的统一形态：kind = Interrupted，文案固定「传输已取消」，
/// 调用方按 `cancel.load()` 判定归属（错误文本仅用于展示）。
fn cancelled_error() -> io::Error {
    io::Error::new(ErrorKind::Interrupted, "传输已取消")
}

/// 网络线程指令（tokio 无界通道；应答走内嵌 std 通道，同步侧阻塞 recv）。
enum SftpCmd {
    ReadDir {
        path: String,
        reply: std_mpsc::Sender<io::Result<Vec<RemoteEntry>>>,
    },
    Stat {
        path: String,
        reply: std_mpsc::Sender<io::Result<RemoteEntry>>,
    },
    RealPath {
        path: String,
        reply: std_mpsc::Sender<io::Result<String>>,
    },
    CreateDir {
        path: String,
        reply: std_mpsc::Sender<io::Result<()>>,
    },
    RemoveFile {
        path: String,
        reply: std_mpsc::Sender<io::Result<()>>,
    },
    RemoveDir {
        path: String,
        reply: std_mpsc::Sender<io::Result<()>>,
    },
    Rename {
        old_path: String,
        new_path: String,
        reply: std_mpsc::Sender<io::Result<()>>,
    },
    Download {
        remote_path: String,
        local_path: PathBuf,
        progress: Option<ProgressFn>,
        cancel: Option<CancelFlag>,
        reply: std_mpsc::Sender<io::Result<u64>>,
    },
    Upload {
        local_path: PathBuf,
        remote_path: String,
        progress: Option<ProgressFn>,
        cancel: Option<CancelFlag>,
        reply: std_mpsc::Sender<io::Result<u64>>,
    },
    Shutdown,
}

/// SFTP 通道句柄：所有方法阻塞至应答（在工作线程调用，勿在 UI 线程）。
///
/// 与 [`crate::SshSession`] 同一套端点语义（认证/主机密钥/跳板机穿透），
/// 但走 `sftp` 子系统而非 PTY——一个端点两种通道，按需各开各的。
pub struct SftpHandle {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SftpCmd>,
    closing: AtomicBool,
}

impl SftpHandle {
    /// 建立连接并启动 SFTP 子系统。
    ///
    /// `jump` 为跳板链（ProxyJump 语义，由外到内，`ssh -J` 顺序），空 = 直连。
    /// 就绪超时与 shell 会话同公式 [`crate::connect_timeout_secs`] 再 +1s
    /// （多跳握手逐跳串行，超时不随跳数缩放会「能连却被自家超时判死」）。
    pub fn spawn(endpoint: crate::SshEndpoint, jump: Vec<crate::SshEndpoint>) -> io::Result<Self> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<SftpCmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<io::Result<()>>();
        let network_endpoint = endpoint.clone();
        // 超时按跳数缩放；spawn 侧比网络侧多留 1s，让网络线程先报真实原因
        let jump_hops = jump.len();
        let timeout_secs = crate::connect_timeout_secs(jump_hops);
        let network = std::thread::Builder::new()
            .name("laterterm-sftp".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("构建 tokio runtime 失败");
                runtime.block_on(sftp_network_main(network_endpoint, jump, ready_tx, cmd_rx));
            })
            .map_err(io::Error::other)?;
        let _ = network;
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(timeout_secs + 1))
            .map_err(|_| {
                io::Error::new(
                    ErrorKind::TimedOut,
                    format!(
                        "SFTP 通道建立超时（{}s 无响应，含 {} 跳跳板）",
                        timeout_secs + 1,
                        jump_hops
                    ),
                )
            })??;

        Ok(Self {
            cmd_tx,
            closing: AtomicBool::new(false),
        })
    }

    fn call<T>(
        &self,
        make: impl FnOnce(std_mpsc::Sender<io::Result<T>>) -> SftpCmd,
    ) -> io::Result<T> {
        let (reply_tx, reply_rx) = std_mpsc::channel::<io::Result<T>>();
        self.cmd_tx
            .send(make(reply_tx))
            .map_err(|_| io::Error::other("SFTP 通道已关闭"))?;
        reply_rx
            .recv()
            .map_err(|_| io::Error::other("SFTP 网络线程已退出"))?
    }

    /// 列目录（`.`/`..` 已由底层过滤）。
    pub fn read_dir(&self, path: &str) -> io::Result<Vec<RemoteEntry>> {
        self.call(|reply| SftpCmd::ReadDir {
            path: path.to_string(),
            reply,
        })
    }

    /// 查询单个路径的元数据（进度条取文件总大小用）。
    pub fn stat(&self, path: &str) -> io::Result<RemoteEntry> {
        self.call(|reply| SftpCmd::Stat {
            path: path.to_string(),
            reply,
        })
    }

    /// 规范化为远端绝对路径（SFTP realpath；面板初始目录用）。
    pub fn realpath(&self, path: &str) -> io::Result<String> {
        self.call(|reply| SftpCmd::RealPath {
            path: path.to_string(),
            reply,
        })
    }

    pub fn create_dir(&self, path: &str) -> io::Result<()> {
        self.call(|reply| SftpCmd::CreateDir {
            path: path.to_string(),
            reply,
        })
    }

    pub fn remove_file(&self, path: &str) -> io::Result<()> {
        self.call(|reply| SftpCmd::RemoveFile {
            path: path.to_string(),
            reply,
        })
    }

    pub fn remove_dir(&self, path: &str) -> io::Result<()> {
        self.call(|reply| SftpCmd::RemoveDir {
            path: path.to_string(),
            reply,
        })
    }

    pub fn rename(&self, old_path: &str, new_path: &str) -> io::Result<()> {
        self.call(|reply| SftpCmd::Rename {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
            reply,
        })
    }

    /// 下载远端文件到本地（返回总字节数；progress 按累计字节触发）。
    ///
    /// `cancel` 置位后循环在下一次分块检查时以
    /// [`ErrorKind::Interrupted`] 返回，本地半成品由调用方删除。
    pub fn download(
        &self,
        remote_path: &str,
        local_path: impl Into<PathBuf>,
        progress: Option<ProgressFn>,
        cancel: Option<CancelFlag>,
    ) -> io::Result<u64> {
        self.call(|reply| SftpCmd::Download {
            remote_path: remote_path.to_string(),
            local_path: local_path.into(),
            progress,
            cancel,
            reply,
        })
    }

    /// 上传本地文件到远端（返回总字节数；progress 按累计字节触发）。
    ///
    /// `cancel` 置位后循环在下一次分块检查时以
    /// [`ErrorKind::Interrupted`] 返回，远端半成品由调用方删除。
    pub fn upload(
        &self,
        local_path: impl Into<PathBuf>,
        remote_path: &str,
        progress: Option<ProgressFn>,
        cancel: Option<CancelFlag>,
    ) -> io::Result<u64> {
        self.call(|reply| SftpCmd::Upload {
            local_path: local_path.into(),
            remote_path: remote_path.to_string(),
            progress,
            cancel,
            reply,
        })
    }

    /// 关闭通道（幂等；网络线程随之退出）。
    ///
    /// 注意：只是排队一条 Shutdown 指令，**不能中断在途传输**（网络线程
    /// 逐条串行，Shutdown 会等到传输完成后才被处理）——逐传输取消请用
    /// `download`/`upload` 的 `cancel` 标志。
    pub fn close(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.cmd_tx.send(SftpCmd::Shutdown);
    }
}

/// （可选跳板链）逐跳穿透建立到端点的传输层并开 sftp 子系统。
///
/// 链式穿透与 shell 会话的 [`crate::network_main`] 同款：外→内逐跳
/// direct-tcpip，中间跳 Handle 存入返回的 tunnels 保活（drop 即断流），
/// 退出时调用方先断目标机、再逆序断跳板链。
async fn open_sftp(
    endpoint: &crate::SshEndpoint,
    jump: &[crate::SshEndpoint],
) -> io::Result<(
    russh_sftp::client::SftpSession,
    Handle<ClientHandler>,
    Vec<Handle<ClientHandler>>,
)> {
    // 第一跳：外层跳板直连；无跳板链时第一跳就是目标机
    let mut handle = match jump.first() {
        Some(first) => connect_and_auth(first).await?,
        None => connect_and_auth(endpoint).await?,
    };
    // 后续跳：跳板链剩余跳 + 目标机，都经上一跳的 direct-tcpip 穿透
    let mut tunnels: Vec<Handle<ClientHandler>> = Vec::new();
    if !jump.is_empty() {
        let total = jump.len();
        let chain: Vec<&crate::SshEndpoint> =
            jump[1..].iter().chain(std::iter::once(endpoint)).collect();
        for (index, next_endpoint) in chain.into_iter().enumerate() {
            // 正在穿过的机器是第 index+1 跳（1 起），如「跳板[1/2] ops@x」
            let through = &jump[index];
            let label = format!("跳板[{}/{}] {}", index + 1, total, through.title());
            let next = open_through(&handle, &label, next_endpoint).await?;
            tunnels.push(handle);
            handle = next;
        }
    }

    let channel = handle
        .channel_open_session()
        .await
        .map_err(|error| io::Error::other(format!("打开 SFTP 通道失败: {error}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| io::Error::other(format!("请求 sftp 子系统失败: {error}")))?;
    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|error| io::Error::other(format!("SFTP 握手失败: {error}")))?;
    Ok((sftp, handle, tunnels))
}

async fn sftp_network_main(
    endpoint: crate::SshEndpoint,
    jump: Vec<crate::SshEndpoint>,
    ready_tx: std_mpsc::Sender<io::Result<()>>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<SftpCmd>,
) {
    let startup = async {
        let (sftp, handle, tunnels) = open_sftp(&endpoint, &jump).await?;
        let _ = ready_tx.send(Ok(()));
        // 常驻：逐条执行指令直至 Shutdown 或通道失效
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                SftpCmd::Shutdown => break,
                SftpCmd::ReadDir { path, reply } => {
                    let result = match sftp.read_dir(&path).await {
                        Ok(entries) => Ok(entries
                            .map(|entry| {
                                let metadata = entry.metadata();
                                RemoteEntry {
                                    name: entry.file_name(),
                                    is_dir: is_dir_from_permissions(metadata.permissions),
                                    size: metadata.size.unwrap_or(0),
                                    modified: metadata.mtime.map(u64::from),
                                }
                            })
                            .collect::<Vec<_>>()),
                        Err(error) => Err(io::Error::other(format!("read_dir {path}: {error}"))),
                    };
                    let _ = reply.send(result);
                }
                SftpCmd::Stat { path, reply } => {
                    let result = match sftp.metadata(&path).await {
                        Ok(metadata) => Ok(RemoteEntry {
                            name: basename_of(&path),
                            is_dir: is_dir_from_permissions(metadata.permissions),
                            size: metadata.size.unwrap_or(0),
                            modified: metadata.mtime.map(u64::from),
                        }),
                        Err(error) => Err(io::Error::other(format!("stat {path}: {error}"))),
                    };
                    let _ = reply.send(result);
                }
                SftpCmd::RealPath { path, reply } => {
                    let result = match sftp.canonicalize(&path).await {
                        Ok(real) => Ok(real),
                        Err(error) => Err(io::Error::other(format!("realpath {path}: {error}"))),
                    };
                    let _ = reply.send(result);
                }
                SftpCmd::CreateDir { path, reply } => {
                    let _ = reply.send(
                        sftp.create_dir(&path)
                            .await
                            .map_err(|e| io::Error::other(format!("create_dir {path}: {e}"))),
                    );
                }
                SftpCmd::RemoveFile { path, reply } => {
                    let _ = reply.send(
                        sftp.remove_file(&path)
                            .await
                            .map_err(|e| io::Error::other(format!("remove_file {path}: {e}"))),
                    );
                }
                SftpCmd::RemoveDir { path, reply } => {
                    let _ = reply.send(
                        sftp.remove_dir(&path)
                            .await
                            .map_err(|e| io::Error::other(format!("remove_dir {path}: {e}"))),
                    );
                }
                SftpCmd::Rename {
                    old_path,
                    new_path,
                    reply,
                } => {
                    let _ = reply.send(sftp.rename(&old_path, &new_path).await.map_err(|e| {
                        io::Error::other(format!("rename {old_path}→{new_path}: {e}"))
                    }));
                }
                SftpCmd::Download {
                    remote_path,
                    local_path,
                    progress,
                    cancel,
                    reply,
                } => {
                    let _ = reply.send(
                        download_file(
                            &sftp,
                            &remote_path,
                            &local_path,
                            progress.as_ref(),
                            cancel.as_ref(),
                        )
                        .await,
                    );
                }
                SftpCmd::Upload {
                    local_path,
                    remote_path,
                    progress,
                    cancel,
                    reply,
                } => {
                    let _ = reply.send(
                        upload_file(
                            &sftp,
                            &local_path,
                            &remote_path,
                            progress.as_ref(),
                            cancel.as_ref(),
                        )
                        .await,
                    );
                }
            }
        }
        // 收尾：关 SFTP 会话，先断目标机，再按建立顺序逆序断跳板链（尽力而为）
        let _ = sftp.close().await;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "sftp closed", "en")
            .await;
        for tunnel in tunnels.iter().rev() {
            let _ = tunnel
                .disconnect(russh::Disconnect::ByApplication, "sftp closed", "en")
                .await;
        }
        io::Result::<()>::Ok(())
    }
    .await;
    if let Err(error) = startup {
        let _ = ready_tx.send(Err(error));
    }
}

/// 路径末段（stat 的展示名）；根目录与空路径退化为自身。
fn basename_of(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        Some(_) | None if trimmed.is_empty() => "/".to_string(),
        _ => trimmed.to_string(),
    }
}

/// 传输循环的块大小：SFTP 报文上限内的甜点值，进度粒度也够细。
const CHUNK: usize = 64 * 1024;

async fn download_file(
    sftp: &russh_sftp::client::SftpSession,
    remote_path: &str,
    local_path: &std::path::Path,
    progress: Option<&ProgressFn>,
    cancel: Option<&CancelFlag>,
) -> io::Result<u64> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut remote = sftp
        .open(remote_path)
        .await
        .map_err(|e| io::Error::other(format!("打开远端 {remote_path}: {e}")))?;
    let mut local = tokio::io::BufWriter::new(
        tokio::fs::File::create(local_path)
            .await
            .map_err(|e| io::Error::other(format!("创建本地 {}: {e}", local_path.display())))?,
    );
    let mut total = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        // 逐块取消检查：网络线程串行，close() 无法打断在途传输
        if let Some(cancel) = cancel
            && cancel.load(Ordering::Relaxed)
        {
            return Err(cancelled_error());
        }
        let n = remote
            .read(&mut buf)
            .await
            .map_err(|e| io::Error::other(format!("读取远端: {e}")))?;
        if n == 0 {
            break;
        }
        local
            .write_all(&buf[..n])
            .await
            .map_err(|e| io::Error::other(format!("写本地: {e}")))?;
        total += n as u64;
        if let Some(progress) = progress {
            progress(total);
        }
    }
    local
        .flush()
        .await
        .map_err(|e| io::Error::other(format!("落盘: {e}")))?;
    // SFTP 句柄是服务器配额，用毕即还
    let _ = remote.close().await;
    Ok(total)
}

async fn upload_file(
    sftp: &russh_sftp::client::SftpSession,
    local_path: &std::path::Path,
    remote_path: &str,
    progress: Option<&ProgressFn>,
    cancel: Option<&CancelFlag>,
) -> io::Result<u64> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut local = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| io::Error::other(format!("打开本地 {}: {e}", local_path.display())))?;
    let mut remote = sftp
        .create(remote_path)
        .await
        .map_err(|e| io::Error::other(format!("创建远端 {remote_path}: {e}")))?;
    let mut total = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        // 逐块取消检查：网络线程串行，close() 无法打断在途传输
        if let Some(cancel) = cancel
            && cancel.load(Ordering::Relaxed)
        {
            return Err(cancelled_error());
        }
        let n = local
            .read(&mut buf)
            .await
            .map_err(|e| io::Error::other(format!("读本地: {e}")))?;
        if n == 0 {
            break;
        }
        remote
            .write_all(&buf[..n])
            .await
            .map_err(|e| io::Error::other(format!("写远端: {e}")))?;
        total += n as u64;
        if let Some(progress) = progress {
            progress(total);
        }
    }
    remote
        .flush()
        .await
        .map_err(|e| io::Error::other(format!("远端冲刷: {e}")))?;
    let _ = remote.close().await;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 目录权限位判定（S_IFDIR）；文件/未知属性不该误判。
    #[test]
    fn permissions_dir_bit_detection() {
        let dir = 0o040755; // drwxr-xr-x
        let file = 0o100644; // -rw-r--r--
        let symlink = 0o120777; // lrwxrwxrwx
        assert!(is_dir_from_permissions(Some(dir)));
        assert!(!is_dir_from_permissions(Some(file)));
        assert!(!is_dir_from_permissions(Some(symlink)));
        assert!(!is_dir_from_permissions(None), "无属性按文件处理");
    }

    /// stat 的展示名：常规路径取末段；尾斜杠、根、空路径都有确定结果。
    #[test]
    fn basename_of_variants() {
        assert_eq!(basename_of("/var/log/syslog"), "syslog");
        assert_eq!(basename_of("/var/log/syslog/"), "syslog");
        assert_eq!(basename_of("plain.txt"), "plain.txt");
        assert_eq!(basename_of("/"), "/");
        assert_eq!(basename_of(""), "/");
        assert_eq!(basename_of("中文名.tar.gz"), "中文名.tar.gz");
    }
}
