//! 配置导入转换层（M3）：Xshell 会话 → LaterTerm 模型。
//!
//! 纯函数、无 I/O——解析（laterterm-xshell）与落盘（laterterm-config）分居两端，
//! 本模块只做字段映射与安全过滤。
//!
//! 安全口径：
//! - XShell 密码不迁移（私有加密不可解，见 laterterm-xshell 文档）
//! - 编码只认 UTF-8 / GBK / GB18030 三个已知值，未知→None（默认 UTF-8）
//! - 服务器名冲突由调用方 upsert 语义覆盖
//!
//! deepin-terminal 的解析与转换已并入 `laterterm_config::importers`
//! （宽容口径 + QSettings unescape + 分组字段），本文件不再保留严格版镜像实现。

use laterterm_config::servers::{ServerAuthKind, ServerEntry};
use laterterm_xshell::XshellSession;

/// XShell 会话 → LaterTerm 服务器条目。非 SSH 协议（TELNET/SERIAL…）跳过。
pub fn xshell_to_server(session: &XshellSession) -> Option<ServerEntry> {
    // LaterTerm 只接 SSH
    if let Some(protocol) = &session.protocol
        && !protocol.eq_ignore_ascii_case("SSH")
    {
        return None;
    }
    let host = session.host.clone()?;
    let username = session.username.clone()?;
    Some(ServerEntry {
        name: session.name.clone(),
        host,
        port: session.port.unwrap_or(22),
        username,
        // XShell 的 UserKey 是密钥名（非路径），LaterTerm 要路径——无法自动映射；
        // 密码绑定原机器加密不可迁移。统一回落 agent，用户在编辑弹窗里改。
        auth: ServerAuthKind::Agent,
        key_path: None,
        note: String::new(),
        // 目录树分组（如「生产/入口层」）整串保留，进结构化分组字段
        group: session.group.clone(),
        shortcut: None,
        encoding: normalize_encoding(session.encoding.as_deref()),
        jump: None,
    })
}

/// 批量转换：返回 (导入的服务器, 跳过的条目名+原因)。
pub fn xshell_batch(sessions: &[XshellSession]) -> (Vec<ServerEntry>, Vec<(String, String)>) {
    let mut servers = Vec::new();
    let mut skipped = Vec::new();
    for session in sessions {
        match xshell_to_server(session) {
            Some(server) => servers.push(server),
            None => {
                let reason = if session
                    .protocol
                    .as_deref()
                    .is_some_and(|p| !p.eq_ignore_ascii_case("SSH"))
                {
                    format!(
                        "非 SSH 协议（{}）",
                        session.protocol.as_deref().unwrap_or("?")
                    )
                } else if session.host.is_none() {
                    "缺 Host".to_string()
                } else if session.username.is_none() {
                    "缺 UserName".to_string()
                } else {
                    "未知".to_string()
                };
                skipped.push((session.name.clone(), reason));
            }
        }
    }
    (servers, skipped)
}

/// 编码归一化：只认 UTF-8/GBK/GB18030（大小写不敏感），未知返回 None。
pub fn normalize_encoding(raw: Option<&str>) -> Option<String> {
    match raw.map(|e| e.to_ascii_uppercase()).as_deref() {
        Some("UTF-8") | Some("UTF8") => None, // 默认，不显式存
        Some("GBK") => Some("GBK".into()),
        Some("GB18030") => Some("GB18030".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xshell_ssh() -> XshellSession {
        XshellSession {
            name: "入口机".into(),
            group: Some("生产/入口层".into()),
            protocol: Some("SSH".into()),
            host: Some("192.0.2.10".into()),
            port: Some(2222),
            username: Some("alice".into()),
            auth_method: laterterm_xshell::XshellAuthMethod::Password,
            user_key: None,
            encoding: Some("UTF-8".into()),
            description: None,
            remote_start_dir: None,
        }
    }

    #[test]
    fn xshell_ssh_maps_to_server_entry() {
        let entry = xshell_to_server(&xshell_ssh()).expect("SSH 必须映射");
        assert_eq!(entry.name, "入口机");
        assert_eq!(entry.host, "192.0.2.10");
        assert_eq!(entry.port, 2222);
        assert_eq!(entry.username, "alice");
        // XShell 密码不可迁移，认证回落 agent
        assert_eq!(entry.auth, ServerAuthKind::Agent);
        // UTF-8 是默认，编码不显式存
        assert_eq!(entry.encoding, None);
        // 目录树分组整串进结构化字段
        assert_eq!(entry.group.as_deref(), Some("生产/入口层"));
    }

    #[test]
    fn xshell_gbk_encoding_normalized() {
        let mut session = xshell_ssh();
        session.encoding = Some("GB18030".into());
        let entry = xshell_to_server(&session).expect("映射");
        assert_eq!(entry.encoding.as_deref(), Some("GB18030"));

        session.encoding = Some("ISO-8859-1".into());
        let entry = xshell_to_server(&session).expect("映射");
        assert_eq!(entry.encoding, None, "未知编码回落默认");
    }

    #[test]
    fn xshell_non_ssh_skipped_with_reason() {
        let mut telnet = xshell_ssh();
        telnet.protocol = Some("TELNET".into());
        let (servers, skipped) = xshell_batch(&[xshell_ssh(), telnet]);
        assert_eq!(servers.len(), 1);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].1.contains("TELNET"), "{:?}", skipped[0]);
    }

    #[test]
    fn xshell_missing_host_skipped() {
        let mut no_host = xshell_ssh();
        no_host.host = None;
        assert!(xshell_to_server(&no_host).is_none());
    }

    #[test]
    fn encoding_normalization_edge_cases() {
        assert_eq!(normalize_encoding(Some("utf-8")), None);
        assert_eq!(normalize_encoding(Some("UTF8")), None);
        assert_eq!(normalize_encoding(Some("gbk")), Some("GBK".into()));
        assert_eq!(normalize_encoding(Some("GB18030")), Some("GB18030".into()));
        assert_eq!(normalize_encoding(Some("BIG5")), None, "不支持 Big5");
        assert_eq!(normalize_encoding(None), None);
    }
}
