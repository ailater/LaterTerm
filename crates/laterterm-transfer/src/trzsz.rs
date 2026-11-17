//! trzsz 原生协议（trz/tsz，M3）：与 ZMODEM 同骨架的另一触发分支。
//!
//! 协议口径（对照官方实现核对：trzsz-go comm.go/transfer.go + trzsz
//! py_utils.py/py_transfer.py/py_recv.py/py_send.py）：
//!
//! - **触发**：远端 `trz`/`tsz` 打印
//!   `\x1b7::TRZSZ:TRANSFER:<S|R|D>:<版本>:<13位ID>:<端口>\r\n`；
//!   本端接 `S`（远端推文件）与 `R`（远端要上传，R 方向 v1 无文件选择 UI，
//!   优雅拒绝）
//! - **帧**：`#<类型>:<载荷>\n` 行帧；protocol-1 字符串模式下字符串/二进制
//!   载荷 = base64(zlib(x))，整数载荷 = 十进制裸文本
//! - **流程**（接收）：客户发 `#ACT:{json}` → 收 `#CFG:{json}` → 发
//!   `#CFG:{json}` → 收 `#NUM:n` → 每文件 `#NAME` → `#SIZE` → N×`#DATA`
//!   → `#MD5` → 结束 `#EXIT`；除 ACT/CFG/EXIT 外每步回 `#SUCC:<回显>`
//!   （整数回显数值、字节回显原编码串），DATA 的 SUCC 回**解码长度**
//! - **binary 声明禁用**：CFG 若声明 `"binary":true`，官方语义 = DATA 走
//!   `#DATA:<len>\n<原始转义字节>`（transfer.go sendData 分流），本端行帧
//!   解析器不兼容，且 GBK 会话的二进制字节会被转码破坏——固定 protocol-1
//!   纯 ASCII 字符串编码，不声明 binary
//! - **超时**：读超时 = 报错终止（无 NAK 无重传），初值 20s（官方默认），
//!   收到服务端 CFG 后按其 timeout 字段刷新
//! - **资源上限三道闸**：① 帧行长 ≤16MiB（base64 侧）；② 载荷解压输出
//!   ≤12MiB（MaxBufSize=10MiB+余量，zlib 可极端膨胀，不解压上限 = 单帧
//!   压缩炸弹即可 OOM）；③ 单文件 SIZE/累计接收量 ≤4GiB——三者共同保证
//!   恶意/乱流远端不能把终端打进无界内存或磁盘
//! - **中断**：流中出现 0x03 即中止
//! - MD5 全文校验失败 = 传输失败（不静默收下坏文件）
//!
//! 与 ZMODEM 分支共用：触发扫描分流、UnixStream 管道喂工作线程、
//! SharedWriter 回写远端、download_dir 落盘与事件口。

use crate::{SharedWriter, TransferDirection, TransferEvent, TransferFlavor, TransferOptions};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 读超时初值：远端中途断流不能让 worker 永久阻塞（官方默认 20s，
/// transfer.go newTransfer；收到服务端 CFG 后按其 timeout 字段刷新）。
const DEFAULT_TIMEOUT_SECS: u64 = 20;

/// 单帧行长上限：官方单块上限 MaxBufSize=10MiB（transfer.go newTransfer），
/// 帧载荷为 base64(zlib(块))，按 4/3 膨胀折算取 16MiB——既封死误触发后
/// 无换行输出灌入的无界内存增长，又不误杀合法大块帧。
const FRAME_LINE_LIMIT: usize = 16 * 1024 * 1024;

/// 单文件 SIZE 与累计接收量上限（4GiB）：防御远端谎报超大 SIZE 或洪流
/// 灌爆磁盘；超限发 ERRO 并失败（仅接收方向；上传方向上限随流式分块读
/// 后续批次一并补，见方案采纳④）。
const MAX_TRANSFER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// 进度节流（worker 内）：首帧必发、距上次 ≥100ms 才发；终值由 Finished
/// 事件兜底，节流吞尾帧无碍。间隔对齐 sftp_panel::ProgressThrottle 先例。
struct ProgressThrottle {
    interval: Duration,
    last: Option<std::time::Instant>,
}

impl ProgressThrottle {
    fn new() -> Self {
        Self {
            interval: Duration::from_millis(100),
            last: None,
        }
    }

