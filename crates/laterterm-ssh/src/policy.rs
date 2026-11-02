//! 主机键 TOFU 策略与连接期错误。
//!
//! TOFU（trust-on-first-use）：首次连接把服务器主机键记入 known_hosts
//! 文件并信任；此后主机键与记录不一致即拒绝——OpenSSH 的默认信任模型。
//! 文件缺省视为「尚未见过该主机」，走首连记录路径。

use russh::client;
use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{Error as KeysError, PublicKeyOrCertificate, check_known_hosts_path};
use std::fmt;
use std::path::PathBuf;

/// 连接期错误。`check_server_key` 的拒绝原因经由本类型回到 `connect` 调用方。
#[derive(Debug)]
pub enum ClientError {
    /// 主机键与 known_hosts 记录不一致——可能存在中间人攻击，已拒绝连接。
    HostKeyChanged { line: usize },
    /// OpenSSH 证书主机暂不支持（M3 主线只覆盖裸主机键）。
    HostCertificate,
    /// known_hosts 文件读写失败。
    KnownHosts(String),
    /// russh 协议层错误。
    Russh(russh::Error),
}

impl From<russh::Error> for ClientError {
    fn from(error: russh::Error) -> Self {
        Self::Russh(error)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostKeyChanged { line } => write!(
                f,
                "服务器主机键与 known_hosts 第 {line} 行记录不一致，可能存在中间人攻击；\
                 如确认服务器换键，可删除该行后重连"
            ),
            Self::HostCertificate => {
                write!(f, "服务器使用 OpenSSH 证书认证，暂不支持")
            }
            Self::KnownHosts(message) => write!(f, "known_hosts 读写失败: {message}"),
            Self::Russh(error) => write!(f, "SSH 连接失败: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// 主机键 TOFU 策略。作为 `client::Handler` 挂进连接流程。
pub(crate) struct HostKeyPolicy {
    pub host: String,
    pub port: u16,
    pub known_hosts: PathBuf,
}

impl client::Handler for HostKeyPolicy {
    type Error = ClientError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => return Err(ClientError::HostCertificate),
        };
        match check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            // 文件缺失 / 首次见到该主机 → 记录并信任（learn 内部会建目录）
            Ok(false) => {
                learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts)
                    .map_err(|error| ClientError::KnownHosts(format!("记录主机键失败: {error}")))?;
                Ok(true)
            }
            Err(KeysError::KeyChanged { line }) => Err(ClientError::HostKeyChanged { line }),
            Err(error) => Err(ClientError::KnownHosts(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误文案可读：主机键变更要提示行号与处置方式。
    #[test]
    fn display_messages_are_actionable() {
        let changed = ClientError::HostKeyChanged { line: 7 };
        assert!(changed.to_string().contains("第 7 行"));
        assert!(changed.to_string().contains("中间人"));

        let inner = ClientError::Russh(russh::Error::Disconnect);
        assert!(inner.to_string().starts_with("SSH 连接失败"));
        assert!(ClientError::HostCertificate.to_string().contains("证书"));
    }

    /// From<russh::Error> 满足 client::Handler::Error 约束形状。
    #[test]
    fn russh_error_converts() {
        let error: ClientError = russh::Error::UnknownKey.into();
        assert!(matches!(error, ClientError::Russh(_)));
        // Debug + Error trait 是 Handler::Error 的硬约束
        let _ = format!("{error:?}");
        fn assert_error<T: std::error::Error + Send>(_: &T) {}
        assert_error(&error);
    }
}
