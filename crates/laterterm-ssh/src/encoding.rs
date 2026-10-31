//! 远程会话编码转换（GBK / GB18030 ↔ UTF-8，M3）。
//!
//! 服务对象：中文遗留服务器（GBK 终端环境）——termview 全程 UTF-8，
//! 转换挂在 [`SessionStreams`](laterterm_core::SessionStreams) 的
//! reader/writer 装饰器上（architecture.md 既定演进位），对视图完全透明。
//!
//! 决策记录：模块放在 laterterm-ssh 而非 laterterm-core——core 保持纯 std
//! 零依赖的纪律，编码转换是远程遗留场景专属需求（本地会话无此问题），
//! encoding_rs（纯 Rust、全表）不该被 core 的所有消费方连带。
//!
//! 语义：
//! - 读方向（GBK → UTF-8）：**lossy**——乱码字节替换为 U+FFFD，
//!   终端显示永不因坏字节卡死；流式状态机跨 `read` 边界保序
//!   （半个汉字跨包到达是常态）
//! - 写方向（UTF-8 → GBK）：合法字符编码下发；**不可映射字符替换为
//!   `?`**（与 iconv `//TRANSLIT` 失败时的传统行为一致）；半个 UTF-8
//!   序列跨 `write` 边界先缓冲，凑齐再编
//! - ASCII 双向恒等穿透

use encoding_rs::{CoderResult, Decoder, Encoder, EncoderResult, Encoding};
use laterterm_core::SessionStreams;
use std::io::{self, Read, Write};

/// 会话字符编码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionEncoding {
    /// UTF-8（默认；现代服务器）。
    #[default]
    Utf8,
    /// GBK（中文遗留服务器最常见）。
    Gbk,
    /// GB18030（GBK 超集；含 GBK 之外字符的遗留环境）。
    Gb18030,
}

impl SessionEncoding {
    fn encoding(self) -> Option<&'static Encoding> {
        match self {
            Self::Utf8 => None,
            Self::Gbk => Some(encoding_rs::GBK),
            Self::Gb18030 => Some(encoding_rs::GB18030),
        }
    }

    /// 给会话字节流装上转码装饰器（UTF-8 会话原样返回）。
    pub fn apply(self, streams: SessionStreams) -> SessionStreams {
        match self.encoding() {
            None => streams,
            Some(encoding) => SessionStreams {
                reader: Box::new(GbkStreamReader::new(streams.reader, encoding)),
                writer: Box::new(GbkStreamWriter::new(streams.writer, encoding)),
            },
        }
    }
}

/// 写端 pending 缓冲的判定结果（先判定后操作，规避借用交叉）。
enum PendingVerdict {
    /// 全部是合法 UTF-8。
    Complete,
    /// 前缀合法 + 一段完整非法序列 + 可能的余量。
    Invalid { valid_up_to: usize, bad: usize },
    /// 前缀合法 + 半个序列（跨 write 边界，等凑齐）。
    Incomplete { valid_up_to: usize },
}

/// 读装饰器：GBK/GB18030 字节流 → UTF-8。
///
/// `pending` 缓冲未消费的源字节（OutputFull/Malformed 处理后的残余与
/// 跨包半个序列），`decoder` 携带跨调用状态。
pub struct GbkStreamReader {
    inner: Box<dyn Read + Send>,
    decoder: Decoder,
    pending: Vec<u8>,
}

impl GbkStreamReader {
    fn new(inner: Box<dyn Read + Send>, encoding: &'static Encoding) -> Self {
        Self {
            inner,
            decoder: encoding.new_decoder(),
            pending: Vec::new(),
        }
    }
}

impl Read for GbkStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // 源字节不足时从内层再拉一批（pending 为空才拉，避免覆盖未消费字节）
            if self.pending.is_empty() {
                let mut chunk = [0u8; 4096];
                let n = self.inner.read(&mut chunk)?;
                if n == 0 {
                    // EOF：last=true 冲刷，尾部残缺序列由 decode_to_utf8
                    // 内置替换为 U+FFFD（lossy 契约），无残余则 written=0
                    let (_, _, written, _) = self.decoder.decode_to_utf8(b"", buf, true);
                    return Ok(written);
                }
                self.pending.extend_from_slice(&chunk[..n]);
            }

            let (result, read, written, _) = self.decoder.decode_to_utf8(&self.pending, buf, false);
            match result {
                CoderResult::InputEmpty => {
                    self.pending.clear();
                    if written > 0 {
                        return Ok(written);
                    }
                    // 本批恰好编完且无输出（理论上罕见）：继续拉下一批
                }
                CoderResult::OutputFull => {
                    // 调用方缓冲写满：保留未消费源字节
                    self.pending.drain(..read);
                    return Ok(written); // written == buf.len()
                }
            }
        }
    }
}

