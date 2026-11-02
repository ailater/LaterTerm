//! 配置导入转换层（M3）：XShell / deepin-terminal → LaterTerm 模型。
//!
//! 纯函数、无 I/O——解析（laterterm-xshell / laterterm-deepin）与
//! 落盘（laterterm-config）分居两端，本模块只做字段映射与安全过滤。
//!
//! 安全口径：
//! - XShell 密码不迁移（私有加密不可解，见 laterterm-xshell 文档）
//! - 编码只认 UTF-8 / GBK / GB18030 三个已知值，未知→None（默认 UTF-8）
//! - 服务器名冲突由调用方 upsert 语义覆盖

// UI 面板接线在下一提交——本模块是纯函数转换层，先行落地并全量测试
#![allow(dead_code)]

use laterterm_config::commands::CommandTemplate;
use laterterm_config::servers::{
    StoredAuthKind as ServerAuthKind, StoredServerEntry as ServerEntry,
};
use laterterm_deepin::{DeepinCommand, DeepinServer};
use laterterm_xshell::XshellSession;

// ───────────────────────── XShell → ServerEntry ─────────────────────────

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
        auth: match session.auth_method {
            laterterm_xshell::XshellAuthMethod::PublicKey => {
                // XShell 的 UserKey 是密钥名（非路径），LaterTerm 要路径——
                // 无法自动映射，回落 agent（用户后续在编辑里改）
                ServerAuthKind::Agent
            }
            _ => ServerAuthKind::Agent,
        },
        encoding: normalize_encoding(session.encoding.as_deref()),
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

// ─────────────────────── deepin → ServerEntry ───────────────────────────

/// deepin-terminal 服务器 → LaterTerm 服务器条目。
pub fn deepin_to_server(server: &DeepinServer) -> Option<ServerEntry> {
    if server.address.is_empty() || server.username.is_empty() {
        return None;
    }
    Some(ServerEntry {
        name: if server.name.is_empty() {
            server.address.clone()
        } else {
            server.name.clone()
        },
        host: server.address.clone(),
        port: server.port,
        username: server.username.clone(),
        auth: match &server.private_key {
            Some(path) if !path.is_empty() => ServerAuthKind::KeyFile(path.clone()),
            _ => ServerAuthKind::Keyring, // 密码默认走密钥环（deepin 密码不入配置文件）
        },
        encoding: normalize_encoding(server.encoding.as_deref()),
    })
}

/// deepin 配置文件不存密码（在 dde 密钥环里），导入后用户在编辑表单重填。
pub fn deepin_needs_password(server: &DeepinServer) -> bool {
    server.private_key.as_deref().unwrap_or("").is_empty()
}

/// 批量转换。
pub fn deepin_servers_batch(servers: &[DeepinServer]) -> (Vec<ServerEntry>, Vec<(String, String)>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for server in servers {
        match deepin_to_server(server) {
            Some(entry) => out.push(entry),
            None => skipped.push((server.name.clone(), "缺地址或用户名".into())),
        }
    }
    (out, skipped)
}

// ─────────────────── deepin → CommandTemplate ───────────────────────────

/// deepin 命令模板 → LaterTerm 命令模板（直映射，模板体不做变量化——
/// deepin 没有 `{{var}}` 语法，原样迁移）。
pub fn deepin_to_command(command: &DeepinCommand) -> CommandTemplate {
    CommandTemplate {
        name: command.name.clone(),
        template: command.command.clone(),
        shortcut: command.shortcut.clone(),
        confirm: false,
    }
}

/// deepin 快捷键格式（`Ctrl+Shift+G`）与 LaterTerm 相同，原样透传；
/// 空的快捷键映射为 None。
pub fn deepin_commands_batch(commands: &[DeepinCommand]) -> Vec<CommandTemplate> {
    commands.iter().map(deepin_to_command).collect()
}

// ───────────────────────────── 工具函数 ────────────────────────────────

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

    fn deepin_server() -> DeepinServer {
        DeepinServer {
            name: "跳板机".into(),
            group: Some("生产".into()),
            username: "ops".into(),
            address: "10.0.0.1".into(),
            port: 22,
            encoding: Some("GBK".into()),
            post_command: None,
            start_path: None,
            private_key: None,
            backspace: None,
            delete: None,
        }
    }

    #[test]
    fn deepin_server_maps_with_password_auth() {
        let entry = deepin_to_server(&deepin_server()).expect("映射");
        assert_eq!(entry.name, "跳板机");
        assert_eq!(entry.host, "10.0.0.1");
        assert_eq!(entry.port, 22);
        assert_eq!(entry.auth, ServerAuthKind::Keyring);
        assert_eq!(entry.encoding.as_deref(), Some("GBK"));
        // 密码不在配置文件里，导入后需手动填
        assert!(deepin_needs_password(&deepin_server()));
    }

    #[test]
    fn deepin_keyfile_auth_maps_directly() {
        let mut server = deepin_server();
        server.private_key = Some("/home/me/.ssh/id_ed25519".into());
        let entry = deepin_to_server(&server).expect("映射");
        assert_eq!(
            entry.auth,
            ServerAuthKind::KeyFile("/home/me/.ssh/id_ed25519".into())
        );
    }

    #[test]
    fn deepin_command_direct_mapping() {
        let command = DeepinCommand {
            name: "更新系统".into(),
            command: "sudo apt update".into(),
            shortcut: Some("Ctrl+Shift+G".into()),
        };
        let template = deepin_to_command(&command);
        assert_eq!(template.name, "更新系统");
        assert_eq!(template.template, "sudo apt update");
        assert_eq!(template.shortcut.as_deref(), Some("Ctrl+Shift+G"));
        assert!(!template.confirm, "deepin 命令无确认语义，默认不确认");
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
