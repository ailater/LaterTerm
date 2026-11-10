//! 第三方服务器配置导入：Deepin 终端 server-config.conf / command-config.conf
//! 与 Xshell 会话（.xsh）。
//! 只迁移连接元数据——两类来源的密码要么不存、要么绑定原机器加密，均不可迁移；
//! 导入后的条目在 LaterTerm 首次连接时补输凭据（可记住入密钥环）。

use anyhow::{Context as _, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

use crate::commands::CommandTemplate;
use crate::servers::{ServerAuthKind, ServerEntry, normalize_group};

/// Deepin 终端远程管理的默认配置路径。
pub fn deepin_default_path() -> Option<std::path::PathBuf> {
    Some(
        std::path::PathBuf::from(std::env::var_os("HOME")?)
            .join(".config/deepin/deepin-terminal/server-config.conf"),
    )
}

/// Deepin 命令模板的默认配置路径（与 server-config.conf 同目录）。
pub fn deepin_commands_default_path() -> Option<std::path::PathBuf> {
    Some(
        std::path::PathBuf::from(std::env::var_os("HOME")?)
            .join(".config/deepin/deepin-terminal/command-config.conf"),
    )
}

/// QSettings 值还原：`\,` `\;` `\\` → 原字符，其余反斜杠原样保留（路径常见）。
/// 与 laterterm-deepin 严格解析器同口径（来源：deepin-terminal QSettings 写出规则）。
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// 解析 Deepin server-config.conf 全文。
///
/// 段头 `[用户名@主机@端口@显示名]` 是权威信息，段内键值补充/兜底：
/// `Name`/`address`/`port`/`userName` 与段头重复，`GroupName` 进分组成员
/// 字段（不再拼进备注），`Path`/`Encode`/`Command`/`PrivateKey` 为附加
/// 信息（并入备注或映射认证方式）。
pub fn parse_deepin_servers(body: &str) -> Vec<ServerEntry> {
    let mut out: Vec<ServerEntry> = Vec::new();
    // (用户, 主机, 端口文本, 名称)；名称可能含 @，按前三个 @ 切分
    let mut header: Option<(String, String, String, String)> = None;
    let mut extras: BTreeMap<String, String> = BTreeMap::new();

    let flush = |header: &mut Option<(String, String, String, String)>,
                 extras: &mut BTreeMap<String, String>,
                 out: &mut Vec<ServerEntry>| {
        let Some((user, host, port_text, header_name)) = header.take() else {
            extras.clear();
            return;
        };
        let get = |key: &str| {
            extras
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        let host = {
            let from_key = get("address");
            if from_key.is_empty() { host } else { from_key }
        };
        let port = get("port")
            .parse::<u16>()
            .or_else(|_| port_text.parse::<u16>())
            .unwrap_or(22);
        let username = {
            let from_key = get("userName");
            if from_key.is_empty() { user } else { from_key }
        };
        let mut name = {
            let from_key = get("Name");
            if from_key.is_empty() {
                header_name
            } else {
                from_key
            }
        };
        if name.is_empty() {
            name = host.clone();
        }
        // 同名条目追加主机区分，避免导入合并时被同名规则整批跳过
        while out.iter().any(|item| item.name == name) {
            name = format!("{name}@{host}");
        }

        let private_key = get("PrivateKey");
        let (auth, key_path) = if private_key.is_empty() {
            (ServerAuthKind::Password, None)
        } else {
            (ServerAuthKind::PrivateKey, Some(private_key))
        };

        let mut note_parts = Vec::new();
        let path = get("Path");
        if !path.is_empty() {
            note_parts.push(format!("目录 {path}"));
        }
        let encode = get("Encode");
        if !encode.is_empty() && !encode.eq_ignore_ascii_case("utf-8") {
            note_parts.push(format!("编码 {encode}"));
        }
        let command = get("Command");
        if !command.is_empty() {
            note_parts.push(format!("启动命令 {command}"));
        }
        let note: String = note_parts.join(" · ").chars().take(200).collect();

        let entry = ServerEntry {
            name,
            host,
            port,
            username,
            auth,
            key_path,
            note,
            // 分组进结构化字段（坏分组经 normalize_group 降级 None，不丢条目）
            group: normalize_group(Some(get("GroupName").as_str())),
            shortcut: None,
            encoding: match encode.to_ascii_uppercase().as_str() {
                "GBK" => Some("GBK".into()),
                "GB18030" => Some("GB18030".into()),
                _ => None,
            },
            jump: None,
            tab_color: None,
        };
        if entry.validate().is_ok() {
            out.push(entry);
        }
        extras.clear();
    };

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            flush(&mut header, &mut extras, &mut out);
            let inner = unescape(inner);
            let mut parts = inner.splitn(4, '@');
            let user = parts.next().unwrap_or_default().to_string();
            let host = parts.next().unwrap_or_default().to_string();
            let port = parts.next().unwrap_or_default().to_string();
            let name = parts.next().unwrap_or_default().to_string();
            header = Some((user, host, port, name));
        } else if let Some((key, value)) = line.split_once('=') {
            extras.insert(unescape(key.trim()), unescape(value.trim()));
        }
    }
    flush(&mut header, &mut extras, &mut out);
    out
}