/// 写装饰器：UTF-8 → GBK/GB18030 字节流。
///
/// `pending` 缓冲跨 `write` 边界的半个 UTF-8 序列；不可映射字符写 `?`。
pub struct GbkStreamWriter {
    inner: Box<dyn Write + Send>,
    encoder: Encoder,
    pending: Vec<u8>,
    /// 编码输出暂存（编码满一批就往 inner 写一批）
    out: Vec<u8>,
}

impl GbkStreamWriter {
    fn new(inner: Box<dyn Write + Send>, encoding: &'static Encoding) -> Self {
        Self {
            inner,
            encoder: encoding.new_encoder(),
            pending: Vec::new(),
            out: Vec::with_capacity(4096),
        }
    }

    /// 把一段合法 UTF-8 编码进 out。
    ///
    /// 用 `without_replacement` 变体自控不可映射字符：写 `?`（终端传统），
    /// 而非 encoding_rs 默认的 HTML 数字实体（`&#128640;` 会在终端露馅）。
    /// Unmappable 返回时 `read` 已含问题字符，直接续推剩余输入。
    fn encode_str(&mut self, text: &str) -> io::Result<()> {
        let mut consumed = 0;
        loop {
            let rest = &text[consumed..];
            if rest.is_empty() {
                return Ok(());
            }
            self.out.resize(self.out.len() + rest.len() * 4 + 1, 0);
            let base = self.out.len() - rest.len() * 4 - 1;
            let tail = &mut self.out[base..];
            let (result, read, written) = self
                .encoder
                .encode_from_utf8_without_replacement(rest, tail, false);
            self.out.truncate(base + written);
            match result {
                EncoderResult::InputEmpty => return Ok(()),
                EncoderResult::OutputFull => consumed += read,
                EncoderResult::Unmappable(_) => {
                    self.out.push(b'?');
                    consumed += read;
                }
            }
        }
    }

    fn drain_out(&mut self) -> io::Result<()> {
        if !self.out.is_empty() {
            self.inner.write_all(&self.out)?;
            self.out.clear();
        }
        Ok(())
    }
}

