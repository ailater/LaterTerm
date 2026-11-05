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
        loop {
            if matches!(self.state, ReaderState::Active { .. }) {
                // 触发前已经确定是普通输出的前缀，优先交给终端；协议数据
                // 从未进入这个 carry。
                if !self.carry.is_empty() {
                    let take = self.carry.len().min(buf.len());
                    buf[..take].copy_from_slice(&self.carry[..take]);
                    self.carry.drain(..take);
                    return Ok(take);
                }

                let done = match &self.state {
                    ReaderState::Active { done, .. } => done.load(Ordering::Acquire),
                    ReaderState::Scanning => false,
                };
                if done {
                    // 协议线程已结束：恢复扫描并继续读取底层会话，不能用 0
                    // 把“协议完成”伪装成会话 EOF。
                    self.state = ReaderState::Scanning;
                    continue;
                }

                // 协议字节必须完全留在传输管道，不能借用调用方的 buf 返回；
                // 否则上层会把它再次送入 VT 解析器。
                let mut protocol_buf = [0u8; 8192];
                let n = self.inner.read(&mut protocol_buf)?;
                if n == 0 {
                    self.state = ReaderState::Scanning;
                    return Ok(0);
                }
                let pump_result = match &mut self.state {
                    ReaderState::Active { pump, .. } => pump.write_all(&protocol_buf[..n]),
                    ReaderState::Scanning => continue,
                };
                if pump_result.is_err() {
                    // 工作线程已退出：当前块回到扫描态，避免丢掉可能的普通输出。
                    self.state = ReaderState::Scanning;
                    return self.scan_chunk(&protocol_buf[..n], buf);
                }
                // 继续泵送，直到协议完成或底层会话结束。
                continue;
            }

            // 小目标缓冲区可能让 scan_chunk 暂存一段已经安全的普通输出；
            // 先把它排空，不能等底层再来一个字节，否则会在长连接上丢尾字节。
            if !self.carry.is_empty() {
                let keep = longest_trigger_prefix_suffix(&self.carry);
                let safe_len = self.carry.len().saturating_sub(keep);
                if safe_len > 0 {
                    let take = safe_len.min(buf.len());
                    buf[..take].copy_from_slice(&self.carry[..take]);
                    self.carry.drain(..take);
                    return Ok(take);
                }
            }

            let n = self.inner.read(buf)?;
            if n == 0 {
                // EOF：carry 里的候选前缀已无成触发可能，冲给视图。
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
            if produced > 0 {
                return Ok(produced);
            }
            // 无产出表示整块只是触发候选尾巴；继续拉取，不能伪造 EOF。
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
                            // 触发行未到齐：只把当前 buffer 能容纳的普通前缀
                            // 交给视图，前缀剩余部分与候选触发串一起保留。
                            let out_len = pos.min(buf.len());
                            buf[..out_len].copy_from_slice(&window[..out_len]);
                            self.carry = window[out_len..].to_vec();
                            return Ok(out_len);
                        }
                    }
                }
            };
            match divert {
                Some((pump, done)) => {
                    self.state = ReaderState::Active { pump, done };
                    // 触发帧在内全部转喂；外层 read 会继续泵送，不能伪造
                    // 一个正长度让 VT 解析器读取未初始化的调用方缓冲区。
                    return Ok(0);
                }
                None => {
                    let _ = self
                        .options
                        .events
                        .send(TransferEvent::Failed("启动接收线程失败".into()));
                }
            }
        }

        // 未触发：只保留确实可能成为触发串开头的尾部字节。
        // 普通提示符、ANSI 控制序列和 UTF-8 文本不能因为协议扫描而延迟。
        let keep = longest_trigger_prefix_suffix(&window);
        let safe_len = window.len().saturating_sub(keep);
        let out_len = safe_len.min(buf.len());
        buf[..out_len].copy_from_slice(&window[..out_len]);
        self.carry = window[out_len..].to_vec();
        Ok(out_len)
    }
}