    /// 判定并记录：返回 true 时调用方才发送进度事件。
    fn should_emit(&mut self) -> bool {
        let now = std::time::Instant::now();
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < self.interval)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// 触发串（`S` = 远端推送；前缀不含 `\x1b7`，兼容不回显 ESC7 的环境）。
pub const TRZSZ_TRIGGER: &[u8] = b"::TRZSZ:TRANSFER:S:";

/// 触发串（`R` = 远端要收文件，本端上传）。
pub const TRZSZ_R_TRIGGER: &[u8] = b"::TRZSZ:TRANSFER:R:";

/// 载荷编码：base64(zlib(data))；解码为其逆。
fn encode_payload(data: &[u8]) -> String {
    use base64::Engine as _;
    let zipped = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let mut encoder = zipped;
    let _ = encoder.write_all(data);
    let zipped = encoder.finish().unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(zipped)
}

/// 单帧载荷解压输出上限：官方单块上限 MaxBufSize=10MiB（transfer.go
/// newTransfer）加 2MiB 余量。zlib 解压可极端膨胀（实测 1GiB 全零 →
/// ~1.36MiB base64，16MiB 行长预算 ⇒ 可解出 ~12GiB 明文），解码处不设
/// 上限 = 恶意/乱流远端单帧即可 OOM 终端并绕过 4GiB 上限红线。
const PAYLOAD_DECODE_LIMIT: usize = 12 * 1024 * 1024;

fn decode_payload(encoded: &str) -> io::Result<Vec<u8>> {
    use base64::Engine as _;
    let zipped = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| io::Error::other(format!("base64 解码失败: {e}")))?;
    // take(上限+1) 读到即可判定超限，绝不做无界解压；+1 是为了区分
    // 「恰好等于上限的合法载荷」与「超限」（take 恰好放行 上限+1 字节）。
    let mut decoder =
        flate2::read::ZlibDecoder::new(&zipped[..]).take((PAYLOAD_DECODE_LIMIT + 1) as u64);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| io::Error::other(format!("zlib 解压失败: {e}")))?;
    if out.len() > PAYLOAD_DECODE_LIMIT {
        return Err(io::Error::other(format!(
            "trzsz: 载荷解压超限（>{PAYLOAD_DECODE_LIMIT} 字节），疑似压缩炸弹"
        )));
    }
    Ok(out)
}

/// 一行协议帧。
#[derive(Debug)]
struct Frame {
    typ: String,
    payload: String,
}

/// 从管道读一行（`\n` 分隔，容忍 `\r`；裸 0x03 = 中止）。
///
/// 读阻塞受 feed 的 `set_read_timeout` 约束（见 [`DEFAULT_TIMEOUT_SECS`]）；
/// 行长受 [`FRAME_LINE_LIMIT`] 约束——两者共同封死「误触发后远端无换行
/// 输出洪流」导致的 worker 永久阻塞 / 内存无界。
fn recv_frame(reader: &mut UnixStream) -> io::Result<Frame> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                io::Error::other("trzsz: 读超时（远端无响应，按协议报错终止）")
            } else {
                error
            }
        })?;
        if n == 0 {
            return Err(io::Error::other("trzsz: 连接在帧中途中断"));
        }
        match byte[0] {
            b'\n' => break,
            0x03 => return Err(io::Error::other("trzsz: 被中断（ctrl+c）")),
            b'\r' => continue,
            other => {
                line.push(other);
                if line.len() > FRAME_LINE_LIMIT {
                    return Err(io::Error::other(
                        "trzsz: 帧行长超限（>16MiB），疑似非协议数据洪流",
                    ));
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&line).into_owned();
    let (typ, payload) = text
        .strip_prefix('#')
        .and_then(|rest| rest.split_once(':'))
        .ok_or_else(|| io::Error::other(format!("trzsz: 帧格式不符: {text:.60}")))?;
    Ok(Frame {
        typ: typ.to_string(),
        payload: payload.to_string(),
    })
}

/// 工作线程主体：完整接收流程。返回收到的文件路径。
pub(crate) fn run_trzsz_receive(
    options: &TransferOptions,
    reply: SharedWriter,
    feed: UnixStream,
    done: Arc<AtomicBool>,
) {
    let result = trzsz_receive_loop(options, reply, feed);
    let event = match result {
        Ok(paths) => TransferEvent::Finished(paths),
        Err(error) => TransferEvent::Failed(format!("trzsz: {error}")),
    };
    let _ = options.events.send(event);
    done.store(true, Ordering::Release);
}

fn send_line(reply: &SharedWriter, typ: &str, payload: &str) -> io::Result<()> {
    let mut writer = reply.clone();
    writer.write_all(format!("#{typ}:{payload}\n").as_bytes())?;
    writer.flush()
}

fn trzsz_receive_loop(
    options: &TransferOptions,
    reply: SharedWriter,
    mut feed: UnixStream,
) -> io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(&options.download_dir)?;
    // 读超时兜底（默认 20s）：远端中途断流/静默时 worker 报错退出，不永久泄漏
    feed.set_read_timeout(Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS)))?;

    // ① ACT：客户端能力声明（lang=rs 标识实现方）
    let action = serde_json::json!({
        "lang": "rs",
        "confirm": true,
        "version": env!("CARGO_PKG_VERSION"),
        "support_dir": false,
        "protocol": 1,
    });
    send_line(&reply, "ACT", &action.to_string())?;

    // ② 收服务端 CFG（确认/超时/分块尺寸等；MVP 只需兼容性应答）
    let frame = recv_frame(&mut feed)?;
    if frame.typ != "CFG" {
        return Err(io::Error::other(format!("期望 CFG，收到 {}", frame.typ)));
    }
    // 按服务端 CFG 的 timeout 刷新读超时（官方口径：超时 = 报错终止，无重传；
    // 字段缺失/非法保持默认 20s）
    if let Ok(server_cfg) = serde_json::from_str::<serde_json::Value>(frame.payload.trim())
        && let Some(timeout) = server_cfg.get("timeout").and_then(|value| value.as_u64())
    {
        feed.set_read_timeout(Some(Duration::from_secs(timeout.max(1))))?;
    }
    // ③ 暂不支持安全目录传输，明确关闭目录能力。不声明 binary：官方语义下
    //    binary=true 的 DATA 走「长度行 + 原始转义字节」非行帧（见模块头），
    //    protocol-1 纯 ASCII 字符串编码是唯一与本端解析器兼容的路径。
    let config = serde_json::json!({
        "lang": "rs",
        "quiet": true,
        "escape_chars": false,
        "directory": false,
        "bufsize": 131_072,
        "timeout": 60,
        "overwrite": false,
        "tmux_output_junk": true,
        "tmux_pane_width": 120,
        "protocol": 1,
    });
    send_line(&reply, "CFG", &config.to_string())?;

    // ④ 文件数
    let frame = recv_frame(&mut feed)?;
    let count: usize = frame
        .payload
        .trim()
        .parse()
        .map_err(|e| io::Error::other(format!("NUM 解析失败: {e}")))?;
    send_line(&reply, "SUCC", frame.payload.trim())?;

    let mut saved = Vec::new();
    for _ in 0..count {
        saved.push(recv_one_file(options, &reply, &mut feed)?);
    }

    // ⑮ EXIT 双向道别
    let frame = recv_frame(&mut feed)?;
    if frame.typ == "EXIT" {
        send_line(&reply, "EXIT", &encode_payload(b"Done"))?;
    }
    Ok(saved.into_iter().flatten().collect())
}

