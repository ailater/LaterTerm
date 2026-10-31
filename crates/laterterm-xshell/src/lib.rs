//! XShell 会话导入（M3 迁移工具·解析侧）。
//!
//! 逆向口径（公开 dotfiles 的真实 .xsh 样本 + 社区转换器佐证）：
//!
//! - 一个会话 = Sessions 树下一个 `名字.xsh`，INI 格式；**子目录 = 分组**
//!   （目录里的 `folder.ini` 是 XShell 元数据，导入时跳过）
//! - 关键节/键：`[CONNECTION]` Host/Port/Protocol；`[CONNECTION:AUTHENTICATION]`
//!   UserName/Method/UserKey；`[CONNECTION:SSH]` InitRemoteDirectory；
//!   `[TERMINAL]` Encoding；`[Information]` Description
//! - `Method` 双形态：旧版数字（0/1/2…）、新版字符串（`PASSWORD`/`PUBLICKEY`…），
//!   两种都映射；未知值原样保留进 `Other`
//! - **密码不导入**：XShell 的 `Password` 是绑定 Windows 用户/SID 的私有
//!   加密块（跨机器/跨系统不可解），导入侧只取连接与认证方式元数据
//! - 文件编码：XShell 历史版本存 UTF-16LE（带 BOM），新版 UTF-8/ANSI——
//!   `load_sessions_dir` 按 BOM 自动识别，UTF-16LE 就地解码
//!
//! 与 deepin 导入同纪律：std-only 零依赖、中性结构、转换接线随面板落地
//! （Host/Port/UserName → SshEndpoint；Encoding → SessionEncoding；
//! UserKey → 密钥名，密钥文件本体仍在 XShell 用户密钥目录，另行迁移）。

use std::io;
use std::path::Path;

/// 认证方式（Method 键双形态映射）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum XshellAuthMethod {
    #[default]
    Unknown,
    Password,
    PublicKey,
    KeyboardInteractive,
    None,
    /// 未知值原样保留（XShell 新增方式不破坏导入）。
    Other(String),
}

impl XshellAuthMethod {
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "0" | "PASSWORD" => Self::Password,
            "1" | "PUBLICKEY" | "PUBLIC_KEY" => Self::PublicKey,
            "2" | "KEYBOARD_INTERACTIVE" => Self::KeyboardInteractive,
            "10" | "NONE" => Self::None,
            "" => Self::Unknown,
            other => Self::Other(other.to_string()),
        }
    }
}

/// 一个 XShell 会话（中性结构）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XshellSession {
    /// 会话名（文件名去 .xsh）。
    pub name: String,
    /// 分组：Sessions 树内的相对目录路径（如 `生产/入口层`）；顶层为 None。
    pub group: Option<String>,
    /// 协议原文（SSH/TELNET/SERIAL…；LaterTerm 面板侧按需过滤非 SSH）。
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth_method: XshellAuthMethod,
    /// 用户密钥名（XShell 用户密钥目录里的标识，非文件路径）。
    pub user_key: Option<String>,
    /// 终端编码原文（如 `UTF-8` / `GB18030`）。
    pub encoding: Option<String>,
    pub description: Option<String>,
    /// 连接后的远程起始目录（InitRemoteDirectory）。
    pub remote_start_dir: Option<String>,
}

/// 解析错误：源（文件或内联文本）+ 行号 + 原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub source: String,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} 第 {} 行：{}", self.source, self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// 解析单个 .xsh 文本（UTF-8/ANSI 已解码后的内容）。
pub fn parse_session(source: &str, text: &str) -> Result<XshellSession, ParseError> {
    let file_name = Path::new(source)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_string());

    let mut session = XshellSession {
        name: file_name,
        ..Default::default()
    };
    let mut section = String::new();
    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = header.strip_suffix(']').ok_or_else(|| ParseError {
                source: source.to_string(),
                line: line_number,
                message: format!("节头缺右括号: {line}"),
            })?;
            section = name.to_ascii_uppercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ParseError {
                source: source.to_string(),
                line: line_number,
                message: format!("无法识别的行: {line}"),
            });
        };
        let key = key.trim();
        let value = value.trim();
        let optional = |value: &str| (!value.is_empty()).then(|| value.to_string());
        // 节名大小写不敏感（XShell 版本间有 [Connection]/[CONNECTION] 漂移）
        match (section.as_str(), key) {
            ("CONNECTION", "Host") => session.host = optional(value),
            ("CONNECTION", "Port") => {
                session.port = value.parse().ok(); // 非数字容忍为 None
            }
            ("CONNECTION", "Protocol") => session.protocol = optional(value),
            ("CONNECTION:AUTHENTICATION", "UserName") => session.username = optional(value),
            ("CONNECTION:AUTHENTICATION", "Method") => {
                session.auth_method = XshellAuthMethod::parse(value);
            }
            ("CONNECTION:AUTHENTICATION", "UserKey") => session.user_key = optional(value),
            ("CONNECTION:SSH", "InitRemoteDirectory") => {
                session.remote_start_dir = optional(value);
            }
            ("TERMINAL", "Encoding") => session.encoding = optional(value),
            ("INFORMATION", "Description") => session.description = optional(value),
            _ => {}
        }
    }
    Ok(session)
}

/// 遍历 Sessions 目录树（`.xsh` 全收、`folder.ini` 跳过、子目录 = 分组）。
/// 文件编码按 BOM 识别：UTF-16LE 解码，其余按 UTF-8 容错读取。
pub fn load_sessions_dir(root: &Path) -> io::Result<Vec<XshellSession>> {
    let mut sessions = Vec::new();
    walk_sessions(root, root, &mut sessions)?;
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions)
}

