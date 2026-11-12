//! 服务器连接配置：只存连接元数据，凭据（密码/口令/passphrase）一律入系统
//! 密钥环（laterterm-secrets），本模块与 settings.toml 里都不出现密钥本体。

use crate::commands::normalize_shortcut;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 单台服务器允许配置的最大隧道数（防手改配置炸出无界列表）。
pub const MAX_TUNNELS_PER_SERVER: usize = 16;

/// SSH 端口转发方向。
///
/// v1 只落 `local` / `remote` 两类：SOCKS5 动态转发 defer（russh 侧零增量，
/// 真正成本是本地 SOCKS5 握手协议实现 + settings.toml 无版本守卫的序列化
/// 边界，先钉稳两类的持久化口径，见 docs/roadmap.md 注记）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TunnelKind {
    /// 本地转发（`ssh -L`）：本机监听 bind，经目标机连 target。
    #[default]
    Local,
    /// 远程转发（`ssh -R`）：目标机侧监听 bind，回连本机侧 target。
    Remote,
}

/// 一条 SSH 端口转发隧道（`[[servers.tunnels]]`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerTunnel {
    /// 转发方向（kebab-case：`local` / `remote`）。
    pub kind: TunnelKind,
    /// 监听侧地址；载入/导入期缺省补 `127.0.0.1`。
    /// 远程转发建议配 IP 字面量：服务器回传的 connected_address 是其记录的
    /// 请求串，主机名形式可能不逐字回传导致路由未命中（通道被拒，不串线）。
    pub bind_host: String,
    /// 监听侧端口（1..=65535；0 在校验/归一化中视为坏值）。
    pub bind_port: u16,
    /// 目标主机（local = 目标机侧看到的目标；remote = 本机侧目标）。
    pub target_host: String,
    /// 目标端口（1..=65535）。
    pub target_port: u16,
    /// 是否随连接建立（停用的隧道仅保留配置不参与连接）。
    pub enabled: bool,
}

impl Default for ServerTunnel {
    fn default() -> Self {
        Self {
            kind: TunnelKind::Local,
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            target_host: String::new(),
            target_port: 0,
            enabled: true,
        }
    }
}

/// 类型级宽容解析：按 `toml::Value` 逐项解析，形状坏/未知 kind 的项直接丢弃。
///
/// 防手改 `kind = "dynamic"`（未来版本变体）或字段类型写错让**整份**
/// settings.toml 解析失败——`Settings::load` 解析失败即全量回默认，比丢
/// 条目更糟；这里宁可静默丢隧道也不炸文件。值级问题（空主机/坏端口）由
/// `normalize_tunnels` 具名回报。
fn lenient_tunnels<'de, D>(deserializer: D) -> Result<Vec<ServerTunnel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<toml::Value>::deserialize(deserializer)?;
    let mut tunnels = Vec::with_capacity(raw.len());
    let mut dropped = 0usize;
    for item in raw {
        match ServerTunnel::deserialize(item) {
            Ok(tunnel) => tunnels.push(tunnel),
            Err(_) => dropped += 1,
        }
    }
    if dropped > 0 {
        eprintln!("无效隧道已忽略 {dropped}（类型或 kind 未知，请检查配置）");
    }
    Ok(tunnels)
}

/// 隧道值级归一化（幂等）：载入（`Settings::load`→`sanitize_servers`）与
/// 导入（`ServerEntry::sanitized`，全部导入源唯一合并入口）双点挂载，两点
/// 重复执行无副作用。trim 各字段；bind_host trim 后为空补默认 `127.0.0.1`；
/// 空 target_host / 0 端口 / 含空白或控制字符的主机按具名 eprintln 丢弃——
/// **只丢隧道，绝不丢条目**（与 jump「宁断链不丢条目」同构，隧道字段绝不
/// 进 `validate_connection` 的丢弃路径）。
pub(crate) fn normalize_tunnels(server: &mut ServerEntry) {
    let name = server.name.clone();
    server.tunnels.retain_mut(|tunnel| {
        tunnel.bind_host = tunnel.bind_host.trim().to_string();
        tunnel.target_host = tunnel.target_host.trim().to_string();
        // 监听地址缺省：本地转发绑回环是最常见形态
        if tunnel.bind_host.is_empty() {
            tunnel.bind_host = "127.0.0.1".into();
        }
        let hosts_ok = [tunnel.bind_host.as_str(), tunnel.target_host.as_str()]
            .into_iter()
            .all(|host| {
                !host.is_empty()
                    && host.chars().count() <= 253
                    && !host.chars().any(|ch| ch.is_whitespace() || ch.is_control())
            });
        let ports_ok = tunnel.bind_port != 0 && tunnel.target_port != 0;
        if !hosts_ok || !ports_ok {
            eprintln!(
                "服务器 {:?} 的无效隧道已忽略（{}:{}→{}:{}）",
                name, tunnel.bind_host, tunnel.bind_port, tunnel.target_host, tunnel.target_port
            );
            return false;
        }
        true
    });
}

/// 认证方式元数据；密码/交互口令使用 `laterterm_secrets::ssh_account(name)`，
/// 私钥 passphrase 使用独立的 `laterterm_secrets::ssh_passphrase_account(name)`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerAuthKind {
    /// 密码认证（密码在密钥环）。
    #[default]
    Password,
    /// keyboard-interactive 认证（口令/动态码在密钥环，跳板机二次验证常见）。
    KeyboardInteractive,
    /// 私钥文件认证（passphrase 可选，在密钥环）。
    PrivateKey,
    /// SSH agent 认证。
    Agent,
}

/// 一台可连接的服务器。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerEntry {
    /// 展示名，同时是密钥环条目与列表的唯一键。
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: ServerAuthKind,
    /// `auth = private-key` 时的私钥文件路径；缺省或留空时按默认路径查找。
    pub key_path: Option<String>,
    /// 备注（纯展示）。
    pub note: String,
    /// 所属分组（扁平单层 TOML 元数据；`None` = 未分组）。
    /// 无父子指针：类型层面无法成环；允许 `/`（Xshell 目录树「生产/入口层」整串展示）。
    #[serde(default)]
    pub group: Option<String>,
    /// 应用内快捷键（ctrl+alt+k 一类），可选；统一注册表查重。
    #[serde(default)]
    pub shortcut: Option<String>,
    /// 会话编码；缺省 UTF-8。
    pub encoding: Option<String>,
    /// 跳板机：另一条条目的展示名（ProxyJump 按名引用）；`None` = 直连。
    /// 多跳链由 [`resolve_jump_chain`] 沿引用逐跳展开。
    #[serde(default)]
    pub jump: Option<String>,
    /// 该服务器的连接标签默认配色（"#RRGGBB"）；`None` = 跟随主题。
    /// 使用时经 `parse_hex_color` 解析，坏值按 None 忽略（不落主题语义）。
    #[serde(default)]
    pub tab_color: Option<String>,
    /// 端口转发隧道（`[[servers.tunnels]]`）。旧文件无此字段 → serde(default)
    /// 免迁移；类型级坏项经 lenient 解析丢弃，值级坏项载入/导入期归一化丢弃，
    /// 两层都绝不丢条目。
    #[serde(default, deserialize_with = "lenient_tunnels")]
    pub tunnels: Vec<ServerTunnel>,
}

