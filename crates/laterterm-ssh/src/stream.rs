//! 会话字节流桥：flume 通道 ↔ std `Read`/`Write` 适配器。
//!
//! 异步泵与视图的阻塞读线程之间只通过这两个类型交界，
//! 泵侧的关闭（发送端 drop）自然转化为视图侧的 EOF / NotConnected。

use std::io::{self, Read, Write};

/// 视图读端：视图的阻塞读线程持有。
pub(crate) struct StreamRx {
    receiver: flume::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
}

impl StreamRx {
    pub(crate) fn new(receiver: flume::Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            pending: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for StreamRx {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // 先吐残留块，取尽再阻塞等新块；
        // 发送端（输出泵）全部关闭 → EOF，即会话结束信号
        if self.offset >= self.pending.len() {
            match self.receiver.recv() {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let remaining = &self.pending[self.offset..];
        let take = remaining.len().min(buf.len());
        buf[..take].copy_from_slice(&remaining[..take]);
        self.offset += take;
        Ok(take)
    }
}

/// 视图写端：键盘输入与命令面板下发共用。
pub(crate) struct StreamTx {
    sender: flume::Sender<Vec<u8>>,
}

impl StreamTx {
    pub(crate) fn new(sender: flume::Sender<Vec<u8>>) -> Self {
        Self { sender }
    }
}

impl Write for StreamTx {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.sender
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "SSH 会话已关闭"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// 残留块跨多次 read 正确续读；发送端关闭后 read 返回 0（EOF）。
    #[test]
    fn rx_drains_chunks_then_eof() {
        let (tx, rx) = flume::bounded(4);
        let mut reader = StreamRx::new(rx);
        tx.send(b"hello".to_vec()).unwrap();
        tx.send(b"world".to_vec()).unwrap();
        drop(tx);

        let mut buffer = [0u8; 3];
        assert_eq!(reader.read(&mut buffer).unwrap(), 3);
        assert_eq!(&buffer, b"hel");
        let mut buffer = [0u8; 3];
        assert_eq!(reader.read(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer[..2], b"lo");
        // 第二块跨过残留边界继续
        let mut buffer = [0u8; 8];
        assert_eq!(reader.read(&mut buffer).unwrap(), 5);
        assert_eq!(&buffer[..5], b"world");
        // 发送端已关 → EOF
        let mut buffer = [0u8; 8];
        assert_eq!(reader.read(&mut buffer).unwrap(), 0);
    }

    /// 接收端关闭后 write 报 NotConnected；空写为无操作。
    #[test]
    fn tx_errors_after_receiver_dropped() {
        let (tx, rx) = flume::unbounded::<Vec<u8>>();
        let mut writer = StreamTx::new(tx);
        assert_eq!(writer.write(b"ok").unwrap(), 2);
        drop(rx);
        let error = writer.write(b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert_eq!(writer.write(b"").unwrap(), 0);
        writer.flush().unwrap();
    }

    /// read 一次最多取 buf.len() 字节，跨线程泵送语义可用。
    #[test]
    fn rx_pumps_across_threads() {
        let (tx, rx) = flume::unbounded();
        let handle = thread::spawn(move || {
            for index in 0..100u32 {
                if tx.send(format!("chunk{index}").into_bytes()).is_err() {
                    break;
                }
            }
        });
        let mut reader = StreamRx::new(rx);
        let mut total = String::new();
        let mut buffer = [0u8; 7];
        loop {
            let read = reader.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            total.push_str(std::str::from_utf8(&buffer[..read]).unwrap());
        }
        handle.join().unwrap();
        assert!(total.starts_with("chunk0"));
        assert!(total.contains("chunk99"));
    }
}