impl Write for GbkStreamWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        loop {
            // 先取边界判定（结束借用），再按边界操作（可变借用）
            let verdict = match std::str::from_utf8(&self.pending) {
                Ok(_) => PendingVerdict::Complete,
                Err(error) => match error.error_len() {
                    Some(bad) => PendingVerdict::Invalid {
                        valid_up_to: error.valid_up_to(),
                        bad,
                    },
                    None => PendingVerdict::Incomplete {
                        valid_up_to: error.valid_up_to(),
                    },
                },
            };
            match verdict {
                PendingVerdict::Complete => {
                    // take 拿走所有权再编码：文本借用与 &mut self 不再交叉
                    let pending = std::mem::take(&mut self.pending);
                    let text = std::str::from_utf8(&pending).expect("已验合法");
                    self.encode_str(text)?;
                    self.drain_out()?;
                    return Ok(buf.len());
                }
                PendingVerdict::Invalid { valid_up_to, bad } => {
                    let mut pending = std::mem::take(&mut self.pending);
                    if valid_up_to > 0 {
                        let text = std::str::from_utf8(&pending[..valid_up_to]).expect("已验合法");
                        self.encode_str(text)?;
                        pending.drain(..valid_up_to);
                    }
                    // 完整的非法序列：替换为 ?，继续处理后续
                    self.out.push(b'?');
                    pending.drain(..bad);
                    self.pending = pending;
                }
                PendingVerdict::Incomplete { valid_up_to } => {
                    let mut pending = std::mem::take(&mut self.pending);
                    if valid_up_to > 0 {
                        let text = std::str::from_utf8(&pending[..valid_up_to]).expect("已验合法");
                        self.encode_str(text)?;
                        pending.drain(..valid_up_to);
                    }
                    // 剩余是跨 write 边界的半个序列：放回 pending 等凑齐
                    self.pending = pending;
                    self.drain_out()?;
                    return Ok(buf.len());
                }
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // 冲刷编码器状态后落盘。pending 若仍有残缺序列，按 ? 兜底——
        // flush 语义是「就此收尾」，不再等待补齐。
        if !self.pending.is_empty() {
            let mut pending = std::mem::take(&mut self.pending);
            let valid_up_to = match std::str::from_utf8(&pending) {
                Ok(_) => pending.len(),
                Err(error) => error.valid_up_to(),
            };
            if valid_up_to > 0 {
                let text = std::str::from_utf8(&pending[..valid_up_to]).expect("已验合法");
                self.encode_str(text)?;
                pending.drain(..valid_up_to);
            }
            // flush 语义是「就此收尾」：残缺序列按 ? 兜底，不再等补齐
            if !pending.is_empty() {
                self.out.push(b'?');
            }
        }
        self.drain_out()?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// 一个只能整块吐字节的假源（控制分块边界）。
    struct ChunkReader {
        chunks: Vec<Vec<u8>>,
        index: usize,
    }

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.index >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.index];
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            self.index += 1;
            Ok(n)
        }
    }

    /// 共享缓冲 Sink：测试里回收写端字节（Box<dyn Write> 无法 downcast）。
    #[derive(Clone, Default)]
    struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn gbk_reader(chunks: Vec<Vec<u8>>) -> GbkStreamReader {
        GbkStreamReader::new(Box::new(ChunkReader { chunks, index: 0 }), encoding_rs::GBK)
    }

    /// 中文 GBK 权威字节（iconv 生成）：中文 = D6 D0 CE C4。
    const ZH_GBK: [u8; 4] = [0xD6, 0xD0, 0xCE, 0xC4];
    /// 中文 UTF-8：E4 B8 AD E6 96 87。
    const ZH_UTF8: [u8; 6] = [0xE4, 0xB8, 0xAD, 0xE6, 0x96, 0x87];

    #[test]
    fn gbk_to_utf8_whole_chunk() {
        let mut reader = gbk_reader(vec![ZH_GBK.to_vec()]);
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("读取");
        assert_eq!(out, "中文");
    }

    /// 半个汉字跨包到达是常态：D6 | D0 CE | C4 三段也必须拼出「中文」。
    #[test]
    fn gbk_to_utf8_split_across_reads() {
        let mut reader = gbk_reader(vec![vec![0xD6], vec![0xD0, 0xCE], vec![0xC4, b'\n']]);
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("读取");
        assert_eq!(out, "中文\n");
    }

    #[test]
    fn ascii_passes_through_unchanged() {
        let mut reader = gbk_reader(vec![b"GBK_MIXED ascii".to_vec()]);
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("读取");
        assert_eq!(out, "GBK_MIXED ascii");
    }

    /// 非法 GBK 字节（0xFF 在 GBK 任何位置都不合法；注意 0x80 合法映射 €）：
    /// lossy 替换 U+FFFD，流不死。
    #[test]
    fn malformed_gbk_yields_replacement_not_stall() {
        let mut reader = gbk_reader(vec![vec![0xFF, 0xD6, 0xD0]]);
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("读取");
        assert_eq!(out, "\u{FFFD}中");
    }

    #[test]
    fn gb18030_encoding_variant_applies() {
        let mut reader = GbkStreamReader::new(
            Box::new(ChunkReader {
                chunks: vec![ZH_GBK.to_vec()],
                index: 0,
            }),
            encoding_rs::GB18030,
        );
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("读取");
        assert_eq!(out, "中文");
    }

    /// UTF-8 → GBK：中文编为 D6 D0 CE C4；写出后 iconv 级别的字节精确。
    #[test]
    fn utf8_to_gbk_writer() {
        let sink = SharedSink::default();
        let mut writer = GbkStreamWriter::new(Box::new(sink.clone()), encoding_rs::GBK);
        writer.write_all("中文".as_bytes()).expect("写入");
        writer.flush().expect("冲刷");
        assert_eq!(*sink.0.lock().unwrap(), ZH_GBK);
    }

    /// 半个 UTF-8 序列跨 write 边界：先缓冲，凑齐再编。
    #[test]
    fn utf8_writer_buffers_partial_sequence() {
        let zhong_utf8 = &ZH_UTF8[..3]; // 「中」= E4 B8 AD
        let sink = SharedSink::default();
        let mut writer = GbkStreamWriter::new(Box::new(sink.clone()), encoding_rs::GBK);
        writer.write_all(&zhong_utf8[..1]).expect("首字节");
        writer.write_all(&zhong_utf8[1..]).expect("余下两字节");
        // 再补一个完整字，验证状态机无残留干扰
        writer.write_all("文".as_bytes()).expect("第二字");
        writer.flush().expect("冲刷");
        assert_eq!(*sink.0.lock().unwrap(), ZH_GBK);
    }

    /// GBK 里没有的字符（如 emoji）：替换为 `?`，其余照编。
    #[test]
    fn unmappable_char_becomes_question_mark() {
        let sink = SharedSink::default();
        let mut writer = GbkStreamWriter::new(Box::new(sink.clone()), encoding_rs::GBK);
        writer.write_all("中🚀文".as_bytes()).expect("写入");
        writer.flush().expect("冲刷");
        assert_eq!(*sink.0.lock().unwrap(), [0xD6, 0xD0, b'?', 0xCE, 0xC4]);
    }

    /// apply 组装口径：UTF-8 会话不包装（原样字节直通），GBK 会话双向装饰。
    #[test]
    fn apply_wraps_only_non_utf8() {
        let make = || SessionStreams {
            reader: Box::new(ChunkReader {
                chunks: vec![ZH_GBK.to_vec()],
                index: 0,
            }),
            writer: Box::new(Vec::new()),
        };

        // UTF-8：GBK 原始字节原样穿透（不做任何转码）
        let mut utf8 = SessionEncoding::Utf8.apply(make());
        let mut raw = Vec::new();
        utf8.reader.read_to_end(&mut raw).unwrap();
        assert_eq!(raw, ZH_GBK);

        // GBK：装饰后读出已是 UTF-8
        let mut gbk = SessionEncoding::Gbk.apply(make());
        let mut text = String::new();
        gbk.reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "中文");
    }
}
