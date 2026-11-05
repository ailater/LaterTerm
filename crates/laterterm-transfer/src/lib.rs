//! 终端内文件传输（M3）：ZMODEM（rz/sz）兼容通道。
//!
//! 形态：[`SessionStreams`] 的又一个装饰器（与 GBK 转码同挂载点）——
//! 读方向持续扫描 ZMODEM 触发序列（`**␘B0`，ZRQINIT 帧头），平时字节
//! 原样穿透给视图；触发后把后续字节**连同触发帧**导入管道，由工作线程
//! 跑 rzsz crate 的真·ZMODEM 状态机（其读端要求 `Read + AsFd` 做超时
//! 轮询，管道端天然满足），协议应答经共享写端回发远端。
//!
//! 用户中止路径天然成立：写端是 UI 键入与协议应答的共享出口，连按
//! 5×CAN 直达远端 sz。
//!
//! trzsz 自有协议（trz/tsz，`::` 文本帧触发）为后续演进：挂同一装饰器
//! 骨架，只换触发串与帧编解码。
//!
//! 安全口径：`junk_path` 打开——远端路径一律剥离，仅按文件名落到
//! `download_dir`，杜绝 `../../` 穿越。
//!
//! 平台边界：Unix 启用 ZMODEM/trzsz 自动传输；Windows 暂不支持自动传输，
//! [`wrap`] 原样返回会话流，保留终端读写，不检测/吞掉触发字节或自动落盘。

#[cfg(unix)]
mod trzsz;

use laterterm_core::SessionStreams;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
#[cfg(unix)]
use std::sync::{Arc, Mutex};

/// 传输事件（UI/日志观察口）。
#[derive(Clone, Debug)]
pub enum TransferEvent {
    /// 检测到 ZMODEM 触发序列，接收已启动。
    Started,
    /// 全部文件接收完成（参数为落盘路径）。
    Finished(Vec<PathBuf>),
    /// 传输失败/中止（协议错误、远端取消、会话 EOF）。
    Failed(String),
}

/// 装饰选项。
pub struct TransferOptions {
    /// 接收文件的落盘目录（不存在则自动创建）。
    pub download_dir: PathBuf,
    /// 事件接收端。
    pub events: std_mpsc::Sender<TransferEvent>,
    /// 上传文件列表（R 触发时推送给远端；None = 优雅拒绝）。
    pub upload_files: Option<Vec<PathBuf>>,
}

/// ZMODEM ZRQINIT 触发序列：`** ␘(CAN) B 0`（lrzsz 同款检测串）。
#[cfg(unix)]
const ZMODEM_TRIGGER: &[u8] = b"**\x18B0";

/// 会话字节流的共享写端：UI 键入与协议应答同一出口。
#[cfg(unix)]
#[derive(Clone)]
pub(crate) struct SharedWriter {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

#[cfg(unix)]
impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap().flush()
    }
}

/// 给会话字节流装上 ZMODEM/trzsz 通道（Unix）。
#[cfg(unix)]
pub fn wrap(streams: SessionStreams, options: TransferOptions) -> SessionStreams {
    let shared = SharedWriter {
        inner: Arc::new(Mutex::new(streams.writer)),
    };
    SessionStreams {
        reader: Box::new(TransferReader {
            inner: streams.reader,
            state: ReaderState::Scanning,
            carry: Vec::new(),
            reply: shared.clone(),
            options,
        }),
        writer: Box::new(shared),
    }
}

/// Windows 暂不支持 ZMODEM/trzsz 自动传输：保持原始会话流，不吞协议字节。
#[cfg(windows)]
pub fn wrap(streams: SessionStreams, _options: TransferOptions) -> SessionStreams {
    streams
}

/// 读装饰器：扫描 / 穿透 / 触发分流 三态。
#[cfg(unix)]
struct TransferReader {
    inner: Box<dyn Read + Send>,
    state: ReaderState,
    /// 扫描态下跨块边界的候选尾巴（< TRIGGER.len() 字节）
    carry: Vec<u8>,
    /// 协议应答回远端的出口（工作线程用）
    reply: SharedWriter,
    options: TransferOptions,
}

#[cfg(unix)]
enum ReaderState {
    /// 常态：字节穿透给视图，滚动扫描触发序列。
    Scanning,
    /// ZMODEM 进行中：字节导入管道喂工作线程。
    Active {
        /// 管道写端（工作线程持读端）
        pump: UnixStream,
        /// 工作线程完成标志
        done: Arc<AtomicBool>,
    },
}

