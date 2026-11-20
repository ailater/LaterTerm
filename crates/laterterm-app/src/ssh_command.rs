//! 添加服务器表单接受的精简 SSH 命令解析。
//!
//! 只负责连接目标，不执行命令、不读取本机 SSH 配置，也不处理凭据。

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SshCommandTarget {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: Option<String>,
}

/// 解析 `ssh [user@]host [-p port]`；`-p` 可位于目标前后，端口缺省为 22。
/// 其他参数与远程命令一律显式拒绝，避免静默忽略后连接到非预期目标。
pub(crate) fn parse_ssh_command(input: &str) -> Result<SshCommandTarget, String> {
    let mut tokens = input.split_whitespace();
    if tokens.next() != Some("ssh") {
        return Err("命令需以 ssh 开头".into());
    }

    let mut destination = None;
    let mut port = 22;
    while let Some(token) = tokens.next() {
        if token == "-p" {
            let value = tokens.next().ok_or_else(|| "-p 后缺少端口".to_string())?;
            port = parse_port(value)?;
        } else if let Some(value) = token.strip_prefix("-p")
            && !value.is_empty()
        {
            port = parse_port(value)?;
        } else if token.starts_with('-') {
            return Err(format!("暂不支持的 SSH 参数 {token:?}，仅支持 -p"));
        } else if destination.replace(token).is_some() {
            return Err("只能有一个目标主机，且不支持远程命令".into());
        }
    }

    let destination = destination.ok_or_else(|| "缺少目标主机".to_string())?;
    if destination.matches('@').count() > 1 {
        return Err("目标格式无效，只能包含一个 @".into());
    }
    let (username, host) = match destination.split_once('@') {
        Some(("", _)) => return Err("用户名不能为空".into()),
        Some((_, "")) => return Err("主机不能为空".into()),
        Some((username, host)) => (Some(username.to_string()), host),
        None => (None, destination),
    };
    if host.is_empty() {
        return Err("主机不能为空".into());
    }

    Ok(SshCommandTarget {
        host: host.to_string(),
        port,
        username,
    })
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| format!("端口无效: {value:?}"))?;
    if port == 0 {
        return Err("端口不能为 0".into());
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::{SshCommandTarget, parse_ssh_command};

    #[test]
    fn parses_compact_port_after_destination() {
        let target = parse_ssh_command("ssh ops@bastion.example.com -p22222").unwrap();
        assert_eq!(
            target,
            SshCommandTarget {
                host: "bastion.example.com".into(),
                port: 22222,
                username: Some("ops".into()),
            }
        );
    }

    #[test]
    fn parses_split_port_on_either_side_and_defaults() {
        for input in [
            "ssh -p 22222 ops@bastion.example.com",
            "  ssh ops@bastion.example.com -p 22222  ",
        ] {
            let target = parse_ssh_command(input).unwrap();
            assert_eq!(target.host, "bastion.example.com");
            assert_eq!(target.port, 22222);
            assert_eq!(target.username.as_deref(), Some("ops"));
        }

        let target = parse_ssh_command("ssh example.com").unwrap();
        assert_eq!(
            target,
            SshCommandTarget {
                host: "example.com".into(),
                port: 22,
                username: None,
            }
        );
    }

    #[test]
    fn rejects_missing_zero_and_out_of_range_ports() {
        for (input, expected) in [
            ("ssh -p ops@example.com", "端口无效"),
            ("ssh ops@example.com -p", "缺少端口"),
            ("ssh ops@example.com -p0", "端口不能为 0"),
            ("ssh ops@example.com -p65536", "端口无效"),
            ("ssh ops@example.com -pabc", "端口无效"),
        ] {
            let error = parse_ssh_command(input).unwrap_err();
            assert!(
                error.contains(expected),
                "{input:?} 应报 {expected:?}，实际 {error:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_or_unsupported_commands() {
        for (input, expected) in [
            ("", "以 ssh 开头"),
            ("mosh example.com", "以 ssh 开头"),
            ("ssh", "缺少目标主机"),
            ("ssh @example.com", "用户名不能为空"),
            ("ssh ops@", "主机不能为空"),
            ("ssh -i key ops@example.com", "仅支持 -p"),
            (
                "ssh ops@one.example.com ops@two.example.com",
                "只能有一个目标主机",
            ),
            ("ssh ops@example.com uptime", "不支持远程命令"),
        ] {
            let error = parse_ssh_command(input).unwrap_err();
            assert!(
                error.contains(expected),
                "{input:?} 应报 {expected:?}，实际 {error:?}"
            );
        }
    }
}
