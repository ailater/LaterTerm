//! deepin-terminal 配置解析（M3 迁移工具的解析侧）。
//!
//! 逆向口径（真实配置样本 + architecture.md 记录）：两份 QSettings INI——
//!
//! - `command-config.conf`：节名 = 命令名；键 `Command`（shell 命令行）、
//!   `Shortcut`（如 `Ctrl+Shift+G`，空 = 无绑定）
//! - `server-config.conf`：节名 = `user@address@port@显示名`（按 `@`
//!   切四段，与 deepin 自身一致）；键含地址/端口/用户/分组/编码
//!   （`Encode`：UTF-8 / GBK）/连接后命令 `Command`/起始目录 `Path`/
//!   `PrivateKey`/退格删除键语义
//!
//! 本 crate 只做「忠实解析成中性结构」（std-only，零依赖）；
//! 到 LaterTerm 命令模板/服务器模型的**转换**在 M2 分支的模板 schema
//! 合入 main 后接线——字段已逐一对应（Encode→SessionEncoding、
//! PrivateKey→KeyFile 认证、Command→连接后下发）。
//!
//! QSettings 逃逸：值中的 `,` `;` `\` 会写成 `\,` `\;` `\\`，解析时还原。

/// 命令模板（command-config.conf 的一个节）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeepinCommand {
    /// 命令名（节名）。
    pub name: String,
    /// shell 命令行。
    pub command: String,
    /// 绑定的快捷键（原文如 `Ctrl+Shift+G`；空/缺省为 None）。
    pub shortcut: Option<String>,
}

/// 远程服务器（server-config.conf 的一个节）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeepinServer {
    /// 显示名（节名第四段，也是 `Name` 键）。
    pub name: String,
    /// 分组（`GroupName`；空 = 未分组）。
    pub group: Option<String>,
    pub username: String,
    pub address: String,
    pub port: u16,
    /// 终端编码（`Encode`：`UTF-8` / `GBK`，原文保留）。
    pub encoding: Option<String>,
    /// 连接成功后下发的命令（`Command`）。
    pub post_command: Option<String>,
    /// 远程起始目录（`Path`）。
    pub start_path: Option<String>,
    /// 私钥文件路径（`PrivateKey`）。
    pub private_key: Option<String>,
    /// 退格键语义（`Backspace`，如 `ascii-del`）。
    pub backspace: Option<String>,
    /// 删除键语义（`Del`，如 `escape-sequence`）。
    pub delete: Option<String>,
}

/// 解析错误：行号 + 原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "第 {} 行：{}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// QSettings 值还原：`\,` `\;` `\\` → 原字符，其余反斜杠原样保留。
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('\\') => out.push('\\'),
                // 不是 QSettings 逃逸序列：反斜杠原样保留（路径常见）
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

/// 极简 INI 行迭代：`[节]` / `键 = 值`，注释（# 与 ;）与空行跳过。
/// 返回 (行号, 节名或键值对)；行号从 1 计。
fn ini_lines(text: &str) -> Result<Vec<(usize, IniLine)>, ParseError> {
    let mut out = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[') {
            let name = section.strip_suffix(']').ok_or_else(|| ParseError {
                line: line_number,
                message: format!("节头缺右括号: {line}"),
            })?;
            out.push((line_number, IniLine::Section(unescape(name))));
        } else if let Some((key, value)) = line.split_once('=') {
            out.push((
                line_number,
                IniLine::Key {
                    key: unescape(key.trim()),
                    value: unescape(value.trim()),
                },
            ));
        } else {
            return Err(ParseError {
                line: line_number,
                message: format!("无法识别的行: {line}"),
            });
        }
    }
    Ok(out)
}

enum IniLine {
    Section(String),
    Key { key: String, value: String },
}