impl Default for ServerEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            auth: ServerAuthKind::Password,
            key_path: None,
            note: String::new(),
            group: None,
            shortcut: None,
            encoding: None,
            jump: None,
            tab_color: None,
            tunnels: Vec::new(),
        }
    }
}

impl ServerEntry {
    /// UI 保存前严格校验连接信息和快捷键。
    pub fn validate(&self) -> Result<()> {
        self.validate_connection()?;
        if let Some(shortcut) = &self.shortcut {
            normalize_shortcut(shortcut).context("快捷键格式不合法")?;
        }
        // 跳板引用只做格式校验；存在性/环由调用方结合全表用 resolve_jump_chain 查
        // （validate_connection 不查 jump：它被载入/导入的「宁断链不丢条目」路径复用）。
        if let Some(jump) = &self.jump {
            let jump = jump.trim();
            if jump.is_empty() {
                bail!("跳板机名称不能为空（留空表示直连）");
            }
            validate_ref_name(jump).context("跳板机名称不合法")?;
        }
        // 分组走严格校验（与宽容归一化 normalize_group 成对，见其文档）
        if let Some(group) = &self.group {
            validate_group(group).context("分组不合法")?;
        }
        self.validate_tunnels()?;
        Ok(())
    }

    /// 隧道严格校验（UI 保存路径，报错留在弹窗内）：端口 1..=65535、主机
    /// 非空无空白/控制字符、条数封顶、(kind, bind_host, bind_port) 组内查重
    /// ——同条目内两条隧道抢同一监听位是配置错误；跨条目冲突只能连接期报
    /// （后连者失败），错误文案点名隧道定位。
    fn validate_tunnels(&self) -> Result<()> {
        if self.tunnels.len() > MAX_TUNNELS_PER_SERVER {
            bail!("隧道数量超过上限（最多 {MAX_TUNNELS_PER_SERVER} 条）");
        }
        let mut seen: Vec<(TunnelKind, &str, u16)> = Vec::new();
        for tunnel in &self.tunnels {
            if tunnel.bind_port == 0 || tunnel.target_port == 0 {
                bail!("隧道端口不能为 0（1..=65535）");
            }
            for (role, host) in [
                ("监听", tunnel.bind_host.as_str()),
                ("目标", tunnel.target_host.as_str()),
            ] {
                if host.is_empty() {
                    bail!("隧道{role}主机不能为空");
                }
                if host.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
                    bail!("隧道{role}主机不能包含空白或控制字符");
                }
                if host.chars().count() > 253 {
                    bail!("隧道{role}主机过长");
                }
            }
            let key = (tunnel.kind, tunnel.bind_host.as_str(), tunnel.bind_port);
            if seen.contains(&key) {
                bail!(
                    "隧道监听位重复（{} {}:{}）",
                    match tunnel.kind {
                        TunnelKind::Local => "本地",
                        TunnelKind::Remote => "远程",
                    },
                    tunnel.bind_host,
                    tunnel.bind_port
                );
            }
            seen.push(key);
        }
        Ok(())
    }

    /// 加载/导入旧配置时只校验连接元数据，避免因快捷键损坏丢失服务器。
    pub(crate) fn validate_connection(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            bail!("服务器名称不能为空");
        }
        if name.chars().count() > 80 {
            bail!("服务器名称过长");
        }
        if name.contains('/') || name.chars().any(char::is_control) {
            bail!("服务器名称不能包含 / 或控制字符");
        }
        let host = self.host.trim();
        if host.is_empty() {
            bail!("主机地址不能为空");
        }
        if host.len() > 253 {
            bail!("主机地址过长");
        }
        if host.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            bail!("主机地址不能包含空白或控制字符");
        }
        if self.port == 0 {
            bail!("端口不能为 0");
        }
        let username = self.username.trim();
        if username.is_empty() {
            bail!("登录用户名不能为空");
        }
        if username.len() > 64
            || username
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            bail!("用户名不能包含空白或超过 64 字符");
        }
        if let Some(key_path) = &self.key_path
            && (key_path.chars().count() > 1024 || key_path.chars().any(char::is_control))
        {
            bail!("私钥路径不合法");
        }
        if self.note.chars().count() > 200 {
            bail!("备注过长（最多 200 字）");
        }
        Ok(())
    }

    /// 导入前整理连接信息；保留历史快捷键原值，编辑时仍需通过 `validate`。
    ///
    /// 跳板引用格式非法（含 `/`、控制字符、超长）时置 `None` 而不是丢弃条目——
    /// 与载入期「宁断链不丢条目」同一口径；悬空/成环的全表检查由
    /// [`clear_broken_jump_refs`] 在合并完成后做。
    ///
    /// 隧道值级归一化在此挂载（normalize_tunnels 具名 eprintln 丢弃坏隧道，
    /// 不丢条目）：`sanitized` 是 LaterTerm/Deepin/Xshell 三导入源经
    /// `merge_entries_into` 的唯一合并入口，坏隧道不能存活到连接期才报错。
    pub fn sanitized(mut self) -> Option<Self> {
        self.name = self.name.trim().to_string();
        self.host = self.host.trim().to_string();
        self.username = self.username.trim().to_string();
        self.note = self.note.trim().to_string();
        self.key_path = self.key_path.take().map(|p| p.trim().to_string());
        self.jump = self
            .jump
            .take()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty() && validate_ref_name(name).is_ok());
        self.group = normalize_group(self.group.as_deref());
        normalize_tunnels(&mut self);
        if self.validate_connection().is_ok() {
            self.warn_invalid_shortcut();
            Some(self)
        } else {
            None
        }
    }

    pub(crate) fn warn_invalid_shortcut(&self) {
        if let Some(shortcut) = &self.shortcut
            && let Err(error) = normalize_shortcut(shortcut)
        {
            eprintln!(
                "服务器 {:?} 的快捷键无效，请在设置中修正: {:?}",
                self.name,
                error.to_string()
            );
        }
    }
}

/// 跳板链最大跳数；防手改配置成环把连接器拖死。
pub const MAX_JUMP_DEPTH: usize = 8;

/// 引用名（跳板机名）的格式校验，规则与服务器名一致：≤80 字、无 `/`、无控制字符。
fn validate_ref_name(name: &str) -> Result<()> {
    if name.chars().count() > 80 {
        bail!("引用名过长");
    }
    if name.contains('/') || name.chars().any(char::is_control) {
        bail!("引用名不能包含 / 或控制字符");
    }
    Ok(())
}