/// 收单个条目：NAME（字符串或 json 对象）→ 目录直建 / 文件走 SIZE→DATA×n→MD5。
fn recv_one_file(
    options: &TransferOptions,
    reply: &SharedWriter,
    feed: &mut UnixStream,
) -> io::Result<Option<PathBuf>> {
    let frame = recv_frame(feed)?;
    if frame.typ != "NAME" {
        return Err(io::Error::other(format!("期望 NAME，收到 {}", frame.typ)));
    }
    // SUCC 应答 = 原样回显 NAME 载荷
    send_line(reply, "SUCC", &frame.payload)?;

    // NAME 载荷：base64(zlib(utf8 名字)) 或 json 对象（目录模式）
    let decoded = decode_payload(&frame.payload)?;
    let text = String::from_utf8_lossy(&decoded).into_owned();
    if text.trim_start().starts_with('{') {
        return Err(io::Error::other("暂不支持 trzsz 目录传输"));
    }
    let name = text;
    // 路径剥离：只按文件名落盘（与 ZMODEM junk_path 同安全口径）
    let safe_name = std::path::Path::new(&name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
        return Err(io::Error::other(format!("非法文件名: {name}")));
    }
    let target = options.download_dir.join(&safe_name);

    // SIZE
    let frame = recv_frame(feed)?;
    let size: u64 = frame
        .payload
        .trim()
        .parse()
        .map_err(|e| io::Error::other(format!("SIZE 解析失败: {e}")))?;
    // SIZE 上限（4GiB）：远端谎报超大文件时拒绝并回 ERRO，不进入接收循环
    if size > MAX_TRANSFER_BYTES {
        let message = format!("文件超过 4GiB 上限（SIZE={size}），已拒绝");
        let _ = send_line(reply, "ERRO", &encode_payload(message.as_bytes()));
        return Err(io::Error::other(message));
    }
    send_line(reply, "SUCC", frame.payload.trim())?;

    // DATA × n（每块回解码长度）+ 全文 MD5
    let mut file = tempfile::NamedTempFile::new_in(&options.download_dir)?;
    let mut hasher = md5::Context::new();
    let mut received: u64 = 0;
    let mut throttle = ProgressThrottle::new();
    while received < size {
        let frame = recv_frame(feed)?;
        if frame.typ != "DATA" {
            return Err(io::Error::other(format!("期望 DATA，收到 {}", frame.typ)));
        }
        let chunk = decode_payload(&frame.payload)?;
        let _ = hasher.write(&chunk);
        file.write_all(&chunk)?;
        received += chunk.len() as u64;
        // 累计接收量上限（4GiB）：远端拆块绕过 SIZE 校验的第二道闸
        if received > MAX_TRANSFER_BYTES {
            let message = format!("接收量超过 4GiB 上限（SIZE={size}），已中止");
            let _ = send_line(reply, "ERRO", &encode_payload(message.as_bytes()));
            return Err(io::Error::other(message));
        }
        // 进度事件（节流发射）：驱动 UI 浮条（trzsz 专属；ZMODEM 无回调不发）
        if throttle.should_emit() {
            let _ = options.events.send(TransferEvent::Progress {
                flavor: TransferFlavor::Trzsz,
                direction: TransferDirection::Receive,
                file: safe_name.clone(),
                transferred: received,
                total: size,
            });
        }
        send_line(reply, "SUCC", &chunk.len().to_string())?;
    }
    file.flush()?;

    // MD5 校验：不符即失败（不静默收坏文件）
    let frame = recv_frame(feed)?;
    let expect = decode_payload(&frame.payload)?;
    let digest = hasher.finalize().0;
    if expect.as_slice() != digest {
        return Err(io::Error::other("MD5 校验失败，文件已丢弃"));
    }
    file.persist_noclobber(&target)
        .map_err(|error| error.error)?;
    send_line(reply, "SUCC", &frame.payload)?;

    Ok(Some(target))
}