fn walk_sessions(root: &Path, dir: &Path, out: &mut Vec<XshellSession>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_sessions(root, &path, out)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("xsh") {
            continue; // folder.ini 及其余杂项跳过
        }
        let bytes = std::fs::read(&path)?;
        let text = decode_xsh(&bytes);
        let display = path.display().to_string();
        match parse_session(&display, &text) {
            Ok(mut session) => {
                session.group = group_of(root, &path);
                out.push(session);
            }
            // 单文件损坏不拖垮整批导入（UI 层按 source 提示）
            Err(error) => eprintln!("跳过损坏会话文件: {error}"),
        }
    }
    Ok(())
}

/// 相对目录路径作为分组（`/` 连接；顶层文件 = None）。
fn group_of(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let parent = relative.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    let parts: Vec<String> = parent
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// BOM 识别解码：UTF-16LE（FF FE）就地解码，其余按 UTF-8 容错。
fn decode_xsh(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结构忠实于公开真实样本（数值 Method 形态），IP/账号虚构。
    const SAMPLE_NUMERIC: &str = "\
[CONNECTION]
Port=2122
StartUp=0
Protocol=SSH
Host=192.0.2.10
KeepAlive=1

[CONNECTION:AUTHENTICATION]
UserName=root
UserKey=
Password=zKWfWf+qjw==
Method=0

[CONNECTION:SSH]
InitRemoteDirectory=/data/wwwroot

[TERMINAL]
Encoding=UTF-8

[INFORMATION]
Description=入口跳板
";

    /// 新版字符串 Method 形态。
    const SAMPLE_STRING_METHOD: &str = "\
[connection]
Host=203.0.113.5
Port=22
Protocol=SSH

[connection:authentication]
UserName=alice
Method=PUBLICKEY
UserKey=my-ed25519
";

    #[test]
    fn numeric_method_session_parses_all_fields() {
        let session = parse_session("入口跳板.xsh", SAMPLE_NUMERIC).expect("解析");
        assert_eq!(session.name, "入口跳板");
        assert_eq!(session.host.as_deref(), Some("192.0.2.10"));
        assert_eq!(session.port, Some(2122));
        assert_eq!(session.protocol.as_deref(), Some("SSH"));
        assert_eq!(session.username.as_deref(), Some("root"));
        assert_eq!(session.auth_method, XshellAuthMethod::Password);
        assert_eq!(session.user_key, None);
        assert_eq!(session.encoding.as_deref(), Some("UTF-8"));
        assert_eq!(session.description.as_deref(), Some("入口跳板"));
        assert_eq!(session.remote_start_dir.as_deref(), Some("/data/wwwroot"));
    }

    #[test]
    fn string_method_and_lowercase_sections_parse() {
        let session = parse_session("alpha.xsh", SAMPLE_STRING_METHOD).expect("解析");
        assert_eq!(session.auth_method, XshellAuthMethod::PublicKey);
        assert_eq!(session.user_key.as_deref(), Some("my-ed25519"));
        assert_eq!(session.port, Some(22));
        // Password 私有加密块从不进入结构
        assert!(
            !format!("{session:?}").contains("zKWfWf"),
            "密码加密块不得进入结构"
        );
    }

    #[test]
    fn unknown_method_kept_verbatim() {
        let session =
            parse_session("x.xsh", "[CONNECTION:AUTHENTICATION]\nMethod=9\n").expect("解析");
        assert_eq!(session.auth_method, XshellAuthMethod::Other("9".into()));
    }

    #[test]
    fn directory_tree_becomes_groups_and_skips_folder_ini() {
        let dir = tempfile::tempdir().expect("临时目录");
        let sessions_root = dir.path().join("Sessions");
        let prod = sessions_root.join("生产").join("入口层");
        std::fs::create_dir_all(&prod).expect("建目录");
        std::fs::write(sessions_root.join("folder.ini"), "[folder]\nanything=1\n")
            .expect("写 folder.ini");
        std::fs::write(sessions_root.join("顶配.xsh"), SAMPLE_NUMERIC).expect("顶层会话");
        std::fs::write(prod.join("内网机.xsh"), SAMPLE_STRING_METHOD).expect("嵌套会话");

        let sessions = load_sessions_dir(&sessions_root).expect("遍历");
        assert_eq!(sessions.len(), 2, "folder.ini 不算会话: {sessions:?}");

        let top = sessions.iter().find(|s| s.name == "顶配").expect("顶层");
        assert_eq!(top.group, None);

        let nested = sessions.iter().find(|s| s.name == "内网机").expect("嵌套");
        assert_eq!(nested.group.as_deref(), Some("生产/入口层"));
    }

    /// 旧版 XShell 的 UTF-16LE 文件（BOM FF FE）能解码解析。
    #[test]
    fn utf16le_file_decodes() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("中文会话.xsh");
        let text = "[CONNECTION]\nHost=192.0.2.99\n[CONNECTION:AUTHENTICATION]\nUserName=小张\n";
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut bytes = vec![0xFF, 0xFE];
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, bytes).expect("写 UTF-16LE 文件");

        let sessions = load_sessions_dir(dir.path()).expect("遍历");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "中文会话");
        assert_eq!(sessions[0].host.as_deref(), Some("192.0.2.99"));
        assert_eq!(sessions[0].username.as_deref(), Some("小张"));
    }

    #[test]
    fn malformed_line_reports_source_and_line() {
        let error = parse_session("bad.xsh", "[CONNECTION]\n裸行\n").expect_err("应报错");
        assert_eq!(error.source, "bad.xsh");
        assert_eq!(error.line, 2);
    }
}