/// 分组的严格校验（UI 保存路径）：trim 后非空、≤80 字、无控制字符。
/// 与名称不同，分组允许 `/`（Xshell 目录树「生产/入口层」整串展示）。
fn validate_group(group: &str) -> Result<()> {
    let name = group.trim();
    if name.is_empty() {
        bail!("分组名不能为空");
    }
    if name.chars().count() > 80 {
        bail!("分组名过长（最多 80 字）");
    }
    if name.chars().any(char::is_control) {
        bail!("分组名不能包含控制字符");
    }
    Ok(())
}

/// 分组的宽容归一化（载入/导入路径，与 `validate_group` 严格报错成对）：
/// trim；trim 后为空、含控制字符或超长的分组整体降级 `None`——
/// 宁丢分组不丢条目，分组绝不进 `validate_connection` 的丢弃路径。
pub fn normalize_group(raw: Option<&str>) -> Option<String> {
    let name = raw?.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.to_string())
}

/// 跳板链缺陷（内部结构化形态，供错误文案与清理共用）。
///
/// `referrer` 指向「实际断链那一跳的引用方」条目：清理时只断它，
/// 链头等合法上游不被级联误清。
enum JumpDefect {
    /// 环（含自引）：成员按环序排列（如 `["A", "B"]` 表示 A→B→A）。
    Cycle(Vec<String>),
    /// 引用了不存在的跳板名：`jump` 为幽灵名，`referrer` 为引用它的条目。
    Missing { jump: String, referrer: String },
    /// 超过 [`MAX_JUMP_DEPTH`] 跳：`referrer` 为令链路溢出的那一跳的引用方。
    TooDeep { referrer: String },
}

/// 检查 `name` 的跳板链是否存在缺陷；无缺陷返回 `None`。
fn jump_defect(servers: &[ServerEntry], name: &str) -> Option<JumpDefect> {
    let find = |target: &str| servers.iter().find(|server| server.name == target);
    let target = find(name)?;
    let mut path = vec![target.name.clone()];
    let mut current = target;
    while let Some(jump_name) = current.jump.as_deref() {
        if let Some(start) = path.iter().position(|visited| visited == jump_name) {
            let mut cycle = path[start..].to_vec();
            cycle.push(jump_name.to_string());
            return Some(JumpDefect::Cycle(cycle));
        }
        let next = match find(jump_name) {
            Some(next) => next,
            // `?` 会把「名字不存在」误判为「无缺陷」，必须显式报 Missing；
            // 此刻 current 尚未推进，正是引用幽灵名的那一跳
            None => {
                return Some(JumpDefect::Missing {
                    jump: jump_name.to_string(),
                    referrer: current.name.clone(),
                });
            }
        };
        path.push(next.name.clone());
        if path.len() > MAX_JUMP_DEPTH + 1 {
            // current 尚未推进到 next：正是它引用的那一跳令链路溢出上限
            return Some(JumpDefect::TooDeep {
                referrer: current.name.clone(),
            });
        }
        current = next;
    }
    None
}

/// 从目标条目沿 `jump` 引用逐跳回溯，返回有序跳板链（由外到内，第 0 项最先连接，
/// `ssh -J` 语义）。
///
/// 自引、环、引用不存在的名字、超过 [`MAX_JUMP_DEPTH`] 跳都报错；
/// 环错误携带完整链路（如 `A→B→A`），便于用户定位手改坏的配置。
pub fn resolve_jump_chain<'a>(
    servers: &'a [ServerEntry],
    name: &str,
) -> Result<Vec<&'a ServerEntry>> {
    if !servers.iter().any(|server| server.name == name) {
        bail!("服务器「{name}」不存在");
    }
    let find = |target: &str| servers.iter().find(|server| server.name == target);
    match jump_defect(servers, name) {
        Some(JumpDefect::Cycle(cycle)) => bail!("跳板链形成环: {}", cycle.join("→")),
        Some(JumpDefect::Missing { jump, .. }) => {
            bail!("跳板机「{jump}」不存在");
        }
        Some(JumpDefect::TooDeep { .. }) => {
            bail!("跳板链过长（最多 {MAX_JUMP_DEPTH} 跳）");
        }
        None => {}
    }
    // 缺陷检查已通过，重新走一遍收集链路；引用方向是「到 X 要经谁」，
    // 回溯得到内→外，连接顺序是外→内，反转一次。
    let target = find(name).expect("上方已确认存在");
    let mut chain: Vec<&ServerEntry> = Vec::new();
    let mut current = target;
    while let Some(next) = current.jump.as_deref().and_then(find) {
        chain.push(next);
        current = next;
    }
    chain.reverse();
    Ok(chain)
}

/// 引用 `name` 作为跳板的条目名列表（删除阻断用）。
pub fn servers_referencing_jump<'a>(servers: &'a [ServerEntry], name: &str) -> Vec<&'a str> {
    servers
        .iter()
        .filter(|server| server.jump.as_deref() == Some(name))
        .map(|server| server.name.as_str())
        .collect()
}

/// 清理全表跳板引用：格式非法、悬空、成环、超深的引用一律置 `None`——
/// 宁断链不丢条目。返回被清理引用的条目名（供载入/导入回报计数）。
///
/// 载入（`Settings::load`→`sanitize_servers`）与导入合并两条路径都要调用，
/// 否则手改 TOML 的坏引用会一直存活到连接期才报错。
/// 环只断环上成员：路过环的条目在下一轮复检后保留有效链路。
/// 悬空/超深只断实际断链的那一跳（缺陷携带的 `referrer`）：
/// 链头沿合法上游走到断点的链路不被级联误清。
pub fn clear_broken_jump_refs(servers: &mut [ServerEntry]) -> Vec<String> {
    // 格式层：先记录格式非法条目，再统一 trim / 置 None
    let format_broken: Vec<String> = servers
        .iter()
        .filter(|server| {
            server.jump.as_deref().is_some_and(|name| {
                name.trim().is_empty() || validate_ref_name(name.trim()).is_err()
            })
        })
        .map(|server| server.name.clone())
        .collect();
    for server in servers.iter_mut() {
        server.jump = server
            .jump
            .take()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty() && validate_ref_name(name).is_ok());
    }
    // 链路层：循环到不动点。环清环上成员，悬空/超深清各自缺陷携带的
    // referrer（断链那一跳的引用方）；路过环的条目在环成员断链后复检通过，
    // 合法上游也不因下游断链被级联误伤。
    let mut cleared = format_broken;
    loop {
        let defects: Vec<JumpDefect> = servers
            .iter()
            .filter_map(|server| jump_defect(servers, &server.name))
            .collect();
        if defects.is_empty() {
            break;
        }
        let mut touched = false;
        for defect in defects {
            let victims = match defect {
                JumpDefect::Cycle(members) => members,
                JumpDefect::Missing { referrer, .. } | JumpDefect::TooDeep { referrer } => {
                    vec![referrer]
                }
            };
            for victim in victims {
                if let Some(server) = servers.iter_mut().find(|server| server.name == victim)
                    && server.jump.take().is_some()
                    && !cleared.contains(&server.name)
                {
                    cleared.push(server.name.clone());
                    touched = true;
                }
            }
        }
        if !touched {
            break;
        }
    }
    cleared
}