/// 返回 `window` 尾部同时匹配某个触发串前缀的最长长度。
///
/// 完整触发串会在 [`TransferReader::scan_chunk`] 的前置扫描中处理，
/// 因而这里最多保留 `trigger.len() - 1` 字节。
#[cfg(unix)]
fn longest_trigger_prefix_suffix(window: &[u8]) -> usize {
    [ZMODEM_TRIGGER, trzsz::TRZSZ_TRIGGER, trzsz::TRZSZ_R_TRIGGER]
        .into_iter()
        .map(|trigger| {
            (1..trigger.len().min(window.len() + 1))
                .rev()
                .find(|&len| window.ends_with(&trigger[..len]))
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
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

    /// 模拟仍连接但暂无新输出的交互会话；不能用 EOF 掩盖尾部滞留。
    struct IdleReader(std::io::Cursor<Vec<u8>>);

    impl Read for IdleReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.read(buf)?;
            if n == 0 && !buf.is_empty() {
                Err(io::ErrorKind::WouldBlock.into())
            } else {
                Ok(n)
            }
        }
    }

    fn scanner(inner: impl Read + Send + 'static) -> TransferReader {
        let (events, _) = std_mpsc::channel();
        TransferReader {
            inner: Box::new(inner),
            state: ReaderState::Scanning,
            carry: Vec::new(),
            reply: SharedWriter {
                inner: Arc::new(Mutex::new(Box::new(io::sink()))),
            },
            options: TransferOptions {
                download_dir: PathBuf::new(),
                events,
                upload_files: None,
            },
        }
    }

    #[test]
    fn prompt_is_visible_before_next_output_or_eof() {
        let prompt = b"user@host:~$ ";
        let mut reader = scanner(IdleReader(io::Cursor::new(prompt.to_vec())));
        let mut buf = [0; 128];
        let n = reader.read(&mut buf).expect("静默长连接不能扣住提示符");
        assert_eq!(&buf[..n], prompt);
        assert_eq!(
            reader.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn ordinary_output_never_waits_for_eighteen_more_bytes() {
        for bytes in [
            b"x".as_slice(),
            b"user@host:~$ ",
            b"\x1b[32muser@host\x1b[0m:~$ ",
            "中文终端提示符 $ ".as_bytes(),
            b"a longer ordinary output that has no transfer prefix",
        ] {
            for buffer_size in 1..=bytes.len() + 2 {
                let mut reader = scanner(IdleReader(io::Cursor::new(bytes.to_vec())));
                let mut output = Vec::new();
                let mut buf = vec![0; buffer_size];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => panic!("长连接不能伪造 EOF"),
                        Ok(n) => output.extend_from_slice(&buf[..n]),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("读取失败: {error}"),
                    }
                }
                assert_eq!(output, bytes, "目标缓冲区大小 {buffer_size}");
            }
        }
    }

    #[test]
    fn scanner_keeps_only_actual_trigger_prefixes() {
        for trigger in [ZMODEM_TRIGGER, trzsz::TRZSZ_TRIGGER, trzsz::TRZSZ_R_TRIGGER] {
            for split in 1..trigger.len() {
                let mut reader = scanner(io::empty());
                let mut buf = [0; 128];
                let mut first = b"visible ".to_vec();
                first.extend_from_slice(&trigger[..split]);
                let n = reader.scan_chunk(&first, &mut buf).unwrap();
                assert_eq!(&buf[..n], b"visible ");
                assert_eq!(reader.carry, trigger[..split]);
                // 候选失配后，旧尾巴必须在这一块立即释放。
                let n = reader.scan_chunk(b"!", &mut buf).unwrap();
                let mut expected = trigger[..split].to_vec();
                expected.push(b'!');
                assert_eq!(&buf[..n], expected);
                assert!(reader.carry.is_empty());
            }
        }
    }

    #[test]
    fn completed_transfer_resumes_scanning_without_false_eof() {
        let mut reader = scanner(IdleReader(io::Cursor::new(b"user@host:~$ ".to_vec())));
        let (pump, _feed) = UnixStream::pair().unwrap();
        reader.state = ReaderState::Active {
            pump,
            done: Arc::new(AtomicBool::new(true)),
        };
        let mut buf = [0; 128];
        let n = reader.read(&mut buf).expect("完成后继续读取会话");
        assert_eq!(&buf[..n], b"user@host:~$ ");
    }

    #[test]
    fn active_protocol_bytes_are_not_terminal_output() {
        let mut reader = scanner(IdleReader(io::Cursor::new(b"protocol frame".to_vec())));
        let (pump, mut feed) = UnixStream::pair().unwrap();
        feed.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        reader.state = ReaderState::Active {
            pump,
            done: Arc::new(AtomicBool::new(false)),
        };
        let mut buf = [0; 128];
        assert_eq!(
            reader.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let mut frame = [0; 14];
        feed.read_exact(&mut frame).unwrap();
        assert_eq!(&frame, b"protocol frame");
    }

    #[test]
    fn subslice_finder_basics() {
        assert_eq!(find_subslice(b"abc**\x18B0def", ZMODEM_TRIGGER), Some(3));
        assert_eq!(find_subslice(b"**\x18B0", ZMODEM_TRIGGER), Some(0));
        assert_eq!(find_subslice(b"nothing here", ZMODEM_TRIGGER), None);
        assert_eq!(find_subslice(b"nothing here", trzsz::TRZSZ_TRIGGER), None);
        assert_eq!(find_subslice(b"xy**", ZMODEM_TRIGGER), None);
        assert_eq!(find_subslice(b"", ZMODEM_TRIGGER), None);
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