// ───────────────────────── R 方向（上传）─────────────────────────

/// 工作线程主体：完整上传流程（R 触发：本端推文件到远端）。
pub(crate) fn run_trzsz_upload(
    options: &TransferOptions,
    reply: SharedWriter,
    feed: UnixStream,
    done: Arc<AtomicBool>,
) {
    let result = trzsz_upload_loop(options, reply, feed);
    let event = match result {
        Ok(_count) => TransferEvent::Finished(vec![]), // 上传无落盘路径
        Err(error) => TransferEvent::Failed(format!("trzsz 上传: {error}")),
    };
    let _ = options.events.send(event);
    done.store(true, Ordering::Release);
}

/// R 方向上传主循环：ACT→CFG→NUM→逐文件(NAME/SIZE/DATA×n/MD5)→EXIT。
/// 文件列表来自 `TransferOptions.upload_files`；None 则发 ERRO 并退出
/// （远端 trz 会显示错误并恢复终端）。
fn trzsz_upload_loop(
    options: &TransferOptions,
    reply: SharedWriter,
    mut feed: UnixStream,
) -> io::Result<usize> {
    // 读超时兜底（同接收方向）：远端断流/静默时 worker 报错退出，不永久泄漏
    feed.set_read_timeout(Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS)))?;
    // 无可上传文件：优雅拒绝（不让远端挂在等 ACT 的状态）
    let Some(files) = &options.upload_files else {
        send_line(
            &reply,
            "ACT",
            r#"{"lang":"rs","confirm":true,"protocol":1}"#,
        )?;
        // 等 CFG 再回 ERRO（远端 trz 在等 ACT 后进入 CFG 交换）
        let _frame = recv_frame(&mut feed)?;
        send_line(&reply, "ERRO", &encode_payload(b"no files to upload"))?;
        return Err(io::Error::other("无可上传文件（upload_files 未设置）"));
    };
    if files.is_empty() {
        send_line(
            &reply,
            "ACT",
            r#"{"lang":"rs","confirm":true,"protocol":1}"#,
        )?;
        let _frame = recv_frame(&mut feed)?;
        send_line(&reply, "ERRO", &encode_payload(b"file list is empty"))?;
        return Err(io::Error::other("上传文件列表为空"));
    }

    // ① ACT
    let action = serde_json::json!({
        "lang": "rs",
        "confirm": true,
        "version": env!("CARGO_PKG_VERSION"),
        "support_dir": false,
        "protocol": 1,
    });
    send_line(&reply, "ACT", &action.to_string())?;

    // ② 收服务端 CFG
    let frame = recv_frame(&mut feed)?;
    if frame.typ != "CFG" {
        return Err(io::Error::other(format!("期望 CFG，收到 {}", frame.typ)));
    }
    // ③ 回客户端 CFG（上传不开目录——只传文件；不声明 binary，口径同接收方向）
    let config = serde_json::json!({
        "lang": "rs",
        "quiet": true,
        "escape_chars": false,
        "directory": false,
        "bufsize": 131_072,
        "timeout": 60,
        "overwrite": true,
        "tmux_output_junk": true,
        "tmux_pane_width": 120,
        "protocol": 1,
    });
    send_line(&reply, "CFG", &config.to_string())?;

    // ④ 文件数
    send_line(&reply, "NUM", &files.len().to_string())?;
    expect_succ(&mut feed, &files.len().to_string())?;

    // ⑤ 逐文件
    for path in files {
        upload_one_file(options, &reply, &mut feed, path)?;
    }

    // ⑥ EXIT
    send_line(&reply, "EXIT", &encode_payload(b"Done"))?;
    Ok(files.len())
}

/// 上传单个文件：NAME→SIZE→DATA×n→MD5（每步等 SUCC 确认）。
/// 注：整文件读入内存为 v1 已知取舍（R 方向无文件选择 UI，路径不可达）；
/// 点亮上传 UI 时同批改流式分块读并补 4GiB 上限（方案采纳④立约）。
fn upload_one_file(
    options: &TransferOptions,
    reply: &SharedWriter,
    feed: &mut UnixStream,
    path: &std::path::Path,
) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::other(format!("路径无文件名: {}", path.display())))?;
    let data =
        std::fs::read(path).map_err(|e| io::Error::other(format!("读 {}: {e}", path.display())))?;

    // NAME（文件名）
    send_line(reply, "NAME", &encode_payload(name.as_bytes()))?;
    expect_succ(feed, &encode_payload(name.as_bytes()))?;

    // SIZE
    send_line(reply, "SIZE", &data.len().to_string())?;
    expect_succ(feed, &data.len().to_string())?;

    // DATA 分块（64KiB）+ 进度事件（节流）
    let mut throttle = ProgressThrottle::new();
    let total = data.len() as u64;
    for (index, chunk) in data.chunks(64 * 1024).enumerate() {
        send_line(reply, "DATA", &encode_payload(chunk))?;
        expect_succ(feed, &chunk.len().to_string())?;
        if throttle.should_emit() {
            let _ = options.events.send(TransferEvent::Progress {
                flavor: TransferFlavor::Trzsz,
                direction: TransferDirection::Upload,
                file: name.clone(),
                transferred: (((index + 1) * 64 * 1024) as u64).min(total),
                total,
            });
        }
    }

    // MD5
    let digest = {
        let mut ctx = md5::Context::new();
        let _ = ctx.write(&data);
        ctx.finalize()
    };
    send_line(reply, "MD5", &encode_payload(&digest.0))?;
    expect_succ(feed, &encode_payload(&digest.0))?;

    Ok(())
}