/// 服务器导入/导出文件格式。只含连接元数据——凭据在系统密钥环里，永不落盘；
/// 导入后的条目首次连接时补输凭据（可勾选记住）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServersFile {
    /// 文件格式版本；缺省按当前版本解析（向前兼容旧文件）。
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub servers: Vec<ServerEntry>,
}

impl ServersFile {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(servers: Vec<ServerEntry>) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            servers,
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self).context("序列化服务器导出文件失败")?;
        std::fs::write(path, body).with_context(|| format!("写入 {} 失败", path.display()))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let mut file: Self =
            toml::from_str(&body).context("解析服务器文件失败（需 [[servers]] 格式）")?;
        if file.version > Self::CURRENT_VERSION {
            bail!(
                "文件版本 v{} 高于当前支持 v{}，请升级 LaterTerm",
                file.version,
                Self::CURRENT_VERSION
            );
        }
        file.version = Self::CURRENT_VERSION;
        Ok(file)
    }
}

use std::path::PathBuf;

/// 认证方式（持久化形态；连接时映射为 laterterm_ssh::SshAuth）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredAuthKind {
    /// ssh-agent（SSH_AUTH_SOCK）。
    Agent,
    /// 私钥文件路径。
    KeyFile(String),
    /// 系统密钥环存密码（account = [`password_account`]）。
    Keyring,
}

/// 一台远程服务器。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredServerEntry {
    /// 显示名（唯一标识，重名保存时后者覆盖前者）。
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub auth: StoredAuthKind,
    /// 终端编码（`"UTF-8"` / `"GBK"` / `"GB18030"`；缺省 UTF-8）。
    #[serde(default)]
    pub encoding: Option<String>,
}

fn default_port() -> u16 {
    22
}

/// 密码在系统密钥环里的 account 命名（与 TOTP 的命名同风格）。
pub fn password_account(name: &str) -> String {
    format!("laterterm-server-{name}")
}

/// 服务器列表（servers.toml 的读写单元）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStore {
    pub servers: Vec<StoredServerEntry>,
}

