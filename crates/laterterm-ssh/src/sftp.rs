//! SFTP 文件通道（M3）：与 shell 会话同一套端点/认证/跳板机语义。
//!
//! 形态：请求-应答式阻塞 API（[`SftpHandle`]）——每个操作经无界通道发给
//! 网络线程（tokio 限网络层的既定口径），调用方在工作线程上同步等待应答，
//! UI 层拿到的是普通 `io::Result`。进度回调按累计字节触发，适合直连进度条。
//!
//! 文件传输语义：下载 `open(READ)` → 64KiB 循环 → 本地 BufWriter 落盘；
//! 上传 `create(CREATE|TRUNCATE|WRITE)` → 同款循环。远端文件句柄用毕即
//! `close()`（SFTP 句柄数是服务器配额，不关会泄漏）。

use crate::{ClientHandler, connect_and_auth};
use russh::client::Handle;
use std::io;
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

/// 网络线程指令（tokio 无界通道；应答走内嵌 std 通道，同步侧阻塞 recv）。
enum SftpCmd {
    ReadDir {
        path: String,
        reply: std_mpsc::Sender<io::Result<Vec<RemoteEntry>>>,
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
        reply: std_mpsc::Sender<io::Result<u64>>,
    },
    Upload {
        local_path: PathBuf,
        remote_path: String,
        progress: Option<ProgressFn>,
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
    pub fn spawn(
        endpoint: crate::SshEndpoint,
        jump: Option<Box<crate::SshEndpoint>>,
    ) -> io::Result<Self> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<SftpCmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<io::Result<()>>();
        let network_endpoint = endpoint.clone();
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
            .recv_timeout(std::time::Duration::from_secs(30))
            .map_err(|_| io::Error::other("SFTP 通道建立超时（30s 无响应）"))??;

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
    pub fn download(
        &self,
        remote_path: &str,
        local_path: impl Into<PathBuf>,
        progress: Option<ProgressFn>,
    ) -> io::Result<u64> {
        self.call(|reply| SftpCmd::Download {
            remote_path: remote_path.to_string(),
            local_path: local_path.into(),
            progress,
            reply,
        })
    }

    /// 上传本地文件到远端（返回总字节数；progress 按累计字节触发）。
    pub fn upload(
        &self,
        local_path: impl Into<PathBuf>,
        remote_path: &str,
        progress: Option<ProgressFn>,
    ) -> io::Result<u64> {
        self.call(|reply| SftpCmd::Upload {
            local_path: local_path.into(),
            remote_path: remote_path.to_string(),
            progress,
            reply,
        })
    }

    /// 关闭通道（幂等；网络线程随之退出）。
    pub fn close(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.cmd_tx.send(SftpCmd::Shutdown);
    }
}

/// （可选跳板机）建立到端点的传输层并开 sftp 子系统。
async fn open_sftp(
    endpoint: &crate::SshEndpoint,
    jump: Option<&crate::SshEndpoint>,
) -> io::Result<(
    russh_sftp::client::SftpSession,
    Handle<ClientHandler>,
    Option<Handle<ClientHandler>>,
)> {
    // 跳板腿：与 shell 会话同一语义（bastion Handle 存活期 = 通道存活期）
    let bastion = match jump {
        Some(jump_endpoint) => Some(connect_and_auth(jump_endpoint).await?),
        None => None,
    };
    let handle = match &bastion {
        Some(bastion_handle) => {
            let forward = bastion_handle
                .channel_open_direct_tcpip(
                    endpoint.host.as_str(),
                    endpoint.port() as u32,
                    "127.0.0.1",
                    0,
                )
                .await
                .map_err(|error| io::Error::other(format!("跳板机开转发通道失败: {error}")))?;
            let handler = ClientHandler {
                host_key_policy: endpoint.host_key_policy.clone(),
                host: endpoint.host.clone(),
                port: endpoint.port(),
            };
            let mut handle = russh::client::connect_stream(
                std::sync::Arc::new(russh::client::Config::default()),
                forward.into_stream(),
                handler,
            )
            .await
            .map_err(|error| io::Error::other(format!("经跳板机连接失败: {error}")))?;
            crate::authenticate(&mut handle, endpoint).await?;
            handle
        }
        None => connect_and_auth(endpoint).await?,
    };

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
    Ok((sftp, handle, bastion))
}

async fn sftp_network_main(
    endpoint: crate::SshEndpoint,
    jump: Option<Box<crate::SshEndpoint>>,
    ready_tx: std_mpsc::Sender<io::Result<()>>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<SftpCmd>,
) {
    let startup = async {
        let (sftp, handle, bastion) = open_sftp(&endpoint, jump.as_deref()).await?;
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
                    reply,
                } => {
                    let _ = reply.send(
                        download_file(&sftp, &remote_path, &local_path, progress.as_ref()).await,
                    );
                }
                SftpCmd::Upload {
                    local_path,
                    remote_path,
                    progress,
                    reply,
                } => {
                    let _ = reply.send(
                        upload_file(&sftp, &local_path, &remote_path, progress.as_ref()).await,
                    );
                }
            }
        }
        // 收尾：关 SFTP 会话与两条 SSH 连接（尽力而为）
        let _ = sftp.close().await;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "sftp closed", "en")
            .await;
        if let Some(bastion) = bastion {
            let _ = bastion
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

/// 传输循环的块大小：SFTP 报文上限内的甜点值，进度粒度也够细。
const CHUNK: usize = 64 * 1024;

async fn download_file(
    sftp: &russh_sftp::client::SftpSession,
    remote_path: &str,
    local_path: &std::path::Path,
    progress: Option<&ProgressFn>,
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
}