#[cfg(unix)]
impl Read for TransferReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match &mut self.state {
            ReaderState::Active { pump, done } => {
                if done.load(Ordering::Acquire) {
                    self.state = ReaderState::Scanning;
                    self.carry.clear();
                    return self.scan_chunk(&[], buf);
                }
                let n = self.inner.read(buf)?;
                if n == 0 {
                    // 会话 EOF：断管让工作线程的接收循环收场
                    self.state = ReaderState::Scanning;
                    return Ok(0);
                }
                if pump.write_all(&buf[..n]).is_err() {
                    // 工作线程已退出（读端 drop）：当前块回到扫描态处理
                    self.state = ReaderState::Scanning;
                    let chunk: Vec<u8> = buf[..n].to_vec();
                    return self.scan_chunk(&chunk, buf);
                }
                // 协议字节已转喂状态机，不进视图；报告消费以驱动读泵
                Ok(n)
            }
            ReaderState::Scanning => loop {
                let n = self.inner.read(buf)?;
                if n == 0 {
                    // EOF：carry 里的候选前缀已无成触发可能，冲给视图
                    if self.carry.is_empty() {
                        return Ok(0);
                    }
                    let take = self.carry.len().min(buf.len());
                    buf[..take].copy_from_slice(&self.carry[..take]);
                    self.carry.drain(..take);
                    return Ok(take);
                }
                let chunk: Vec<u8> = buf[..n].to_vec();
                let produced = self.scan_chunk(&chunk, buf)?;
                // 无产出 = 整块进了 carry（触发尾巴/未齐行）：
                // 必须继续拉内层，返回 0 会被调用方当 EOF
                if produced > 0 || matches!(self.state, ReaderState::Active { .. }) {
                    return Ok(produced);
                }
            },
        }
    }
}

#[cfg(unix)]
impl TransferReader {
    /// 在内容（carry + chunk）里扫描触发；命中则起传输并吞掉本块，
    /// 未命中则放行安全前缀（跨块尾巴留在 carry）。
    fn scan_chunk(&mut self, chunk: &[u8], buf: &mut [u8]) -> io::Result<usize> {
        let mut window = std::mem::take(&mut self.carry);
        window.extend_from_slice(chunk);

        // 三触发扫描：取最早出现者；ZMODEM 从触发帧起全量导入，
        // trzsz S/R 吞掉整行触发串、从行尾起导入（触发行不是 # 帧）
        let zmodem_at = find_subslice(&window, ZMODEM_TRIGGER);
        let trzsz_s_at = find_subslice(&window, trzsz::TRZSZ_TRIGGER);
        let trzsz_r_at = find_subslice(&window, trzsz::TRZSZ_R_TRIGGER);
        // 按位置排序取最早
        let route = [
            zmodem_at.map(|p| (Route::Zmodem, p)),
            trzsz_s_at.map(|p| (Route::TrzszReceive, p)),
            trzsz_r_at.map(|p| (Route::TrzszUpload, p)),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(_, pos)| *pos);

        enum Route {
            Zmodem,
            TrzszReceive,
            TrzszUpload,
        }

        if let Some((route, pos)) = route {
            let _ = self.options.events.send(TransferEvent::Started);
            // trzsz 系触发行需完整到达才能定位行尾；行未到齐则等下一块
            let after_trigger = &window[pos..];
            let divert = match route {
                Route::Zmodem => spawn_receiver(&self.options, self.reply.clone(), after_trigger),
                Route::TrzszReceive | Route::TrzszUpload => {
                    match after_trigger.iter().position(|b| *b == b'\n') {
                        Some(nl) => match route {
                            Route::TrzszReceive => spawn_trzsz(
                                &self.options,
                                self.reply.clone(),
                                &after_trigger[nl + 1..],
                            ),
                            Route::TrzszUpload => spawn_trzsz_upload(
                                &self.options,
                                self.reply.clone(),
                                &after_trigger[nl + 1..],
                            ),
                            Route::Zmodem => unreachable!(),
                        },
                        None => {
                            // 触发行未到齐：前缀放行，触发行整体扣进 carry 等下轮
                            let out_len = pos.min(buf.len());
                            buf[..out_len].copy_from_slice(&window[..out_len]);
                            self.carry = window[pos..].to_vec();
                            return Ok(out_len);
                        }
                    }
                }
            };
            match divert {
                Some((pump, done)) => {
                    self.state = ReaderState::Active { pump, done };
                    // 触发帧在内全部转喂：本块不给视图
                    return Ok(chunk.len().max(1));
                }
                None => {
                    let _ = self
                        .options
                        .events
                        .send(TransferEvent::Failed("启动接收线程失败".into()));
                }
            }
        }

        // 未触发：放行「确定不可能是触发前缀」的部分，尾巴留待下轮
        let keep = [ZMODEM_TRIGGER, trzsz::TRZSZ_TRIGGER, trzsz::TRZSZ_R_TRIGGER]
            .iter()
            .flat_map(|trigger| (1..trigger.len()).map(move |len| &trigger[..len]))
            .filter(|prefix| window.ends_with(prefix))
            .map(|prefix| prefix.len())
            .max()
            .unwrap_or(0);
        let safe_len = window.len() - keep;
        let out_len = safe_len.min(buf.len());
        buf[..out_len].copy_from_slice(&window[..out_len]);
        self.carry = window[out_len..].to_vec();
        Ok(out_len)
    }
}