impl ServerStore {
    /// 从配置目录读（文件缺失/损坏回落空表，损坏不致命——面板可重建）。
    pub fn load() -> Self {
        let Some(path) = servers_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    /// 落盘（同名校验由调用方做，这里保序全量写）。
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = servers_path() else {
            return Err("无法定位配置目录（缺 HOME）".into());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }

    /// 按名取条目。
    pub fn get(&self, name: &str) -> Option<&StoredServerEntry> {
        self.servers.iter().find(|server| server.name == name)
    }

    /// 插入或按名覆盖（重命名语义 = 先 remove 再 upsert）。
    pub fn upsert(&mut self, entry: StoredServerEntry) {
        match self
            .servers
            .iter()
            .position(|server| server.name == entry.name)
        {
            Some(index) => self.servers[index] = entry,
            None => self.servers.push(entry),
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|server| server.name != name);
        before != self.servers.len()
    }
}

fn servers_path() -> Option<PathBuf> {
    crate::app_config_dir().map(|dir| dir.join("servers.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    #[test]
    fn legacy_migration_is_once_and_does_not_replace_current_servers() {
        let legacy = ServerStore {
            servers: vec![StoredServerEntry {
                name: "legacy".into(),
                host: "old.example".into(),
                port: 22,
                username: "ops".into(),
                auth: StoredAuthKind::Keyring,
                encoding: None,
            }],
        };
        let mut settings = Settings::default();
        assert!(settings.migrate_legacy_servers(legacy.clone()));
        assert!(settings.legacy_servers_migrated);
        assert_eq!(settings.servers.len(), 1);
        settings.remove_server("legacy");
        assert!(!settings.migrate_legacy_servers(legacy.clone()));
        assert!(settings.servers.is_empty(), "删除后不能被旧文件补回");
        let mut settings = Settings::default();
        settings.servers.push(entry("legacy", "new.example"));
        assert!(settings.migrate_legacy_servers(legacy));
        assert_eq!(settings.servers[0].host, "new.example");
    }

    fn entry(name: &str, host: &str) -> ServerEntry {
        ServerEntry {
            name: name.into(),
            host: host.into(),
            username: "ops".into(),
            ..Default::default()
        }
    }

    #[test]
    fn tab_color_roundtrip() {
        let mut server = entry("生产入口", "prod.example");
        server.tab_color = Some("#FF5500".into());
        let json = serde_json::to_string(&server).expect("序列化");
        assert!(
            json.contains("\"#FF5500\""),
            "色值应为十六进制字符串: {json}"
        );
        let back: ServerEntry = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.tab_color.as_deref(), Some("#FF5500"));
    }

    #[test]
    fn missing_tab_color_defaults_to_none() {
        // 旧版 servers.toml 无 tab_color 键 → serde(default) 回落 None，不拒载
        let server = entry("日志机", "log.example");
        assert_eq!(server.tab_color, None);
        let json = serde_json::to_string(&server).expect("序列化");
        let back: ServerEntry = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.tab_color, None);
    }

    #[test]
    fn validates_required_fields() {
        assert!(entry("跳板机", "10.0.0.1").validate().is_ok());
        assert!(entry("", "10.0.0.1").validate().is_err());
        assert!(entry("a/b", "10.0.0.1").validate().is_err());
        assert!(entry("主机为空", "").validate().is_err());

        let zero_port = ServerEntry {
            name: "p".into(),
            host: "h".into(),
            port: 0,
            ..Default::default()
        };
        assert!(zero_port.validate().is_err());
    }

    #[test]
    fn private_key_auth_accepts_default_key_path() {
        for key_path in [None, Some(String::new()), Some("   ".into())] {
            let server = ServerEntry {
                auth: ServerAuthKind::PrivateKey,
                key_path,
                ..entry("默认私钥", "example.com")
            };
            assert!(
                server.validate().is_ok(),
                "私钥路径 {:?} 留空时应允许使用默认路径",
                server.key_path
            );
            assert!(server.sanitized().is_some());
        }
    }

    #[test]
    fn private_key_path_keeps_length_and_control_character_guards() {
        let mut server = ServerEntry {
            auth: ServerAuthKind::PrivateKey,
            key_path: Some("钥".repeat(1024)),
            ..entry("私钥", "example.com")
        };
        assert!(server.validate().is_ok());
        for path in ["钥".repeat(1025), "id\nkey".into(), "id\0key".into()] {
            server.key_path = Some(path);
            assert!(server.validate().is_err());
        }
    }

    #[test]
    fn settings_roundtrip_keeps_private_key_servers_without_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            r#"
[[servers]]
name = "默认私钥"
host = "default.example.com"
username = "ops"
auth = "private-key"

[[servers]]
name = "空路径"
host = "empty.example.com"
username = "ops"
auth = "private-key"
key_path = "   "
"#,
        )
        .unwrap();
        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings.servers.len(), 2, "默认私钥条目不能在加载时被丢弃");
        for server in &settings.servers {
            assert_eq!(server.auth, ServerAuthKind::PrivateKey);
            assert_eq!(server.key_path, None);
            server.validate().unwrap();
        }
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
    }

    #[test]
    fn settings_server_crud_replaces_and_removes() {
        let mut settings = Settings::default();
        settings.upsert_server(entry("a", "10.0.0.1")).unwrap();
        settings.upsert_server(entry("a", "10.0.0.2")).unwrap();
        assert_eq!(settings.servers.len(), 1);
        assert_eq!(settings.servers[0].host, "10.0.0.2");
        assert!(settings.remove_server("a"));
        assert!(settings.servers.is_empty());
        assert!(!settings.remove_server("a"));
    }

    #[test]
    fn toml_roundtrip_keeps_auth_kind_and_optional_fields() {
        let raw = r#"
[[servers]]
name = "内网机"
host = "192.168.1.10"
port = 2222
username = "ops"
auth = "keyboard-interactive"
note = "老机房"
group = "生产/入口层"

[[servers]]
name = "跳板"
host = "jump.example.com"
username = "root"
auth = "private-key"
key_path = "/home/me/.ssh/id_ed25519"
"#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.servers.len(), 2);
        assert_eq!(
            settings.servers[0].auth,
            ServerAuthKind::KeyboardInteractive
        );
        assert_eq!(settings.servers[0].port, 2222);
        assert_eq!(
            settings.servers[1].key_path.as_deref(),
            Some("/home/me/.ssh/id_ed25519")
        );
        // 分组字段往返保留
        assert_eq!(settings.servers[0].group.as_deref(), Some("生产/入口层"));
        assert_eq!(settings.servers[1].group, None);
        // 缺省 port/auth 走默认值；旧文件缺 group/group/jump 字段解析为 None
        let legacy: Settings =
            toml::from_str("[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n").unwrap();
        assert_eq!(legacy.servers[0].port, 22);
        assert_eq!(legacy.servers[0].auth, ServerAuthKind::Password);
        assert_eq!(legacy.servers[0].group, None);
        assert!(settings.servers[0].validate().is_ok());
    }

    #[test]
    fn sanitize_drops_unsalvageable_and_trims_rest() {
        let raw = r#"
[[servers]]
name = "  跳板  "
host = " jump.example.com "
username = " root "
note = "ok"
group = " 生产 "

[[servers]]
name = "  "
host = "h"
username = "u"

[[servers]]
name = "分组坏了"
host = "h3"
username = "u3"
group = "坏\n组"
"#;
        let mut settings: Settings = toml::from_str(raw).unwrap();
        settings.sanitize_servers();
        assert_eq!(settings.servers.len(), 2);
        assert_eq!(settings.servers[0].name, "跳板");
        assert_eq!(settings.servers[0].host, "jump.example.com");
        assert_eq!(settings.servers[0].username, "root");
        assert_eq!(settings.servers[0].group.as_deref(), Some("生产"));
        // 手改坏的分组降级 None，条目本身保留
        assert_eq!(settings.servers[1].name, "分组坏了");
        assert_eq!(settings.servers[1].group, None);
    }

    #[test]
    fn servers_file_roundtrip_and_version_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers-export.toml");
        let file = ServersFile::new(vec![ServerEntry {
            name: "跳板".into(),
            host: "jump.example.com".into(),
            port: 2222,
            username: "ops".into(),
            auth: ServerAuthKind::KeyboardInteractive,
            key_path: None,
            note: "导出测试".into(),
            group: Some("生产".into()),
            shortcut: Some("ctrl+alt+1".into()),
            encoding: None,
            jump: None,
            tab_color: None,
            tunnels: Vec::new(),
        }]);
        file.save_to(&path).unwrap();
        let loaded = ServersFile::load_from(&path).unwrap();
        assert_eq!(loaded.version, ServersFile::CURRENT_VERSION);
        assert_eq!(loaded.servers, file.servers);
        assert_eq!(loaded.servers[0].group.as_deref(), Some("生产"));
        assert!(loaded.servers[0].validate().is_ok());

        // 缺 version 的旧文件按当前版本解析
        let legacy: ServersFile =
            toml::from_str("[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n").unwrap();
        assert_eq!(legacy.version, 0);
        assert_eq!(legacy.servers.len(), 1);

        // 未来版本拒绝导入（version 必须在 [[servers]] 之前的文件顶层）
        let future = "version = 99\n[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n";
        std::fs::write(&path, future).unwrap();
        assert!(ServersFile::load_from(&path).is_err());
    }

    #[test]
    fn entry_sanitized_trims_and_drops_invalid() {
        let ok = ServerEntry {
            name: " 跳板 ".into(),
            host: " h ".into(),
            username: " ops ".into(),
            ..Default::default()
        }
        .sanitized()
        .unwrap();
        assert_eq!(ok.name, "跳板");
        assert_eq!(ok.host, "h");
        assert_eq!(ok.username, "ops");
        assert!(ServerEntry::default().sanitized().is_none(), "空条目丢弃");
    }

    // ---------- 分组（扁平单层）严格/宽容成对口径 ----------

    #[test]
    fn group_validate_strict_and_normalize_forgiving_pair_up() {
        let mut server = entry("跳板", "10.0.0.1");
        // 允许 `/` 与首尾空白（保存路径会先 trim 再进 validate）
        server.group = Some(" 生产/入口层 ".into());
        assert!(server.validate().is_ok());
        for bad in ["   ".to_string(), "坏\n组".to_string(), "长".repeat(81)] {
            server.group = Some(bad.clone());
            assert!(server.validate().is_err(), "{bad:?} 分组应严格报错");
        }
        // sanitized（宽容路径）：trim 保留合法分组；坏分组降级 None，条目保留
        server.group = Some("  生产  ".into());
        assert_eq!(
            server.clone().sanitized().unwrap().group.as_deref(),
            Some("生产")
        );
        for raw in [
            Some("   ".to_string()),
            Some("坏\u{0}组".to_string()),
            Some("长".repeat(81)),
            None,
        ] {
            server.group = raw.clone();
            let sanitized = server.clone().sanitized().unwrap();
            assert_eq!(sanitized.group, None, "{raw:?} 应归一化为 None");
            assert!(
                sanitized.validate_connection().is_ok(),
                "分组问题绝不丢条目"
            );
        }
        // normalize_group 直查：None 入 None 出
        assert_eq!(normalize_group(None), None);
        assert_eq!(normalize_group(Some("  A  ")), Some("A".into()));
    }

    // ---------- 跳板链（ProxyJump）纯逻辑 ----------

    fn jump_entry(name: &str, jump: Option<&str>) -> ServerEntry {
        ServerEntry {
            name: name.into(),
            host: "10.0.0.1".into(),
            username: "ops".into(),
            jump: jump.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_jump_chain_orders_hops_outer_to_inner() {
        // 目标 T → 跳板 inner → 跳板 outer；连接顺序应为 outer 最先
        let servers = vec![
            jump_entry("T", Some("inner")),
            jump_entry("inner", Some("outer")),
            jump_entry("outer", None),
        ];
        let chain = resolve_jump_chain(&servers, "T").unwrap();
        let names: Vec<&str> = chain.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["outer", "inner"]);
        // 单跳与直连
        assert_eq!(
            resolve_jump_chain(&servers, "inner")
                .unwrap()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["outer"]
        );
        assert!(resolve_jump_chain(&servers, "outer").unwrap().is_empty());
    }

    #[test]
    fn resolve_jump_chain_rejects_self_loop_missing_and_depth() {
        let servers = vec![
            jump_entry("自引", Some("自引")),
            jump_entry("A", Some("B")),
            jump_entry("B", Some("A")),
            jump_entry("悬空", Some("不存在")),
            jump_entry("T", Some("A")),
        ];
        // 自引：环链路 A→A
        let error = resolve_jump_chain(&servers, "自引")
            .unwrap_err()
            .to_string();
        assert!(error.contains("环"), "{error}");
        assert!(error.contains("自引→自引"), "{error}");
        // 双节点环：A→B→A
        let error = resolve_jump_chain(&servers, "A").unwrap_err().to_string();
        assert!(error.contains("A→B→A"), "{error}");
        // 缺失引用
        let error = resolve_jump_chain(&servers, "悬空")
            .unwrap_err()
            .to_string();
        assert!(error.contains("不存在"), "{error}");
        assert!(error.contains("「不存在」"), "{error}");
        // 目标本身不存在
        assert!(resolve_jump_chain(&servers, "幽灵").is_err());
        // 深度超限（9 跳 > 8）
        let mut deep = vec![jump_entry("d0", Some("d1"))];
        for hop in 1..=9 {
            deep.push(jump_entry(
                &format!("d{hop}"),
                Some(&format!("d{}", hop + 1)),
            ));
        }
        let error = resolve_jump_chain(&deep, "d0").unwrap_err().to_string();
        assert!(error.contains("跳板链过长"), "{error}");
    }

    #[test]
    fn servers_referencing_jump_lists_referrers_only() {
        let servers = vec![
            jump_entry("a", Some("jump")),
            jump_entry("b", Some("jump")),
            jump_entry("c", Some("其他")),
            jump_entry("jump", None),
        ];
        assert_eq!(servers_referencing_jump(&servers, "jump"), vec!["a", "b"]);
        assert!(servers_referencing_jump(&servers, "无").is_empty());
    }

    #[test]
    fn clear_broken_jump_refs_keeps_entries_breaks_links() {
        let mut servers = vec![
            jump_entry("好的", Some("跳板")),
            jump_entry("跳板", None),
            // 悬空引用
            jump_entry("悬空", Some("幽灵")),
            // 自引成环
            jump_entry("自引", Some("自引")),
            // 格式非法（含 /）
            jump_entry("脏名", Some("pro d/uct")),
        ];
        let cleared = clear_broken_jump_refs(&mut servers);
        assert_eq!(cleared.len(), 3);
        for name in ["悬空", "自引", "脏名"] {
            assert!(
                cleared.iter().any(|item| item == name),
                "{cleared:?} 应含 {name}"
            );
        }
        assert_eq!(servers.len(), 5, "宁断链不丢条目");
        assert_eq!(servers[0].jump.as_deref(), Some("跳板"));
        assert_eq!(servers[1].jump, None);
        for broken in ["悬空", "自引", "脏名"] {
            let entry = servers.iter().find(|entry| entry.name == broken).unwrap();
            assert_eq!(entry.jump, None, "{broken} 的引用应被清空");
        }
    }

    #[test]
    fn clear_broken_jump_refs_breaks_cycle_between_two_entries() {
        let mut servers = vec![
            jump_entry("A", Some("B")),
            jump_entry("B", Some("A")),
            jump_entry("旁观", Some("A")),
        ];
        let cleared = clear_broken_jump_refs(&mut servers);
        // A、B 成环都被断链；旁观→A 在 A 断链后是否悬空不影响本函数判定
        // （顺序敏感：断的是环上条目，旁观条目保持原引用，其链路下一轮载入会再查）
        assert_eq!(cleared.len(), 2);
        assert_eq!(servers[2].jump.as_deref(), Some("A"));
    }

    #[test]
    fn clear_broken_jump_refs_only_cuts_the_broken_hop_not_legal_upstream() {
        // 回归钉子（2026-11-06）：A→B 合法、B→幽灵 断链。旧实现 victims 取
        // 链头 name，A 会被级联误清并静默降级直连；定点清理只断引用方 B。
        let mut servers = vec![jump_entry("A", Some("B")), jump_entry("B", Some("幽灵"))];
        let cleared = clear_broken_jump_refs(&mut servers);
        assert_eq!(cleared, vec!["B"], "只断实际断链那一跳的引用方");
        assert_eq!(
            servers[0].jump.as_deref(),
            Some("B"),
            "合法上游 A 不被级联误清"
        );
        let chain: Vec<&str> = resolve_jump_chain(&servers, "A")
            .unwrap()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(chain, vec!["B"], "A 经 B 的链路清理后仍可解析");
    }

    #[test]
    fn clear_broken_jump_refs_trims_deep_chains_at_the_overflow_hop() {
        // T→d1..d12 共 12 跳（d1..d11 各跳下一个、d12 直连），上限 8 跳。
        // 各条目视角的溢出跳引用方不同：T 链裁 d8、d1 链裁 d9、d2 链裁 d10、
        // d3 链裁 d11（单轮裁剪到位）；下一轮复检无缺陷即退出。
        let mut servers = vec![jump_entry("T", Some("d1"))];
        for hop in 1..=11 {
            servers.push(jump_entry(
                &format!("d{hop}"),
                Some(&format!("d{}", hop + 1)),
            ));
        }
        servers.push(jump_entry("d12", None));
        let cleared = clear_broken_jump_refs(&mut servers);
        let mut expected = vec!["d8", "d9", "d10", "d11"];
        expected.sort_unstable();
        let mut actual = cleared.clone();
        actual.sort_unstable();
        assert_eq!(actual, expected, "每条各自溢出的链路各裁自己的引用方");
        assert_eq!(
            servers[0].jump.as_deref(),
            Some("d1"),
            "链头 T 不被级联误清"
        );
        // T 的链路裁剪后恰好收敛到上限：d1..d8 共 8 跳
        assert_eq!(
            resolve_jump_chain(&servers, "T").unwrap().len(),
            MAX_JUMP_DEPTH
        );
        for name in ["T", "d1", "d7", "d8", "d9", "d10", "d11", "d12"] {
            let hops = resolve_jump_chain(&servers, name).unwrap().len();
            assert!(
                hops <= MAX_JUMP_DEPTH,
                "{name} 清理后应可解析且 ≤{MAX_JUMP_DEPTH} 跳，实际 {hops}"
            );
        }
    }

    #[test]
    fn sanitize_servers_returns_cleared_jump_refs_for_reporting() {
        // 回报契约：sanitize_servers 返回被清条目名清单（Settings::load 据此
        // eprintln 回报）；eprintln 传输本身不做 stdout 断言。
        let mut settings = Settings {
            servers: vec![jump_entry("内网机", Some("幽灵")), jump_entry("直连", None)],
            ..Default::default()
        };
        assert_eq!(settings.sanitize_servers(), vec!["内网机"]);
        assert_eq!(settings.servers[0].jump, None);
        assert_eq!(settings.servers[1].jump, None, "正常条目不动");
    }

    #[test]
    fn validate_rejects_bad_jump_format_but_validate_connection_ignores_jump() {
        let mut server = jump_entry("自身", Some("自身名"));
        server.name = "自身名".into();
        // validate（UI 严格路径）管格式；自引的存在性错误由 resolve_jump_chain 报
        assert!(server.validate().is_ok());
        for bad in ["a/b".to_string(), "坏\n名".to_string(), "长".repeat(81)] {
            server.jump = Some(bad);
            assert!(server.validate().is_err(), "{:?} 应格式非法", server.jump);
        }
        // validate_connection（载入/导入宽松路径）不查 jump：脏引用不丢条目
        server.jump = Some("a/b".into());
        assert!(server.validate_connection().is_ok());
        // sanitized 清格式非法引用、trim 合法引用
        server.jump = Some("  好/坏  ".into());
        assert_eq!(server.clone().sanitized().unwrap().jump, None);
        server.jump = Some("  跳板  ".into());
        assert_eq!(
            server.clone().sanitized().unwrap().jump.as_deref(),
            Some("跳板")
        );
    }

    #[test]
    fn toml_roundtrip_keeps_jump_and_legacy_files_still_load() {
        let raw = r#"
[[servers]]
name = "内网机"
host = "192.168.1.10"
username = "ops"
jump = "跳板"

[[servers]]
name = "跳板"
host = "jump.example.com"
username = "root"
"#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.servers[0].jump.as_deref(), Some("跳板"));
        assert_eq!(settings.servers[1].jump, None);
        let chain = resolve_jump_chain(&settings.servers, "内网机").unwrap();
        assert_eq!(chain[0].name, "跳板");

        // 旧文件无 jump 字段照常解析（serde 缺省 None）
        let legacy: Settings =
            toml::from_str("[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n").unwrap();
        assert_eq!(legacy.servers[0].jump, None);

        // 往返保留
        let text = toml::to_string_pretty(&settings).unwrap();
        let reloaded: Settings = toml::from_str(&text).unwrap();
        assert_eq!(reloaded.servers, settings.servers);
    }

    #[test]
    fn load_breaks_dangling_jump_refs_instead_of_dropping_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            r#"
[[servers]]
name = "内网机"
host = "192.168.1.10"
username = "ops"
jump = "幽灵"

[[servers]]
name = "正常"
host = "10.0.0.9"
username = "ops"
"#,
        )
        .unwrap();
        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings.servers.len(), 2, "悬空引用不能丢条目");
        assert_eq!(settings.servers[0].jump, None, "悬空引用载入期即断链");
        assert_eq!(settings.servers[0].name, "内网机");
    }

    // ---------- 端口转发隧道（local/remote）序列化与宽容口径 ----------

    fn tunnel(
        kind: TunnelKind,
        bind_host: &str,
        bind_port: u16,
        target: &str,
        target_port: u16,
    ) -> ServerTunnel {
        ServerTunnel {
            kind,
            bind_host: bind_host.into(),
            bind_port,
            target_host: target.into(),
            target_port,
            enabled: true,
        }
    }

    #[test]
    fn tunnels_toml_roundtrip_keeps_kind_and_fields() {
        let mut server = entry("内网机", "10.0.0.1");
        server.tunnels = vec![
            tunnel(TunnelKind::Local, "127.0.0.1", 8080, "db.internal", 5432),
            tunnel(TunnelKind::Remote, "0.0.0.0", 8443, "127.0.0.1", 443),
        ];
        let text = toml::to_string_pretty(&server).unwrap();
        assert!(text.contains("[[tunnels]]"), "{text}");
        assert!(text.contains("kind = \"local\""), "{text}");
        assert!(text.contains("kind = \"remote\""), "{text}");
        let back: ServerEntry = toml::from_str(&text).unwrap();
        assert_eq!(back.tunnels, server.tunnels);
    }

    #[test]
    fn legacy_files_without_tunnels_load_with_empty_list() {
        // 旧版 settings.toml 无 [[servers.tunnels]] → serde(default) 空表，不拒载
        let legacy: Settings =
            toml::from_str("[[servers]]\nname = 'x'\nhost = 'h'\nusername = 'u'\n").unwrap();
        assert!(legacy.servers[0].tunnels.is_empty());
        assert!(legacy.servers[0].validate().is_ok());
    }

    #[test]
    fn lenient_tunnels_drops_bad_items_instead_of_failing_the_file() {
        // 手改出未知 kind（未来版本变体）必须只丢该隧道，绝不能让整份
        // settings.toml 解析失败触发全量回默认
        let raw = r#"
[[servers]]
name = "内网机"
host = "10.0.0.1"
username = "ops"

[[servers.tunnels]]
kind = "local"
bind_host = "127.0.0.1"
bind_port = 8080
target_host = "db"
target_port = 5432

[[servers.tunnels]]
kind = "dynamic"
bind_host = "127.0.0.1"
bind_port = 1080
"#;
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.servers.len(), 1, "坏隧道不丢条目");
        assert_eq!(
            settings.servers[0].tunnels.len(),
            1,
            "未知 kind 的隧道被丢弃"
        );
        assert_eq!(settings.servers[0].tunnels[0].target_host, "db");
    }

    #[test]
    fn normalize_tunnels_trims_fills_default_bind_and_drops_bad_values() {
        let mut server = entry("内网机", "10.0.0.1");
        server.tunnels = vec![
            // 空监听地址 → 补 127.0.0.1；首尾空白 trim
            ServerTunnel {
                bind_host: "  ".into(),
                bind_port: 8080,
                target_host: " db ".into(),
                target_port: 5432,
                ..Default::default()
            },
            // 空 target_host → 丢
            tunnel(TunnelKind::Local, "127.0.0.1", 8081, "  ", 5433),
            // 0 端口 → 丢
            tunnel(TunnelKind::Local, "127.0.0.1", 0, "db", 5434),
            // 含空白的主机 → 丢
            tunnel(TunnelKind::Local, "127.0.0.1", 8082, "bad host", 5435),
        ];
        normalize_tunnels(&mut server);
        assert_eq!(server.tunnels.len(), 1, "只丢坏隧道，不丢条目");
        assert_eq!(
            server.tunnels[0].bind_host, "127.0.0.1",
            "空 bind_host 补默认"
        );
        assert_eq!(server.tunnels[0].target_host, "db", "target_host 应 trim");
        assert!(server.validate_connection().is_ok(), "隧道问题绝不丢条目");
        // 幂等：归一化结果再跑一遍不变
        let once = server.tunnels.clone();
        normalize_tunnels(&mut server);
        assert_eq!(server.tunnels, once);
    }

    #[test]
    fn sanitized_import_path_normalizes_tunnels_without_dropping_entry() {
        // 导入路径（sanitized → merge_entries_into 的唯一合并入口）同样只丢隧道
        let mut server = entry(" 内网机 ", "10.0.0.1");
        server.tunnels = vec![
            tunnel(TunnelKind::Local, "127.0.0.1", 8080, "db", 5432),
            tunnel(TunnelKind::Remote, "0.0.0.0", 9999, "", 80),
        ];
        let sanitized = server.sanitized().expect("条目本身合法必须保留");
        assert_eq!(sanitized.name, "内网机");
        assert_eq!(sanitized.tunnels.len(), 1, "坏隧道导入期即丢弃");
        assert_eq!(sanitized.tunnels[0].target_host, "db");
    }

    #[test]
    fn validate_tunnels_rejects_bad_ports_hosts_duplicates_and_overflow() {
        let mut server = entry("内网机", "10.0.0.1");
        // 0 端口
        server.tunnels = vec![tunnel(TunnelKind::Local, "127.0.0.1", 0, "db", 5432)];
        assert!(server.validate().is_err());
        // 空/带空白目标主机
        for bad_target in ["", "bad host", "坏\n主机"] {
            server.tunnels = vec![tunnel(
                TunnelKind::Local,
                "127.0.0.1",
                8080,
                bad_target,
                5432,
            )];
            assert!(server.validate().is_err(), "{bad_target:?} 应报错");
        }
        // 同 (kind, bind_host, bind_port) 查重
        server.tunnels = vec![
            tunnel(TunnelKind::Local, "127.0.0.1", 8080, "db", 5432),
            tunnel(TunnelKind::Local, "127.0.0.1", 8080, "web", 80),
        ];
        assert!(server.validate().is_err(), "同监听位重复应报错");
        // 同端口不同 kind / 不同 bind_host 合法共存
        server.tunnels = vec![
            tunnel(TunnelKind::Local, "127.0.0.1", 8080, "db", 5432),
            tunnel(TunnelKind::Remote, "127.0.0.1", 8080, "web", 80),
            tunnel(TunnelKind::Local, "0.0.0.0", 8080, "db", 5433),
        ];
        assert!(server.validate().is_ok());
        // 条数封顶
        server.tunnels = (0..=MAX_TUNNELS_PER_SERVER)
            .map(|index| {
                tunnel(
                    TunnelKind::Local,
                    "127.0.0.1",
                    9000 + index as u16,
                    "db",
                    5432,
                )
            })
            .collect();
        assert!(
            server.validate().is_err(),
            "超过 {MAX_TUNNELS_PER_SERVER} 条应报错"
        );
    }

    #[test]
    fn load_drops_bad_tunnels_and_keeps_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            r#"