/// 从 Deepin 默认路径读取并解析；文件不存在时给出可行动的错误提示。
pub fn load_deepin_servers(path: &Path) -> Result<Vec<ServerEntry>> {
    if !path.exists() {
        bail!(
            "未找到 Deepin 终端配置 {}（本机没装 Deepin 终端？）",
            path.display()
        );
    }
    let body =
        std::fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    Ok(parse_deepin_servers(&body))
}

/// 解析 Deepin command-config.conf 为命令模板（宽容口径）。
///
/// 与 laterterm-deepin 的严格版不同：坏行/未知键/空 `Command` 的节不整体报错，
/// 而是跳过并计数——导入要尽量多带数据回家，但跳过多少必须可见。
/// 节名 → 命令名；`Command` → 模板体；`Shortcut` 原样保留（归一化交给合并方）；
/// `confirm` 统一 `true`（与主线 UI 新建命令一致，导入命令执行前同样要求确认）。
///
/// 返回 (模板列表, 跳过计数)。跳过计数 = 空/缺 `Command` 的节 + 节外键/未知键 + 坏行。
pub fn parse_deepin_commands(body: &str) -> (Vec<CommandTemplate>, usize) {
    let mut out: Vec<CommandTemplate> = Vec::new();
    let mut skipped = 0usize;
    // 当前节的中间态：(节名, Command, Shortcut)
    let mut current: Option<(String, Option<String>, Option<String>)> = None;
    {
        let flush = |current: &mut Option<(String, Option<String>, Option<String>)>,
                     out: &mut Vec<CommandTemplate>,
                     skipped: &mut usize| {
            if let Some((name, command, shortcut)) = current.take() {
                match command {
                    Some(template) if !template.is_empty() => out.push(CommandTemplate {
                        name,
                        template,
                        shortcut: shortcut.filter(|text| !text.is_empty()),
                        confirm: true,
                    }),
                    _ => *skipped += 1, // 空/缺 Command 的节丢弃并计数
                }
            }
        };
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                flush(&mut current, &mut out, &mut skipped);
                current = Some((unescape(inner), None, None));
            } else if let Some((key, value)) = line.split_once('=') {
                let value = unescape(value.trim());
                match unescape(key.trim()).as_str() {
                    "Command" => match &mut current {
                        Some(entry) => entry.1 = Some(value),
                        None => skipped += 1, // 节外的键
                    },
                    "Shortcut" => match &mut current {
                        Some(entry) => entry.2 = Some(value),
                        None => skipped += 1,
                    },
                    _ => skipped += 1, // 未知键（deepin 未来加键不破坏导入）
                }
            } else {
                skipped += 1; // 坏行
            }
        }
        flush(&mut current, &mut out, &mut skipped);
    }
    (out, skipped)
}