/// 解析 command-config.conf。
pub fn parse_commands(text: &str) -> Result<Vec<DeepinCommand>, ParseError> {
    let mut commands = Vec::new();
    let mut current: Option<DeepinCommand> = None;
    for (line, entry) in ini_lines(text)? {
        match entry {
            IniLine::Section(name) => {
                if let Some(done) = current.take() {
                    commands.push(done);
                }
                current = Some(DeepinCommand {
                    name,
                    ..Default::default()
                });
            }
            IniLine::Key { key, value } => {
                let command = current.as_mut().ok_or_else(|| ParseError {
                    line,
                    message: format!("键 {key} 出现在任何节之外"),
                })?;
                match key.as_str() {
                    "Command" => command.command = value,
                    "Shortcut" => command.shortcut = (!value.is_empty()).then_some(value),
                    // 未知键容忍跳过（deepin 未来加键不破坏导入）
                    _ => {}
                }
            }
        }
    }
    if let Some(done) = current.take() {
        commands.push(done);
    }
    Ok(commands)
}

/// 解析 server-config.conf。
pub fn parse_servers(text: &str) -> Result<Vec<DeepinServer>, ParseError> {
    let mut servers = Vec::new();
    let mut current: Option<DeepinServer> = None;
    for (line, entry) in ini_lines(text)? {
        match entry {
            IniLine::Section(header) => {
                if let Some(done) = current.take() {
                    servers.push(done);
                }
                // 节名 = user@address@port@Name（deepin 自身按 @ 切四段；
                // 名字在末段，可安全容纳含 @ 的用户名/地址）
                let mut parts = header.split('@');
                let username = parts.next().unwrap_or_default().to_string();
                let address = parts.next().unwrap_or_default().to_string();
                let port: u16 = parts
                    .next()
                    .unwrap_or("22")
                    .parse()
                    .map_err(|_| ParseError {
                        line,
                        message: format!("节头端口不是数字: {header}"),
                    })?;
                let name = parts.next().unwrap_or_default().to_string();
                current = Some(DeepinServer {
                    name,
                    username,
                    address,
                    port,
                    ..Default::default()
                });
            }
            IniLine::Key { key, value } => {
                let server = current.as_mut().ok_or_else(|| ParseError {
                    line,
                    message: format!("键 {key} 出现在任何节之外"),
                })?;
                let optional = |value: String| (!value.is_empty()).then_some(value);
                match key.as_str() {
                    "Name" => server.name = value,
                    "GroupName" => server.group = optional(value),
                    "userName" => server.username = value,
                    "address" => server.address = value,
                    "port" => {
                        server.port = value.parse().map_err(|_| ParseError {
                            line,
                            message: format!("port 不是数字: {value}"),
                        })?
                    }
                    "Encode" => server.encoding = optional(value),
                    "Command" => server.post_command = optional(value),
                    "Path" => server.start_path = optional(value),
                    "PrivateKey" => server.private_key = optional(value),
                    "Backspace" => server.backspace = optional(value),
                    "Del" => server.delete = optional(value),
                    _ => {}
                }
            }
        }
    }
    if let Some(done) = current.take() {
        servers.push(done);
    }
    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成 fixture（结构与真实配置一致；IP/用户名全部虚构，真实数据不入库）。
    const COMMANDS_FIXTURE: &str = "\
[更新系统]
Command = sudo apt update && sudo apt dist-upgrade -y
Shortcut =

[跳板机otp]
Command = /opt/tools/googleAuth RXOOV3HI4KGVVZHK
Shortcut = Ctrl+Shift+G

[依赖修复]
Command = sudo dpkg --configure -a
Shortcut =
";

    const SERVERS_FIXTURE: &str = "\
[alice@10.0.0.1@2222@入口机]
Backspace = ascii-del
Command = ls -l
Del = escape-sequence
Encode = GBK
GroupName = 生产
Name = 入口机
Path = /data/wwwroot/
PrivateKey = /home/alice/.ssh/id_ed25519
address = 10.0.0.1
port = 2222
userName = alice

[bob@203.0.113.7@22@测试服]
Backspace = ascii-del
Command =
Del = escape-sequence
Encode = UTF-8
GroupName =
Name = 测试服
Path =
PrivateKey =
address = 203.0.113.7
port = 22
userName = bob
";

    #[test]
    fn commands_parse_names_commands_shortcuts() {
        let commands = parse_commands(COMMANDS_FIXTURE).expect("解析");
        assert_eq!(commands.len(), 3);

        assert_eq!(commands[0].name, "更新系统");
        assert_eq!(
            commands[0].command,
            "sudo apt update && sudo apt dist-upgrade -y"
        );
        assert_eq!(commands[0].shortcut, None);

        assert_eq!(commands[1].name, "跳板机otp");
        assert_eq!(commands[1].shortcut.as_deref(), Some("Ctrl+Shift+G"));

        assert_eq!(commands[2].command, "sudo dpkg --configure -a");
    }

    #[test]
    fn servers_parse_all_fields() {
        let servers = parse_servers(SERVERS_FIXTURE).expect("解析");
        assert_eq!(servers.len(), 2);

        let first = &servers[0];
        assert_eq!(first.name, "入口机");
        assert_eq!(first.group.as_deref(), Some("生产"));
        assert_eq!(first.username, "alice");
        assert_eq!(first.address, "10.0.0.1");
        assert_eq!(first.port, 2222);
        assert_eq!(first.encoding.as_deref(), Some("GBK"));
        assert_eq!(first.post_command.as_deref(), Some("ls -l"));
        assert_eq!(first.start_path.as_deref(), Some("/data/wwwroot/"));
        assert_eq!(
            first.private_key.as_deref(),
            Some("/home/alice/.ssh/id_ed25519")
        );
        assert_eq!(first.backspace.as_deref(), Some("ascii-del"));
        assert_eq!(first.delete.as_deref(), Some("escape-sequence"));

        // 空值键 → None（而非空串）
        let second = &servers[1];
        assert_eq!(second.group, None);
        assert_eq!(second.post_command, None);
        assert_eq!(second.private_key, None);
        assert_eq!(second.port, 22);
    }

    /// 节头四段是主数据源，键值与之冗余：键值以节头缺失时兜底为准
    /// （deepin 写全两处；这里验证节头解析独立成立）。
    #[test]
    fn server_section_header_is_the_primary_source() {
        let servers =
            parse_servers("[carol@192.0.2.10@22022@名字带@也不怕]\nName = 名字带@也不怕\n")
                .expect("解析");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].username, "carol");
        assert_eq!(servers[0].address, "192.0.2.10");
        assert_eq!(servers[0].port, 22022);
        assert_eq!(servers[0].name, "名字带@也不怕");
    }

    #[test]
    fn qsettings_escapes_are_restored() {
        let commands =
            parse_commands("[逗号分号反斜杠]\nCommand = a\\,b\\;c\\\\d\n").expect("解析");
        assert_eq!(commands[0].command, r"a,b;c\d");
        // 路径反斜杠原样保留（非 QSettings 逃逸）
        let commands = parse_commands("[路径]\nCommand = C:\\path\\to\n").expect("解析");
        assert_eq!(commands[0].command, r"C:\path\to");
    }

    #[test]
    fn malformed_lines_report_line_numbers() {
        let error = parse_commands("裸行没有等号\n").expect_err("应报错");
        assert_eq!(error.line, 1);
        assert!(error.message.contains("裸行"));

        let error = parse_servers("[user@host@不是端口@名]\n").expect_err("应报错");
        assert!(error.message.contains("端口"), "{error}");
    }

    /// 真实本机配置（若存在）必须可解析——不进库，只作冒烟。
    #[test]
    fn local_real_config_smoke_if_present() {
        let base = std::path::Path::new(
            std::env::var("HOME")
                .unwrap_or_else(|_| "/home/jii".into())
                .as_str(),
        )
        .join(".config/deepin/deepin-terminal");
        for (file, check) in [("command-config.conf", 0usize), ("server-config.conf", 0)] {
            let path = base.join(file);
            let Ok(text) = std::fs::read_to_string(&path) else {
                eprintln!("跳过（本机无 {}）", path.display());
                return;
            };
            let parsed = if check == 0 {
                parse_commands(&text)
                    .map(|list| list.len())
                    .map_err(|e| e.to_string())
            } else {
                unreachable!()
            };
            match parsed {
                Ok(count) => println!("{file}: {count} 条"),
                Err(message) => panic!("{file} 解析失败: {message}"),
            }
            // server 侧独立冒烟
            if file.starts_with("server") {
                let count = parse_servers(&text).expect("server 解析").len();
                println!("{file}: {count} 台");
            }
        }
    }
}