[[servers]]
name = "内网机"
host = "10.0.0.1"
username = "ops"

[[servers.tunnels]]
bind_port = 8080
target_host = "db"
target_port = 5432

[[servers.tunnels]]
bind_port = 0
target_host = "坏"
target_port = 1
"#,
        )
        .unwrap();
        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings.servers.len(), 1, "坏隧道不丢条目");
        // bind_host 缺省补 127.0.0.1；0 端口坏值载入期被归一化丢弃
        assert_eq!(settings.servers[0].tunnels.len(), 1);
        assert_eq!(settings.servers[0].tunnels[0].bind_host, "127.0.0.1");
    }

    #[test]
    fn retarget_jump_refs_rewrites_only_matching_names() {
        let mut settings = Settings {
            servers: vec![
                jump_entry("引用方", Some("旧名")),
                jump_entry("旁人", Some("别人")),
                jump_entry("旧名", None),
            ],
            ..Default::default()
        };
        settings.retarget_jump_refs("旧名", "新名");
        assert_eq!(settings.servers[0].jump.as_deref(), Some("新名"));
        assert_eq!(settings.servers[1].jump.as_deref(), Some("别人"));
        // 改写后链路应可解析（重命名语义闭环）
        settings.servers[2].name = "新名".into();
        assert_eq!(
            resolve_jump_chain(&settings.servers, "引用方").unwrap()[0].name,
            "新名"
        );
    }
}