/// 从指定路径读取并解析 Deepin 命令模板；文件不存在时给出可行动的错误提示。
pub fn load_deepin_commands(path: &Path) -> Result<(Vec<CommandTemplate>, usize)> {
    if !path.exists() {
        bail!(
            "未找到 Deepin 终端命令配置 {}（本机没装 Deepin 终端？）",
            path.display()
        );
    }
    let body =
        std::fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    Ok(parse_deepin_commands(&body))
}

/// 按字节序标记解码文本：Xshell 的 .xsh 常为 UTF-16LE。
pub fn decode_text_with_bom(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// 解析单个 Xshell 会话文件内容（INI 风格）。
///
/// 只取 Host/Port/User 与认证方式；`UserKey` 是 Xshell 侧的密钥名称而非本机路径，
/// 仅作为占位保留，私钥本体需要用户自行放置并核对路径。
pub fn parse_xshell_session(file_stem: &str, body: &str) -> Option<ServerEntry> {
    let mut host = String::new();
    let mut port = 22u16;
    let mut username = String::new();
    let mut method = String::new();
    let mut user_key = String::new();
    for line in body.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "host" if host.is_empty() => host = value.to_string(),
            "port" => {
                if let Ok(parsed) = value.parse::<u16>() {
                    port = parsed;
                }
            }
            "user" if username.is_empty() => username = value.to_string(),
            "method" => method = value.to_string(),
            "userkey" => user_key = value.to_string(),
            _ => {}
        }
    }
    if host.is_empty() {
        return None;
    }
    let upper = method.to_ascii_uppercase();
    let (auth, key_path) = if upper.contains("PUBLICKEY") && !user_key.is_empty() {
        (ServerAuthKind::PrivateKey, Some(user_key))
    } else if upper.contains("INTERACTIVE") {
        (ServerAuthKind::KeyboardInteractive, None)
    } else {
        (ServerAuthKind::Password, None)
    };
    let name = if file_stem.trim().is_empty() {
        host.clone()
    } else {
        file_stem.trim().to_string()
    };
    let note = if method.is_empty() {
        String::new()
    } else {
        format!("来自 Xshell（{method}）")
    };
    let entry = ServerEntry {
        name,
        host,
        port,
        username,
        auth,
        key_path,
        note,
        group: None,
        shortcut: None,
        encoding: None,
        jump: None,
        tab_color: None,
    };
    (entry.validate().is_ok()).then_some(entry)
}