/// 收一帧并断言 SUCC 载荷与期望一致。
fn expect_succ(feed: &mut UnixStream, expect: &str) -> io::Result<()> {
    let frame = recv_frame(feed)?;
    if frame.typ == "SUCC" && frame.payload == expect {
        Ok(())
    } else if frame.typ == "ERRO" {
        let decoded = decode_payload(&frame.payload)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|_| frame.payload.clone());
        Err(io::Error::other(format!("远端拒绝: {decoded}")))
    } else {
        Err(io::Error::other(format!(
            "期望 SUCC:{expect}，收到 {}:{}",
            frame.typ, frame.payload
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_codec_roundtrip() {
        let data = b"hello trzsz \xe4\xb8\xad\xe6\x96\x87 binary \x00\x01\x02";
        let encoded = encode_payload(data);
        assert!(!encoded.contains('\n'), "行帧载荷不得含换行");
        assert_eq!(decode_payload(&encoded).unwrap(), data.to_vec());
    }

    #[test]
    fn receive_rejects_existing_file_without_changing_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("existing.txt");
        std::fs::write(&target, "原始内容".as_bytes()).unwrap();
        let (events, _) = flume::unbounded();
        let options = TransferOptions {
            download_dir: dir.path().to_path_buf(),
            events,
            upload_files: None,
        };
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(io::sink()) as Box<dyn Write + Send>
            )),
        };
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        write!(
            sender,
            "#NAME:{}\n#SIZE:3\n#DATA:{}\n#MD5:{}\n",
            encode_payload(b"existing.txt"),
            encode_payload(b"new"),
            encode_payload(&md5::compute(b"new").0)
        )
        .unwrap();
        assert!(recv_one_file(&options, &reply, &mut receiver).is_err());
        assert_eq!(std::fs::read(target).unwrap(), "原始内容".as_bytes());
    }

    #[test]
    fn interrupted_receive_leaves_no_partial_target() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _) = flume::unbounded();
        let options = TransferOptions {
            download_dir: dir.path().to_path_buf(),
            events,
            upload_files: None,
        };
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(io::sink()) as Box<dyn Write + Send>
            )),
        };
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        write!(
            sender,
            "#NAME:{}\n#SIZE:5\n#DATA:{}\n",
            encode_payload(b"partial.txt"),
            encode_payload(b"abc")
        )
        .unwrap();
        drop(sender);
        assert!(recv_one_file(&options, &reply, &mut receiver).is_err());
        assert!(!dir.path().join("partial.txt").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn directory_entries_are_rejected_instead_of_flattened() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _) = flume::unbounded();
        let options = TransferOptions {
            download_dir: dir.path().to_path_buf(),
            events,
            upload_files: None,
        };
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(io::sink()) as Box<dyn Write + Send>
            )),
        };
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        let name =
            serde_json::json!({"path_name": ["parent", "child"], "is_dir": true}).to_string();
        writeln!(sender, "#NAME:{}", encode_payload(name.as_bytes())).unwrap();
        assert!(recv_one_file(&options, &reply, &mut receiver).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// 协议回放：伪造远端 trz（ACT→CFG→NUM→单文件→EXIT）逐帧喂入，
    /// 断言文件落盘、SUCC 序列、EXIT 应答——离线全链路。
    #[test]
    fn trzsz_protocol_replay_receive() {
        let dir = tempfile::tempdir().expect("临时目录");
        let download = dir.path().join("dl");
        std::fs::create_dir_all(&download).expect("建目录");

        let content = b"trzsz payload bytes 0123456789".repeat(64);
        let (events_tx, events_rx) = flume::unbounded::<TransferEvent>();
        let events_tx = events_tx.clone();
        let options = TransferOptions {
            download_dir: download.clone(),
            events: events_tx.clone(),
            upload_files: None,
        };

        // 管道：伪远端帧 → 工作线程
        let (mut pump, feed) = UnixStream::pair().expect("pair");
        // 应答端：SharedWriter → 缓冲
        let (reply_w, mut reply_r) = UnixStream::pair().expect("pair pair");
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(reply_w) as Box<dyn Write + Send>
            )),
        };

        let done = Arc::new(AtomicBool::new(false));
        let worker = {
            let options = TransferOptions {
                download_dir: options.download_dir.clone(),
                events: events_tx.clone(),
                upload_files: None,
            };
            let reply = reply.clone();
            let done = done.clone();
            std::thread::Builder::new()
                .name("test-trzsz".into())
                .spawn(move || run_trzsz_receive(&options, reply, feed, done))
                .expect("起线程")
        };

        // —— 伪远端逐步推进 ——
        let step = |pump: &mut UnixStream, reply_r: &mut UnixStream, typ: &str, payload: &str| {
            pump.write_all(format!("#{typ}:{payload}\n").as_bytes())
                .unwrap();
            pump.flush().unwrap();
            // 读应答行
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                assert_eq!(reply_r.read(&mut byte).unwrap(), 1, "等应答 EOF");
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            String::from_utf8(line).unwrap()
        };

        // ① 应收到 ACT（json 含 lang=rs）
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            assert_eq!(reply_r.read(&mut byte).unwrap(), 1);
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        let act = String::from_utf8(line).unwrap();
        assert!(
            act.starts_with("#ACT:") && act.contains("\"lang\":\"rs\""),
            "{act}"
        );

        // ② CFG → ③ 期望客户端 CFG
        let cfg = step(&mut pump, &mut reply_r, "CFG", r#"{"escape_chars":false}"#);
        assert!(cfg.starts_with("#CFG:"), "{cfg}");
        // 钉死 binary 修复：CFG 绝不声明 binary——官方语义下 binary=true 的
        // DATA 非 base64 行帧，本端解析器不兼容（模块头注记）
        assert!(!cfg.contains("binary"), "CFG 不得声明 binary: {cfg}");

        // ④ NUM=1 → SUCC:1
        let succ = step(&mut pump, &mut reply_r, "NUM", "1");
        assert_eq!(succ, "#SUCC:1");

        // NAME → SUCC 回显
        let name_payload = encode_payload("回放文件.txt".as_bytes());
        let succ = step(&mut pump, &mut reply_r, "NAME", &name_payload);
        assert_eq!(succ, format!("#SUCC:{name_payload}"));

        // SIZE → SUCC
        let succ = step(&mut pump, &mut reply_r, "SIZE", &content.len().to_string());
        assert_eq!(succ, format!("#SUCC:{}", content.len()));

        // DATA 两块 → SUCC 各回解码长度
        let (a, b) = content.split_at(content.len() / 2);
        let succ = step(&mut pump, &mut reply_r, "DATA", &encode_payload(a));
        assert_eq!(succ, format!("#SUCC:{}", a.len()));
        let succ = step(&mut pump, &mut reply_r, "DATA", &encode_payload(b));
        assert_eq!(succ, format!("#SUCC:{}", b.len()));

        // MD5 → SUCC 回显
        let digest = {
            let mut ctx = md5::Context::new();
            let _ = ctx.write(&content);
            ctx.finalize()
        };
        let succ = step(&mut pump, &mut reply_r, "MD5", &encode_payload(&digest.0));
        assert_eq!(succ, format!("#SUCC:{}", encode_payload(&digest.0)));

        // EXIT → 期望 EXIT 应答
        let exit = step(&mut pump, &mut reply_r, "EXIT", &encode_payload(b"bye"));
        assert!(exit.starts_with("#EXIT:"), "{exit}");

        // 断言事件与落盘（DATA 块会先发节流 Progress 事件：排空至 Finished）
        worker.join().expect("工作线程");
        let mut paths = None;
        for _ in 0..4 {
            let event = events_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("事件");
            match event {
                TransferEvent::Finished(finished) => {
                    paths = Some(finished);
                    break;
                }
                TransferEvent::Progress {
                    flavor,
                    direction,
                    file,
                    transferred,
                    total,
                } => {
                    assert_eq!(flavor, TransferFlavor::Trzsz);
                    assert_eq!(direction, TransferDirection::Receive);
                    assert_eq!(file, "回放文件.txt");
                    assert!(transferred <= total && total == content.len() as u64);
                }
                other => panic!("期望 Progress/Finished: {other:?}"),
            }
        }
        let paths = paths.expect("期望 Finished 事件");
        assert_eq!(paths, vec![download.join("回放文件.txt")]);
        assert_eq!(
            std::fs::read(&paths[0]).unwrap(),
            content.to_vec(),
            "字节级一致"
        );
    }

    /// 协议回放（上传）：伪造远端 tsz 收文件（R 触发），本端推送一个文件，
    /// 断言协议交互序列（ACT→CFG→NUM→NAME/SIZE/DATA/MD5→EXIT）。
    #[test]
    fn trzsz_protocol_replay_upload() {
        let dir = tempfile::tempdir().expect("临时目录");
        let content = b"upload payload test 0123456789abcdef".repeat(32);
        let upload_file = dir.path().join("上传测试.txt");
        std::fs::write(&upload_file, &content).expect("写源文件");

        let (events_tx, events_rx) = flume::unbounded::<TransferEvent>();
        let options = TransferOptions {
            download_dir: dir.path().join("dl"),
            events: events_tx,
            upload_files: Some(vec![upload_file.clone()]),
        };

        let (mut pump, feed) = UnixStream::pair().expect("pair");
        let (reply_w, mut reply_r) = UnixStream::pair().expect("reply pair");
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(reply_w) as Box<dyn Write + Send>
            )),
        };
        let done = Arc::new(AtomicBool::new(false));
        let worker = {
            let options = TransferOptions {
                download_dir: options.download_dir.clone(),
                events: options.events.clone(),
                upload_files: options.upload_files.clone(),
            };
            let reply = reply.clone();
            let done = done.clone();
            std::thread::Builder::new()
                .name("test-trzsz-up".into())
                .spawn(move || run_trzsz_upload(&options, reply, feed, done))
                .expect("起线程")
        };

        // 伪远端逐步应答
        let read_line = |reply_r: &mut UnixStream| -> String {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                assert_eq!(reply_r.read(&mut byte).unwrap(), 1);
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            String::from_utf8(line).unwrap()
        };
        let succ = |pump: &mut UnixStream, reply_r: &mut UnixStream, expect: &str| {
            let line = read_line(reply_r);
            assert!(line.starts_with('#'), "帧格式: {line}");
            let (typ, payload) = line[1..].split_once(':').expect("帧格式");
            pump.write_all(format!("#SUCC:{payload}\n").as_bytes())
                .unwrap();
            pump.flush().unwrap();
            assert_eq!(typ, expect, "帧类型");
        };

        // ① 收 ACT → 回 CFG
        let act = read_line(&mut reply_r);
        assert!(act.starts_with("#ACT:") && act.contains("lang"), "{act}");
        pump.write_all(b"#CFG:{\"binary\":true}\n").unwrap();
        pump.flush().unwrap();

        // ② 收客户端 CFG
        let cfg = read_line(&mut reply_r);
        assert!(cfg.starts_with("#CFG:"), "{cfg}");
        // 钉死 binary 修复（口径同接收方向回放）
        assert!(!cfg.contains("binary"), "CFG 不得声明 binary: {cfg}");

        // ③ NUM=1 → SUCC
        let num = read_line(&mut reply_r);
        assert!(num.starts_with("#NUM:1"), "{num}");
        pump.write_all(b"#SUCC:1\n").unwrap();
        pump.flush().unwrap();

        // ④ NAME → SUCC 回显
        succ(&mut pump, &mut reply_r, "NAME");
        // ⑤ SIZE → SUCC
        succ(&mut pump, &mut reply_r, "SIZE");
        // ⑥ DATA（≤64KiB 一块）→ SUCC 回**解码长度**（非编码载荷回显！）
        {
            let line = read_line(&mut reply_r);
            let (typ, payload) = line[1..].split_once(':').expect("帧格式");
            assert_eq!(typ, "DATA");
            let decoded = decode_payload(payload).expect("解码 DATA 载荷");
            pump.write_all(format!("#SUCC:{}\n", decoded.len()).as_bytes())
                .unwrap();
            pump.flush().unwrap();
        }
        // ⑦ MD5 → SUCC
        succ(&mut pump, &mut reply_r, "MD5");
        // ⑧ EXIT
        let exit = read_line(&mut reply_r);
        assert!(exit.starts_with("#EXIT:"), "{exit}");

        worker.join().expect("工作线程");
        // DATA 块会先发节流 Progress 事件：排空至 Finished
        let mut paths = None;
        for _ in 0..4 {
            let event = events_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("事件");
            if let TransferEvent::Finished(finished) = event {
                paths = Some(finished);
                break;
            }
            assert!(
                matches!(event, TransferEvent::Progress { .. }),
                "期望 Progress/Finished: {event:?}"
            );
        }
        let paths = paths.expect("期望 Finished 事件");
        assert!(paths.is_empty(), "上传完成无落盘路径: {paths:?}");
    }

    /// R 触发 + 无文件列表 → 优雅拒绝（ERRO 帧，远端恢复终端）。
    #[test]
    fn trzsz_upload_no_files_graceful_reject() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (events_tx, events_rx) = flume::unbounded::<TransferEvent>();
        let options = TransferOptions {
            download_dir: dir.path().join("dl"),
            events: events_tx,
            upload_files: None,
        };

        let (mut pump, feed) = UnixStream::pair().expect("pair");
        // 预写一帧 CFG：无文件路径先发 ACT 再等 CFG，管道里得有数据才不挂
        pump.write_all(b"#CFG:{\"binary\":true}\n")
            .expect("预写 CFG");
        pump.flush().expect("flush");
        drop(pump); // 关写端让 recv_frame 在读完缓冲后能感知 EOF

        let (reply_w, _reply_r) = UnixStream::pair().expect("reply");
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(reply_w) as Box<dyn Write + Send>
            )),
        };
        let done = Arc::new(AtomicBool::new(false));
        run_trzsz_upload(&options, reply, feed, done);

        let event = events_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("事件");
        assert!(
            matches!(&event, TransferEvent::Failed(msg) if msg.contains("无可上传")),
            "拒绝事件: {event:?}"
        );
    }

    /// 读超时注入：服务端 CFG 声明 timeout:1 后静默（保持连接不 EOF），
    /// worker 应在约 1s 后报「读超时」并退出，而不是永久阻塞泄漏线程。
    #[test]
    fn silent_remote_times_out_transfer() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (events_tx, events_rx) = flume::unbounded::<TransferEvent>();
        let options = TransferOptions {
            download_dir: dir.path().join("dl"),
            events: events_tx,
            upload_files: None,
        };
        let (mut pump, feed) = UnixStream::pair().expect("pair");
        // CFG 声明 1 秒超时；之后保持写端存活但不再发任何帧
        pump.write_all(b"#CFG:{\"timeout\":1}\n").expect("写 CFG");
        pump.flush().expect("flush");
        // 写端故意不 drop（forget 防 drop 关闭）：断流 ≠ EOF，
        // 静默挂起必须靠 read timeout 兜底
        std::mem::forget(pump);

        let (reply_w, _reply_r) = UnixStream::pair().expect("reply");
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(reply_w) as Box<dyn Write + Send>
            )),
        };
        let done = Arc::new(AtomicBool::new(false));
        let worker = std::thread::Builder::new()
            .name("test-trzsz-timeout".into())
            .spawn({
                let options = TransferOptions {
                    download_dir: options.download_dir.clone(),
                    events: options.events.clone(),
                    upload_files: None,
                };
                move || run_trzsz_receive(&options, reply, feed, done)
            })
            .expect("起线程");

        let event = events_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("超时事件应到达");
        worker.join().expect("worker 应自行退出");
        assert!(
            matches!(&event, TransferEvent::Failed(msg) if msg.contains("读超时")),
            "期望读超时失败事件: {event:?}"
        );
    }

    /// SIZE 超限（>4GiB）：拒绝接收并回 ERRO 帧，远端可见失败原因；
    /// 本端不产生任何落盘文件。
    #[test]
    fn oversize_size_is_rejected_with_erro() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (events, _) = flume::unbounded();
        let options = TransferOptions {
            download_dir: dir.path().to_path_buf(),
            events,
            upload_files: None,
        };
        let (reply_w, mut reply_r) = UnixStream::pair().expect("reply pair");
        let reply = SharedWriter {
            inner: Arc::new(std::sync::Mutex::new(
                Box::new(reply_w) as Box<dyn Write + Send>
            )),
        };
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        write!(
            sender,
            "#NAME:{}\n#SIZE:{}\n",
            encode_payload(b"huge.bin"),
            MAX_TRANSFER_BYTES + 1
        )
        .unwrap();
        let error = recv_one_file(&options, &reply, &mut receiver).unwrap_err();
        assert!(
            error.to_string().contains("4GiB"),
            "错误应说明上限: {error}"
        );
        // 应答序列：NAME 的 SUCC → 超限 ERRO
        let mut responses = Vec::new();
        for _ in 0..2 {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                assert_eq!(reply_r.read(&mut byte).unwrap(), 1, "等应答行");
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            responses.push(String::from_utf8(line).unwrap());
        }
        assert!(responses[0].starts_with("#SUCC:"), "{:?}", responses);
        assert!(
            responses[1].starts_with("#ERRO:"),
            "超限必须回 ERRO 帧: {:?}",
            responses
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "超限不落盘"
        );
    }

    /// 帧行长超限：无换行洪流灌满 16MiB 行缓冲必须报错终止，不无界增长内存。
    #[test]
    fn frame_line_over_limit_is_rejected() {
        let (mut sender, mut receiver) = UnixStream::pair().expect("pair");
        let flood_len = FRAME_LINE_LIMIT + 1;
        let writer = std::thread::Builder::new()
            .name("test-flood".into())
            .spawn(move || {
                // 分块写 16MiB+1 的无换行字节（socket 缓冲有限，须由读端并行消费）
                let chunk = [b'a'; 8192];
                let mut written = 0;
                while written < flood_len {
                    let take = (flood_len - written).min(chunk.len());
                    sender.write_all(&chunk[..take]).expect("写洪流");
                    written += take;
                }
                sender.write_all(b"\n").expect("写行尾");
            })
            .expect("起写线程");
        let error = recv_frame(&mut receiver).unwrap_err();
        assert!(
            error.to_string().contains("行长超限"),
            "错误应说明行长超限: {error}"
        );
        writer.join().expect("写线程");
    }

    /// 解压炸弹拒绝：base64 行受 16MiB 行长上限约束，但 zlib 可极端膨胀
    /// （1GiB 全零 ≈ 1.36MiB base64）——解码处必须限输出，否则恶意/乱流
    /// 远端单帧即可 OOM 终端并绕过 4GiB 红线。
    #[test]
    fn decompress_bomb_payload_is_rejected() {
        // 远超 12MiB 解压上限的全零载荷（线上只有几 KB 的 base64）
        let bomb = vec![0u8; PAYLOAD_DECODE_LIMIT + 1];
        let encoded = encode_payload(&bomb);
        assert!(
            encoded.len() < FRAME_LINE_LIMIT,
            "炸弹帧在线上必须很紧凑，才能穿过行长上限: {}",
            encoded.len()
        );
        let error = decode_payload(&encoded).unwrap_err();
        assert!(
            error.to_string().contains("解压超限"),
            "错误应说明解压超限: {error}"
        );
        // 恰好等于上限的合法载荷不受误杀（take 上限+1 的边界语义）
        let legit = vec![0u8; PAYLOAD_DECODE_LIMIT];
        assert_eq!(
            decode_payload(&encode_payload(&legit)).expect("达上限载荷应完整解码"),
            legit,
            "恰好达上限的载荷必须完整解码"
        );
    }
}