impl From<StoredServerEntry> for ServerEntry {
    fn from(entry: StoredServerEntry) -> Self {
        let (auth, key_path) = match entry.auth {
            StoredAuthKind::Agent => (ServerAuthKind::Agent, None),
            StoredAuthKind::KeyFile(path) => (ServerAuthKind::PrivateKey, Some(path)),
            StoredAuthKind::Keyring => (ServerAuthKind::Password, None),
        };
        Self {
            name: entry.name,
            host: entry.host,
            port: entry.port,
            username: entry.username,
            auth,
            key_path,
            encoding: entry.encoding,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn main_servers_convert_without_losing_key_or_encoding() {
        let stored = StoredServerEntry {
            name: "旧服务器".into(),
            host: "host".into(),
            port: 2222,
            username: "ops".into(),
            auth: StoredAuthKind::KeyFile("/tmp/id".into()),
            encoding: Some("GBK".into()),
        };
        let entry = ServerEntry::from(stored);
        assert_eq!(entry.auth, ServerAuthKind::PrivateKey);
        assert_eq!(entry.key_path.as_deref(), Some("/tmp/id"));
        assert_eq!(entry.encoding.as_deref(), Some("GBK"));
        assert!(entry.validate().is_ok());
        let file = ServersFile::new(vec![entry]);
        let text = toml::to_string(&file).unwrap();
        assert_eq!(toml::from_str::<ServersFile>(&text).unwrap(), file);
    }
}