/// 批量导入 Xshell 会话文件：返回成功解析的条目与被跳过的文件数。
pub fn load_xshell_sessions(paths: &[std::path::PathBuf]) -> Result<(Vec<ServerEntry>, usize)> {
    if paths.is_empty() {
        bail!("未选择任何会话文件");
    }
    let mut out: Vec<ServerEntry> = Vec::new();
    let mut skipped = 0usize;
    for path in paths {
        let bytes = std::fs::read(path).with_context(|| format!("读取 {} 失败", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let body = decode_text_with_bom(&bytes);
        match parse_xshell_session(&stem, &body) {
            Some(mut entry) => {
                while out.iter().any(|item| item.name == entry.name) {
                    entry.name = format!("{}@{}", entry.name, entry.host);
                }
                out.push(entry);
            }
            None => skipped += 1,
        }
    }
    Ok((out, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    const DEEPIN_SAMPLE: &str = r#"
[jilikun@10.10.24.191@2222@跳板机]
Backspace = ascii-del
Command = ls -l
Encode = UTF-8
GroupName =
Name = 跳板机
Path = /data/wwwroot/
PrivateKey =
address = 10.10.24.191
port = 2222
userName = jilikun
[root@119.28.62.112@22@tencent-hk]
GroupName = other
Name = tencent-hk
Encode = GBK
PrivateKey = /home/me/.ssh/id_ed25519
address = 119.28.62.112
port = 22
userName = root
"#;

    #[test]
    fn deepin_sample_parses_header_and_extras() {
        let entries = parse_deepin_servers(DEEPIN_SAMPLE);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "跳板机");
        assert_eq!(entries[0].host, "10.10.24.191");
        assert_eq!(entries[0].port, 2222);
        assert_eq!(entries[0].username, "jilikun");
        assert_eq!(entries[0].auth, ServerAuthKind::Password);
        assert_eq!(entries[0].group, None, "空 GroupName → 未分组");
        assert!(
            entries[0].note.contains("目录 /data/wwwroot/"),
            "{}",
            entries[0].note
        );
        // UTF-8 编码不进备注，GBK 编码要进（GBK 转码迁移的线索）
        assert_eq!(entries[1].auth, ServerAuthKind::PrivateKey);
        assert_eq!(
            entries[1].key_path.as_deref(),
            Some("/home/me/.ssh/id_ed25519")
        );
        // GroupName 进结构化分组字段，不再拼进备注
        assert_eq!(entries[1].group.as_deref(), Some("other"));
        assert!(!entries[1].note.contains("分组"), "{}", entries[1].note);
        assert!(entries[1].note.contains("编码 GBK"));
        for entry in &entries {
            entry.validate().unwrap();
        }
    }

    #[test]
    fn deepin_qsettings_escapes_restored_in_names_values_and_groups() {
        let body = "[root@10.0.0.1@22@逗号\\,名字]\n\
                    Command = echo a\\,b\n\
                    GroupName = 生产\\,一线\n";
        let entries = parse_deepin_servers(body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "逗号,名字");
        assert_eq!(entries[0].group.as_deref(), Some("生产,一线"));
        assert!(
            entries[0].note.contains("启动命令 echo a,b"),
            "{}",
            entries[0].note
        );
        // 非 QSettings 逃逸的反斜杠（路径）原样保留
        let body = "[root@10.0.0.2@22@路径]\nPath = C:\\data\\www\n";
        let entries = parse_deepin_servers(body);
        assert!(
            entries[0].note.contains("目录 C:\\data\\www"),
            "{}",
            entries[0].note
        );
    }

    #[test]
    fn deepin_commands_parse_tolerantly_with_skip_count() {
        let body = "\
[更新系统]
Command = sudo apt update && sudo apt dist-upgrade -y
Shortcut =

[跳板机otp]
Command = /opt/tools/googleAuth RXOOV3HI4KGVVZHK
Shortcut = Ctrl+Shift+G

[空命令]
Command =
Mystery = x
坏行没有等号
";
        let (commands, skipped) = parse_deepin_commands(body);
        assert_eq!(commands.len(), 2, "空 Command 的节丢弃");
        assert_eq!(commands[0].name, "更新系统");
        assert_eq!(
            commands[0].template,
            "sudo apt update && sudo apt dist-upgrade -y"
        );
        assert_eq!(commands[0].shortcut, None);
        assert!(commands[0].confirm, "导入命令与主线一致默认需确认");
        assert_eq!(commands[1].shortcut.as_deref(), Some("Ctrl+Shift+G"));
        for command in &commands {
            command.validate().expect("导入产物必须过主线校验");
        }
        // 空命令节 + 未知键 + 坏行各计一次跳过
        assert_eq!(skipped, 3);
    }

    #[test]
    fn deepin_commands_unescape_and_roundtrip_through_settings() {
        let (escaped, skipped) =
            parse_deepin_commands("[转义]\nCommand = echo a\\,b\\;c\\\\d\nShortcut =\n");
        assert_eq!(skipped, 0);
        assert_eq!(escaped[0].template, r"echo a,b;c\d");
        // 缺 Shortcut 的节同样成立
        let (plain, _) = parse_deepin_commands("[只有命令]\nCommand = pwd\n");
        assert_eq!(plain[0].name, "只有命令");
        assert_eq!(plain[0].shortcut, None);

        // 产物 upsert → 落盘 → 重读回环（转义还原后的模板字节不丢）
        let mut settings = Settings::default();
        for command in escaped {
            settings.upsert_command(command).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.commands.len(), 1);
        assert_eq!(loaded.commands[0].template, r"echo a,b;c\d");
        assert_eq!(loaded.commands, settings.commands);
    }

    #[test]
    fn deepin_duplicate_names_get_host_suffix() {
        let body = "[root@1.1.1.1@22@同名]\naddress = 1.1.1.1\nport = 22\nuserName = root\n\
                    [root@2.2.2.2@22@同名]\naddress = 2.2.2.2\nport = 22\nuserName = root\n";
        let entries = parse_deepin_servers(body);
        assert_eq!(entries.len(), 2);
        assert_ne!(entries[0].name, entries[1].name);
        assert_eq!(entries[1].name, "同名@2.2.2.2");
    }

    #[test]
    fn xshell_ini_parses_auth_variants() {
        let password = parse_xshell_session(
            "web-1",
            "[CONNECTION]\nHost=192.168.1.10\nPort=2222\nUser=ops\n\
             [CONNECTION_AUTHENTICATION]\nMethod=PASSWORD\n",
        )
        .unwrap();
        assert_eq!(password.name, "web-1");
        assert_eq!(password.host, "192.168.1.10");
        assert_eq!(password.port, 2222);
        assert_eq!(password.auth, ServerAuthKind::Password);

        let interactive = parse_xshell_session(
            "跳板",
            "[CONNECTION]\nHost=jump.example.com\nUser=root\n\
             [CONNECTION_AUTHENTICATION]\nMethod=KEYBOARD_INTERACTIVE\n",
        )
        .unwrap();
        assert_eq!(interactive.auth, ServerAuthKind::KeyboardInteractive);
        assert_eq!(interactive.port, 22, "缺省端口");

        let pubkey = parse_xshell_session(
            "k",
            "[CONNECTION]\nHost=h\nUser=u\n\
             [CONNECTION_AUTHENTICATION]\nMethod=PUBLICKEY\nUserKey=mykey\n",
        )
        .unwrap();
        assert_eq!(pubkey.auth, ServerAuthKind::PrivateKey);
        assert_eq!(pubkey.key_path.as_deref(), Some("mykey"));
    }

    #[test]
    fn xshell_utf16le_bom_decodes() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "[CONNECTION]\nHost=h\nUser=u\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let body = decode_text_with_bom(&bytes);
        assert!(body.contains("Host=h"), "{body:?}");
        let entry = parse_xshell_session("中文会话", &body).unwrap();
        assert_eq!(entry.name, "中文会话");
        assert_eq!(entry.host, "h");
    }

    #[test]
    fn xshell_without_host_is_skipped() {
        assert!(parse_xshell_session("x", "[CONNECTION]\nPort=22\n").is_none());
    }
}

#[cfg(test)]
mod real_machine_tests {
    use super::*;

    /// 本机有 Deepin 终端配置时做真机冒烟；无该文件则自动跳过（其他机器不受影响）。
    #[test]
    fn deepin_real_machine_file_if_present() {
        let Some(path) = deepin_default_path() else {
            return;
        };
        if !path.exists() {
            return;
        }
        let entries = load_deepin_servers(&path).expect("真机 Deepin 配置应可解析");
        println!("真机 Deepin 条目 {} 台：", entries.len());
        for entry in &entries {
            println!(
                "  {} · {}@{}:{} [分组 {:?} · note: {}]",
                entry.name, entry.username, entry.host, entry.port, entry.group, entry.note
            );
        }
        assert!(!entries.is_empty(), "本机样例应至少解析出一台");
    }

    /// 本机有 Deepin 命令配置时做真机冒烟；无该文件则自动跳过（其他机器不受影响）。
    #[test]
    fn deepin_commands_real_machine_if_present() {
        let Some(path) = deepin_commands_default_path() else {
            return;
        };
        if !path.exists() {
            return;
        }
        let (commands, skipped) = load_deepin_commands(&path).expect("真机命令配置应可解析");
        println!("真机 Deepin 命令 {} 条（跳过 {skipped}）：", commands.len());
        for command in &commands {
            println!("  {} · {}", command.name, command.template);
            command.validate().expect("真机命令应过主线校验");
        }
    }
}