/// 在 haystack 中找 needle 的首位置。
#[cfg(unix)]
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 起 ZMODEM 接收工作线程。返回（管道写端供读泵灌数据， 完成标志）。
#[cfg(unix)]
fn spawn_receiver(
    options: &TransferOptions,
    reply: SharedWriter,
    initial: &[u8],
) -> Option<(UnixStream, Arc<AtomicBool>)> {
    std::fs::create_dir_all(&options.download_dir).ok()?;
    let (mut pump, feed) = UnixStream::pair().ok()?;
    // 初始字节（含触发帧）先行灌入：状态机需要完整 ZRQINIT 起步
    pump.write_all(initial).ok()?;

    let done = Arc::new(AtomicBool::new(false));
    let done_flag = Arc::clone(&done);
    let events = options.events.clone();
    let output_dir = options.download_dir.clone();

    std::thread::Builder::new()
        .name("laterterm-zmodem".into())
        .spawn(move || {
            let mut session = rzsz::zmodem::session::Session::new();
            let mut reader = rzsz::serial::reader::ModemReader::new(feed, 16384);
            let mut out = reply;
            // junk_path=ON 剥远端路径；restricted=ON 禁危险模式；
            // rename=ON 防覆盖已有文件
            let config = rzsz::receiver::ReceiverConfig {
                verbosity: 0,
                quiet: true,
                protect: false,
                rename: true,
                resume: false,
                restricted: true,
                binary: true,
                ascii: false,
                escape_ctrl: false,
                junk_path: true,
                output_dir,
            };
            let event =
                match rzsz::receiver::receive_files(&mut session, &mut reader, &mut out, &config) {
                    Ok(names) => {
                        let paths = names
                            .iter()
                            .map(|name| config.output_dir.join(name))
                            .collect::<Vec<_>>();
                        TransferEvent::Finished(paths)
                    }
                    Err(error) => TransferEvent::Failed(format!("ZMODEM 接收失败: {error}")),
                };
            let _ = events.send(event);
            done_flag.store(true, Ordering::Release);
        })
        .ok()?;
    Some((pump, done))
}

/// 起 trzsz 接收工作线程（与 ZMODEM 的 spawn_receiver 同骨架）。
#[cfg(unix)]
fn spawn_trzsz(
    options: &TransferOptions,
    reply: SharedWriter,
    initial: &[u8],
) -> Option<(UnixStream, Arc<AtomicBool>)> {
    std::fs::create_dir_all(&options.download_dir).ok()?;
    let (mut pump, feed) = UnixStream::pair().ok()?;
    pump.write_all(initial).ok()?;

    let done = Arc::new(AtomicBool::new(false));
    let done_flag = Arc::clone(&done);
    let thread_options = TransferOptions {
        download_dir: options.download_dir.clone(),
        events: options.events.clone(),
        upload_files: None,
    };

    std::thread::Builder::new()
        .name("laterterm-trzsz".into())
        .spawn(move || {
            trzsz::run_trzsz_receive(&thread_options, reply, feed, done_flag);
        })
        .ok()?;
    Some((pump, done))
}

