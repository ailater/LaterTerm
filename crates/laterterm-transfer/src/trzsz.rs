//! trzsz 原生协议（trz/tsz，M3）：与 ZMODEM 同骨架的另一触发分支。
//!
//! 逆向口径（trzsz 官方源码 trzsz-libs/utils.py + transfer.py）：
//!
//! - **触发**：远端 `trz`/`tsz` 打印
//!   `\x1b7::TRZSZ:TRANSFER:<S|R|D>:<版本>:<13位ID>:<端口>\r\n`；
//!   本端只接 `S`（远端推文件），`R`（远端要上传）为后续
//! - **帧**：`#<类型>:<载荷>\n` 行帧；字符串/二进制载荷 = base64(zlib(x))
//! - **流程**（接收）：客户发 `#ACT:{json}` → 收 `#CFG:{json}` → 发
//!   `#CFG:{json}` → 收 `#NUM:n` → 每文件 `#NAME` → `#SIZE` → N×`#DATA`
//!   → `#MD5` → 结束 `#EXIT`；除 ACT/CFG/EXIT 外每步回 `#SUCC:<回显>`
//!   （整数回显数值、字节回显原编码串），DATA 的 SUCC 回**解码长度**
//! - **中断**：流中出现 0x03 即中止
//! - MD5 全文校验失败 = 传输失败（不静默收下坏文件）
//!
//! 与 ZMODEM 分支共用：触发扫描分流、UnixStream 管道喂工作线程、
//! SharedWriter 回写远端、download_dir 落盘与事件口。

use crate::{SharedWriter, TransferEvent, TransferOptions};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 触发串（`S` = 远端推送；前缀不含 `\x1b7`，兼容不回显 ESC7 的环境）。
pub const TRZSZ_TRIGGER: &[u8] = b"::TRZSZ:TRANSFER:S:";

/// 载荷编码：base64(zlib(data))；解码为其逆。
fn encode_payload(data: &[u8]) -> String {
    use base64::Engine as _;
    let zipped = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let mut encoder = zipped;
    let _ = encoder.write_all(data);
    let zipped = encoder.finish().unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(zipped)
}

fn decode_payload(encoded: &str) -> io::Result<Vec<u8>> {
    use base64::Engine as _;
    let zipped = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| io::Error::other(format!("base64 解码失败: {e}")))?;
    let mut decoder = flate2::read::ZlibDecoder::new(&zipped[..]);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| io::Error::other(format!("zlib 解压失败: {e}")))?;
    Ok(out)
}

/// 一行协议帧。
struct Frame {
    typ: String,
    payload: String,
}

/// 从管道读一行（`\n` 分隔，容忍 `\r`；裸 0x03 = 中止）。
fn recv_frame(reader: &mut UnixStream) -> io::Result<Frame> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte)?;
        if n == 0 {
            return Err(io::Error::other("trzsz: 连接在帧中途中断"));
        }
        match byte[0] {
            b'\n' => break,
            0x03 => return Err(io::Error::other("trzsz: 被中断（ctrl+c）")),
            b'\r' => continue,
            other => line.push(other),
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

    // ① ACT：客户端能力声明（lang=rs 标识实现方）
    let action = serde_json::json!({
        "lang": "rs",
        "confirm": true,
        "version": env!("CARGO_PKG_VERSION"),
        "support_dir": true,
        "protocol": 1,
    });
    send_line(&reply, "ACT", &action.to_string())?;

    // ② 收服务端 CFG（确认/超时/分块尺寸等；MVP 只需兼容性应答）
    let frame = recv_frame(&mut feed)?;
    if frame.typ != "CFG" {
        return Err(io::Error::other(format!("期望 CFG，收到 {}", frame.typ)));
    }
    // ③ 回客户端 CFG（目录传输开——NAME 可能是 json 对象）
    let config = serde_json::json!({
        "lang": "rs",
        "quiet": true,
        "binary": true,
        "escape_chars": false,
        "directory": true,
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
    let (name, is_dir) = if text.trim_start().starts_with('{') {
        let object: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| io::Error::other(format!("NAME json 解析失败: {e}")))?;
        let name = object
            .get("path_name")
            .and_then(|list| list.as_array())
            .and_then(|parts| parts.last())
            .and_then(|part| part.as_str())
            .ok_or_else(|| io::Error::other("NAME json 缺 path_name"))?
            .to_string();
        let is_dir = object
            .get("is_dir")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        (name, is_dir)
    } else {
        (text, false)
    };
    // 路径剥离：只按文件名落盘（与 ZMODEM junk_path 同安全口径）
    let safe_name = std::path::Path::new(&name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
        return Err(io::Error::other(format!("非法文件名: {name}")));
    }
    let target = options.download_dir.join(&safe_name);

    if is_dir {
        std::fs::create_dir_all(&target)?;
        return Ok(None);
    }

    // SIZE
    let frame = recv_frame(feed)?;
    let size: u64 = frame
        .payload
        .trim()
        .parse()
        .map_err(|e| io::Error::other(format!("SIZE 解析失败: {e}")))?;
    send_line(reply, "SUCC", frame.payload.trim())?;

    // DATA × n（每块回解码长度）+ 全文 MD5
    let mut file = std::fs::File::create(&target)?;
    let mut hasher = md5::Context::new();
    let mut received: u64 = 0;
    while received < size {
        let frame = recv_frame(feed)?;
        if frame.typ != "DATA" {
            return Err(io::Error::other(format!("期望 DATA，收到 {}", frame.typ)));
        }
        let chunk = decode_payload(&frame.payload)?;
        let _ = hasher.write(&chunk);
        file.write_all(&chunk)?;
        received += chunk.len() as u64;
        send_line(reply, "SUCC", &chunk.len().to_string())?;
    }
    file.flush()?;

    // MD5 校验：不符即失败（不静默收坏文件）
    let frame = recv_frame(feed)?;
    let expect = decode_payload(&frame.payload)?;
    let digest = hasher.finalize().0;
    if expect.as_slice() != digest {
        let _ = std::fs::remove_file(&target);
        return Err(io::Error::other("MD5 校验失败，文件已丢弃"));
    }
    send_line(reply, "SUCC", &frame.payload)?;

    Ok(Some(target))
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

    /// 协议回放：伪造远端 trz（ACT→CFG→NUM→单文件→EXIT）逐帧喂入，
    /// 断言文件落盘、SUCC 序列、EXIT 应答——离线全链路。
    #[test]
    fn trzsz_protocol_replay_receive() {
        let dir = tempfile::tempdir().expect("临时目录");
        let download = dir.path().join("dl");
        std::fs::create_dir_all(&download).expect("建目录");

        let content = b"trzsz payload bytes 0123456789".repeat(64);
        let (events_tx, events_rx) = std::sync::mpsc::channel::<TransferEvent>();
        let events_tx = std::sync::mpsc::Sender::clone(&events_tx);
        let options = TransferOptions {
            download_dir: download.clone(),
            events: std::sync::mpsc::Sender::clone(&events_tx),
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

        // 断言事件与落盘
        worker.join().expect("工作线程");
        let event = events_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("事件");
        let TransferEvent::Finished(paths) = event else {
            panic!("期望 Finished: {event:?}")
        };
        assert_eq!(paths, vec![download.join("回放文件.txt")]);
        assert_eq!(
            std::fs::read(&paths[0]).unwrap(),
            content.to_vec(),
            "字节级一致"
        );
    }
}