/// 起 trzsz 上传工作线程（R 触发：本端推文件到远端）。
#[cfg(unix)]
fn spawn_trzsz_upload(
    options: &TransferOptions,
    reply: SharedWriter,
    initial: &[u8],
) -> Option<(UnixStream, Arc<AtomicBool>)> {
    let (mut pump, feed) = UnixStream::pair().ok()?;
    pump.write_all(initial).ok()?;

    let done = Arc::new(AtomicBool::new(false));
    let done_flag = Arc::clone(&done);
    let thread_options = TransferOptions {
        download_dir: options.download_dir.clone(),
        events: options.events.clone(),
        upload_files: options.upload_files.clone(),
    };

    std::thread::Builder::new()
        .name("laterterm-trzsz-up".into())
        .spawn(move || {
            trzsz::run_trzsz_upload(&thread_options, reply, feed, done_flag);
        })
        .ok()?;
    Some((pump, done))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn subslice_finder_basics() {
        assert_eq!(find_subslice(b"abc**\x18B0def", ZMODEM_TRIGGER), Some(3));
        assert_eq!(find_subslice(b"**\x18B0", ZMODEM_TRIGGER), Some(0));
        assert_eq!(find_subslice(b"nothing here", ZMODEM_TRIGGER), None);
        assert_eq!(find_subslice(b"nothing here", trzsz::TRZSZ_TRIGGER), None);
        assert_eq!(find_subslice(b"xy**", ZMODEM_TRIGGER), None);
        assert_eq!(find_subslice(b"", ZMODEM_TRIGGER), None);
    }

    #[test]
    fn prompt_is_delivered_without_waiting_for_more_output() {
        let (events, _) = std_mpsc::channel();
        let mut reader = TransferReader {
            inner: Box::new(io::empty()),
            reply: SharedWriter { inner: Arc::new(std::sync::Mutex::new(Box::new(io::sink()))) },
            state: ReaderState::Scanning,
            carry: Vec::new(),
            options: TransferOptions { download_dir: PathBuf::from("/tmp"), events, upload_files: None },
        };
        let prompt = b"[jii@ubuntu ~]$ ";
        let mut output = [0; 128];
        let n = reader.scan_chunk(prompt, &mut output).unwrap();
        assert_eq!(&output[..n], prompt);
        assert!(reader.carry.is_empty());
    }

    #[test]
    fn partial_trigger_is_preserved_across_chunks() {
        let (events, _) = std_mpsc::channel();
        let mut reader = TransferReader {
            inner: Box::new(io::empty()),
            reply: SharedWriter { inner: Arc::new(std::sync::Mutex::new(Box::new(io::sink()))) },
            state: ReaderState::Scanning,
            carry: Vec::new(),
            options: TransferOptions { download_dir: PathBuf::from("/tmp"), events, upload_files: None },
        };
        let mut output = [0; 128];
        let n = reader.scan_chunk(b"hello::TRZSZ:TR", &mut output).unwrap();
        assert_eq!(&output[..n], b"hello");
        assert_eq!(reader.carry, b"::TRZSZ:TR");
        let n = reader.scan_chunk(b"not-a-trigger", &mut output).unwrap();
        assert_eq!(&output[..n], b"::TRZSZ:TRnot-a-trigger");
    }

    /// 真协议回环：sz（rzsz sender）发文件 → 本装饰器检测/分流/落盘，
    /// 全程无网络。验证：触发检测、应答回通、字节完整落盘、事件序列。
    #[test]
    fn zmodem_loopback_receive() {
        let dir = tempfile::tempdir().expect("临时目录");
        let download_dir = dir.path().join("downloads");
        let payload_path = dir.path().join("payload.bin");
        // 256KiB 确定性内容（覆盖多帧 + CRC16 长帧路径）
        let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&payload_path, &payload).expect("写源文件");

        // proto 对：远端 sz 的协议输出（= 我们的会话读流）
        let (proto_s, proto_r) = UnixStream::pair().expect("proto pair");
        // reply 对：我们的协议应答（回远端 sz 的读端）
        let (reply_s, reply_r) = UnixStream::pair().expect("reply pair");

        // 远端 sz 线程：握手 → 单文件 → 收尾
        let sender_payload = payload_path.clone();
        let sender = std::thread::Builder::new()
            .name("test-sz".into())
            .spawn(move || {
                let mut session = rzsz::zmodem::session::Session::new();
                let mut reader = rzsz::serial::reader::ModemReader::new(reply_r, 16384);
                let mut out = proto_s;
                rzsz::sender::get_receiver_init(&mut session, &mut reader, &mut out)
                    .expect("sz 握手");
                let config = rzsz::sender::SenderConfig::default();
                rzsz::sender::send_file(
                    &mut session,
                    &mut reader,
                    &mut out,
                    &sender_payload,
                    &config,
                    0,
                    0,
                    None,
                )
                .expect("sz 发送");
                rzsz::sender::finish_session(&mut session, &mut reader, &mut out).expect("sz 收尾");
            })
            .expect("起 sz 线程");

        // 会话流：读端=远端协议输出，写端=应答回远端；装上传输装饰器
        let (events_tx, events_rx) = std_mpsc::channel::<TransferEvent>();
        let streams = SessionStreams {
            reader: Box::new(proto_r),
            writer: Box::new(reply_s),
        };
        let mut wrapped = wrap(
            streams,
            TransferOptions {
                download_dir: download_dir.clone(),
                events: events_tx,
                upload_files: None,
            },
        );

        // 读泵：持续读（协议字节被装饰器内部消化），直至事件到达
        let pump = std::thread::Builder::new()
            .name("test-pump".into())
            .spawn(move || {
                let mut sink = [0u8; 8192];
                loop {
                    match wrapped.reader.read(&mut sink) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            })
            .expect("起读泵");

        // 事件断言：Started → Finished(1 文件)
        let first = events_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("Started 事件");
        assert!(matches!(first, TransferEvent::Started), "首事件: {first:?}");
        let second = events_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("Finished 事件");
        let TransferEvent::Finished(paths) = second else {
            panic!("期望 Finished，实际: {second:?}");
        };
        assert_eq!(paths.len(), 1, "单文件: {paths:?}");

        // 字节级完整性
        let received = std::fs::read(&paths[0]).expect("读落盘文件");
        assert_eq!(received.len(), payload.len());
        assert_eq!(received, payload, "内容逐字节一致");

        // sz 侧也正常收尾
        sender.join().expect("sz 线程结束");
        let _ = pump.join();
    }

    /// 穿透语义：无触发序列的普通输出原样到达视图。
    #[test]
    fn passthrough_when_no_trigger() {
        struct OnceReader(Vec<u8>);
        impl Read for OnceReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() || buf.is_empty() {
                    return Ok(0);
                }
                let n = self.0.len().min(buf.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0.drain(..n);
                Ok(n)
            }
        }
        let (events_tx, _events_rx) = std_mpsc::channel();
        let streams = SessionStreams {
            reader: Box::new(OnceReader(b"ls -la\r\ntotal 0\r\n".to_vec())),
            writer: Box::new(Vec::new()),
        };
        let mut wrapped = wrap(
            streams,
            TransferOptions {
                download_dir: PathBuf::from("/tmp/laterterm-transfer-test"),
                events: events_tx,
                upload_files: None,
            },
        );
        let mut out = Vec::new();
        wrapped.reader.read_to_end(&mut out).expect("读取");
        assert_eq!(out, b"ls -la\r\ntotal 0\r\n");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::io::{self, Read, Write};
    use std::sync::{Arc, Mutex};

    #[test]
    fn unsupported_transfer_keeps_both_streams_unchanged() {
        #[derive(Clone)]
        struct MemoryWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for MemoryWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().expect("临时目录");
        let download_dir = dir.path().join("downloads");
        let bytes = b"before**\x18B0::TRZSZ:TRANSFER:S:1\r\nafter";
        let output = MemoryWriter(Arc::new(Mutex::new(Vec::new())));
        let (events, received_events) = std_mpsc::channel();
        let streams = SessionStreams {
            reader: Box::new(io::Cursor::new(bytes.to_vec())),
            writer: Box::new(output.clone()),
        };
        let mut streams = wrap(
            streams,
            TransferOptions {
                download_dir: download_dir.clone(),
                events,
                upload_files: None,
            },
        );
        let mut received = Vec::new();
        streams.reader.read_to_end(&mut received).expect("原样读流");
        streams.writer.write_all(b"input").expect("原样写流");
        assert_eq!(received, bytes);
        assert_eq!(*output.0.lock().unwrap(), b"input");
        assert!(received_events.try_recv().is_err(), "不报告虚假的传输事件");
        assert!(!download_dir.exists(), "Windows fallback 不自动落盘");
    }
}
