//! laterterm-config — LaterTerm 设置持久化。
//!
//! 设置以 TOML 存于用户配置目录（Linux 为 `~/.config/laterterm/settings.toml`）。
//! 设计取舍：
//! - 字段全部 `#[serde(default)]`：文件缺字段或旧版本文件都能读到完整设置；
//! - 加载后做数值钳制：被手改坏的文件不至于让渲染层拿到 0 号字或负行高；
//! - 保存走「临时文件 + rename」原子替换，进程中途被杀不会留下半截配置。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod commands;
pub mod importers;
pub mod servers;
pub mod session;
pub mod sync;
pub mod theme;
pub mod totp;
pub mod triggers;

pub use commands::{CommandTemplate, normalize_shortcut, shortcut_conflicts};
pub use importers::{
    deepin_commands_default_path, deepin_default_path, load_deepin_commands, load_deepin_servers,
    load_xshell_sessions,
};
pub use servers::{
    ServerAuthKind, ServerEntry, ServerTunnel, ServersFile, TunnelKind, clear_broken_jump_refs,
    normalize_group, resolve_jump_chain, servers_referencing_jump,
};
// WebDAV 同步元数据类型最常用，随手 re-export；其余走 sync:: 路径。
pub use sync::WebdavSyncSettings;
pub use theme::{DEFAULT_THEME, Theme, ThemeRegistry, themes_dir};
pub use totp::{TotpAlgorithm, TotpConfig, TotpEntry, generate_code, parse_secret_input};
pub use triggers::{TriggerAction, TriggerRule};

pub const APP_DIR: &str = "laterterm";
pub const SETTINGS_FILE: &str = "settings.toml";

/// 终端外观与行为设置，对应工作台「设置」面板可调项。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// 内置面板快捷键覆盖；缺字段时沿用现有默认绑定。
    #[serde(default)]
    pub shortcuts: PanelShortcuts,
    /// 用户自定义快捷命令模板；缺字段的旧配置自动为空。
    #[serde(default)]
    pub commands: Vec<CommandTemplate>,
    /// TOTP 动态码条目元数据；密钥本体在系统密钥环（laterterm-secrets），绝不落盘。
    #[serde(default)]
    pub totp: Vec<totp::TotpEntry>,
    /// 服务器连接元数据；凭据同样只在密钥环。
    #[serde(default)]
    pub servers: Vec<servers::ServerEntry>,
    /// 输出触发器规则（正则命中 → 通知/高亮）；正则语法在 app 侧编译期校验，
    /// 本 crate 保持零 regex 依赖（见 triggers.rs 模块头注记）。
    #[serde(default)]
    pub triggers: Vec<triggers::TriggerRule>,
    /// Quake 下拉模式设置；热键本体由桌面端 portal 存储与改绑，
    /// 应用侧不持有键位文本，避免与桌面端真实绑定漂移。
    #[serde(default)]
    pub quake: QuakeSettings,
    /// SSH 断线自动重连设置（设置→服务器页「自动重连」卡片）。
    #[serde(default)]
    pub reconnect: ReconnectSettings,
    /// 启动时恢复上次会话的标签列表（默认关——避免测试环境误触发）。
    #[serde(default)]
    pub restore_session: bool,
    /// 旧主线 servers.toml 只迁移一次，避免已删除条目重启后复活。
    #[serde(default)]
    pub legacy_servers_migrated: bool,
    /// 本地终端 shell 可执行文件路径；None = 跟随 $SHELL / 平台默认（动态解析，
    /// 用户日后改登录 shell 应用自动跟随）。首启引导可将其固化为具体路径。
    #[serde(default)]
    pub shell: Option<String>,
    /// WebDAV 配置同步账户元数据（地址 + 账号）。密码只进系统密钥环，
    /// 本段是明文同步元数据，合并期恒取本地（sync::LOCAL_ONLY_PATHS）。
    #[serde(default)]
    pub sync: sync::WebdavSyncSettings,
}

/// Quake 下拉模式设置（设置→快捷键页「系统级热键」卡片）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuakeSettings {
    /// 是否启用系统级全局热键（Linux 走 xdg-desktop-portal GlobalShortcuts）。
    pub enabled: bool,
    /// 窗口失焦时自动最小化收起（Quake 惯例默认开）。
    pub auto_hide_on_blur: bool,
}

impl Default for QuakeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_hide_on_blur: true,
        }
    }
}

impl QuakeSettings {
    /// 载入期钳制。当前全为布尔字段无可钳制范围，保留入口与外观项对齐，
    /// 后续新增数值字段（如宽限期）时在此收紧。
    pub fn sanitize(&mut self) {}
}

/// SSH 断线自动重连设置（设置→服务器页「自动重连」卡片）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconnectSettings {
    /// 断线自动重连开关。默认开——断线自动重拨是终端的常规预期；
    /// 密钥环命中免补输，缺失才弹既有补输对话框。
    pub enabled: bool,
    /// 最大自动尝试次数（指数退避 1s/2s/4s…）；0 = 只提示不自动重试。
    pub max_attempts: u32,
}

impl Default for ReconnectSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 3,
        }
    }
}

impl ReconnectSettings {
    /// 载入期钳制：次数封顶 10，防手改出天文数字拖垮后台
    /// （Appearance::sanitize 先例；容器级 serde(default) 覆盖缺字段）。
    pub fn sanitize(&mut self) {
        self.max_attempts = self.max_attempts.clamp(0, 10);
    }
}

/// 内置面板与窗格操作的快捷键覆盖；`None` 表示使用默认绑定，原始文本保持不变。
/// `zoom_pane` 的 `None` = 默认 `ctrl+shift+z`（窗格缩放是内置功能，永远有生效键，
/// 「清空」语义即恢复默认，与命令/服务器面板的「清空 = 真解绑」不同——区分口径见
/// 设置页行文案与编辑弹窗说明）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelShortcuts {
    pub commands_panel: Option<String>,
    pub servers_panel: Option<String>,
    /// 窗格缩放（放大/还原焦点窗格）；`None` = 默认 `ctrl+shift+z`。
    pub zoom_pane: Option<String>,
}

impl PanelShortcuts {
    /// 保存前复用统一快捷键校验；不归一化或清空已保存的绑定。
    pub fn validate(&self) -> Result<()> {
        for (panel, shortcut) in [
            ("快捷命令面板", &self.commands_panel),
            ("服务器面板", &self.servers_panel),
            ("窗格缩放", &self.zoom_pane),
        ] {
            if let Some(shortcut) = shortcut {
                normalize_shortcut(shortcut).with_context(|| format!("{panel}快捷键格式不合法"))?;
            }
        }
        Ok(())
    }
}

/// 终端光标形状。TOML 中以小写字符串存储（`cursor_style = "block"`）。
///
/// `Block` 上标 `#[serde(other)]`：手改出未知值时降级为方块，而不是让整份
/// settings.toml 解析失败——否则 `load_or_default` 会静默回退全默认，下次
/// 保存即覆盖用户整个配置文件。serde 要求 `other` 变体必须声明在末位，
/// 故 Block 物理上排最后；逻辑默认仍为方块（见 `Appearance::default()`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    /// 竖线（光标列左缘的细竖条）。
    Bar,
    /// 下划线（贴格底的横条）。
    Underline,
    /// 方块（默认，占满整格）。
    #[serde(other)]
    Block,
}

/// 外观项。数值范围与设置面板的步进按钮一致。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub padding: f32,
    pub scrollback: usize,
    pub theme: String,
    /// 是否启用连字（calt）；终端下常有人偏好关闭。
    #[serde(default = "default_true")]
    pub ligatures: bool,
    /// 备选字体链（主字体缺字时按序回退，如 CJK）；空表示用系统默认回退。
    #[serde(default)]
    pub font_fallbacks: Vec<String>,

    /// 光标闪烁。默认开启；关闭后光标常显。
    #[serde(default = "default_true")]
    pub cursor_blink: bool,

    /// 光标形状：方块 / 竖线 / 下划线。默认方块；旧配置缺字段自动得方块。
    pub cursor_style: CursorStyle,

    /// 平滑滚动：滚轮/触控板按像素累积连续滚动。默认开启。
    #[serde(default = "default_true")]
    pub smooth_scrolling: bool,

    /// 选中即复制：拖拽/双击/三击松开自动写入剪贴板。默认开启；旧配置缺字段自动为 true。
    #[serde(default = "default_true")]
    pub copy_on_select: bool,

    /// 多行粘贴确认：剪贴板内容含换行（含单行命令+尾回车）时先弹确认弹窗，
    /// 防误粘贴即执行。默认开启；旧配置缺字段自动为 true。
    #[serde(default = "default_true")]
    pub confirm_multiline_paste: bool,

    /// 复制提示 toast：选区复制成功后窗口右下角短暂显示「已复制 N 字符」。
    /// 默认开启；旧配置缺字段自动为 true。
    #[serde(default = "default_true")]
    pub copy_toast: bool,

    /// 关闭运行中终端确认：关闭窗格/标签/窗口时若探测到会话内仍有前台或
    /// 子进程在运行（如 sleep/vim/后台 job），先弹确认弹窗再执行关闭。
    /// 默认开启；旧配置缺字段自动为 true。
    #[serde(default = "default_true")]
    pub confirm_close_running: bool,

    /// 终端 bell 时发送系统桌面通知。默认关闭（bell 走视觉提示，测试环境也不外发）。
    #[serde(default)]
    pub bell_notification: bool,

    /// 会话日志全局默认：新会话（标签/分屏/SSH）attach 时读此值作审计 tee 初值。
    /// 默认关闭——PTY 输出含潜在敏感内容（回显型密码提示仍可能入盘），不宜默认落盘；
    /// 运行中窗格的热开/热关在工作区标签右键菜单逐标签进行，不随此处联动。
    #[serde(default)]
    pub session_logging: bool,

    /// kitty keyboard protocol 支持开关（CSI > 1u / CSI ? u 协商）。默认
    /// 关闭：本端编码表是手写风险面，未经真实应用（fish/tmux）验证前不
    /// 默认开启；开启后应用经查询自动启用 disambiguate 层编码（Esc 与
    /// 修饰文本键转 CSI u）。
    #[serde(default)]
    pub kitty_keyboard: bool,

    /// Shell 集成（OSC 133 提示符标记 + OSC 7 cwd）：对本地 bash 会话经环境
    /// 变量注入 PROMPT_COMMAND/PS0 钩子（laterterm-local
    /// `shell_integration_env`）。默认开启——只影响 LaterTerm 内新开的本地
    /// bash 会话，零 rc 文件改动；best-effort：用户 ~/.bashrc 重赋值这两个
    /// 变量即静默失效（设置面板有提示文案，README 有 zsh/fish 自装片段）。
    #[serde(default = "default_true")]
    pub shell_integration: bool,

    /// 右侧工具面板是否图钉固定。默认固定（占布局宽度常驻）；
    /// 取消固定后转为悬浮自动隐藏（点终端收起，快捷键/标题栏按钮唤出）。
    #[serde(default = "default_true")]
    pub right_panel_pinned: bool,

    /// 终端背景不透明度：1.0 = 完全不透明（默认，绘制与旧版逐字节一致）；
    /// 允许 0.15~1.0，载入期 sanitize 钳制。低于 1.0 时窗口走透明合成，
    /// 窗口底色由 app 层根容器以主题 background 原色@不透明度承载。
    #[serde(default = "default_background_opacity")]
    pub background_opacity: f32,

    /// 背景模糊：background_opacity < 1.0 时请求窗口管理器/合成器模糊窗口
    /// 背后内容。Linux 仅 Wayland KDE blur 协议支持；X11 无此路径，按纯
    /// 透明显示（设置页有降级提示）。默认关闭。
    #[serde(default)]
    pub background_blur: bool,

    /// 接收目录；空 = ~/Downloads。
    pub downloads_dir: String,
}

fn default_true() -> bool {
    true
}

fn default_background_opacity() -> f32 {
    1.0
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            font_family: "JetBrainsMono Nerd Font".into(),
            font_size: 13.0,
            // 1.2 行距读长日志更舒适；纯网格对齐不受影响（TUI 按单元格定位）
            line_height: 1.2,
            padding: 8.0,
            scrollback: 100_000,
            theme: DEFAULT_THEME.into(),
            ligatures: true,
            cursor_blink: true,
            cursor_style: CursorStyle::Block,
            smooth_scrolling: true,
            copy_on_select: true,
            confirm_multiline_paste: true,
            copy_toast: true,
            confirm_close_running: true,
            bell_notification: false,
            session_logging: false,
            kitty_keyboard: false,
            shell_integration: true,
            right_panel_pinned: true,
            background_opacity: 1.0,
            background_blur: false,
            font_fallbacks: Vec::new(),
            downloads_dir: String::new(),
        }
    }
}

impl Settings {
    /// 保留当前同名配置，迁移标记与条目在同一 settings.toml 中原子保存。
    pub fn migrate_legacy_servers(&mut self, legacy: servers::ServerStore) -> bool {
        if self.legacy_servers_migrated {
            return false;
        }
        for entry in legacy.servers {
            let entry: servers::ServerEntry = entry.into();
            if let Some(entry) = entry.sanitized()
                && !self
                    .servers
                    .iter()
                    .any(|current| current.name == entry.name)
            {
                self.servers.push(entry);
            }
        }
        self.legacy_servers_migrated = true;
        true
    }

    pub fn upsert_command(&mut self, command: CommandTemplate) -> Result<()> {
        command.validate()?;
        if let Some(existing) = self
            .commands
            .iter_mut()
            .find(|item| item.name == command.name)
        {
            *existing = command;
        } else {
            self.commands.push(command);
        }
        Ok(())
    }

    pub fn remove_command(&mut self, name: &str) -> bool {
        let before = self.commands.len();
        self.commands.retain(|command| command.name != name);
        before != self.commands.len()
    }

    /// 新增或按名称替换触发器规则（upsert_command 同款）。
    pub fn upsert_trigger(&mut self, trigger: triggers::TriggerRule) -> Result<()> {
        trigger.validate()?;
        if let Some(existing) = self
            .triggers
            .iter_mut()
            .find(|item| item.name == trigger.name)
        {
            *existing = trigger;
        } else {
            self.triggers.push(trigger);
        }
        Ok(())
    }

    pub fn remove_trigger(&mut self, name: &str) -> bool {
        let before = self.triggers.len();
        self.triggers.retain(|trigger| trigger.name != name);
        before != self.triggers.len()
    }

    pub fn command_shortcut_conflicts(&self) -> Vec<String> {
        commands::shortcut_conflicts(self.commands.iter().filter_map(|command| {
            command
                .shortcut
                .as_deref()
                .map(|shortcut| (command.name.as_str(), shortcut))
        }))
    }

    /// 新增或按名称替换 TOTP 条目；密钥本体由调用方负责写入密钥环。
    pub fn upsert_totp(&mut self, entry: totp::TotpEntry) -> Result<()> {
        entry.validate()?;
        if let Some(existing) = self.totp.iter_mut().find(|item| item.name == entry.name) {
            *existing = entry;
        } else {
            self.totp.push(entry);
        }
        Ok(())
    }

    pub fn remove_totp(&mut self, name: &str) -> bool {
        let before = self.totp.len();
        self.totp.retain(|entry| entry.name != name);
        before != self.totp.len()
    }

    /// 把加载中的 TOTP 条目元数据修正到合法范围；无法挽救的条目直接丢弃。
    fn sanitize_totp(&mut self) {
        self.totp.retain_mut(|entry| {
            entry.name = entry.name.trim().to_string();
            if entry.name.is_empty() {
                return false;
            }
            entry.digits = entry.digits.clamp(6, 8);
            entry.period = entry.period.clamp(1, 3600);
            true
        });
    }

    /// 新增或按名称替换服务器条目。
    pub fn upsert_server(&mut self, server: servers::ServerEntry) -> Result<()> {
        server.validate()?;
        if let Some(existing) = self
            .servers
            .iter_mut()
            .find(|item| item.name == server.name)
        {
            *existing = server;
        } else {
            self.servers.push(server);
        }
        Ok(())
    }

    pub fn remove_server(&mut self, name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|server| server.name != name);
        before != self.servers.len()
    }

    /// shell 字段载入期校验：非法值归 None（回退动态默认），不让坏值活到
    /// spawn 期才炸（LocalSession::spawn 失败是 .expect panic）。
    ///
    /// 结构性规则与 laterterm-local 的 `is_valid_shell_path` 一致——config
    /// 不依赖 laterterm-local（避免反向引入 portable-pty），谓词在此复制一份，
    /// 两处修改必须同步。残余风险（路径存在但非可执行文件）与 $SHELL 注入
    /// 路径风险相同，v1 不做 exists 校验（测试环境路径不稳定）。
    fn sanitize_shell(&mut self) {
        self.shell = self
            .shell
            .take()
            .map(|shell| shell.trim().to_string())
            .filter(|shell| is_valid_shell_path(shell));
    }

    /// 修正加载中的连接信息；错误快捷键保留原值供用户修正。
    ///
    /// 跳板引用（jump）单独走全表清理：格式非法、悬空、成环、超深一律断链
    /// 保条目——这是载入期兜底，坏引用不会一直存活到连接期才暴露。
    /// 返回被清理引用的条目名清单（供 `Settings::load` 向用户回报）。
    fn sanitize_servers(&mut self) -> Vec<String> {
        self.servers.retain_mut(|server| {
            server.name = server.name.trim().to_string();
            server.host = server.host.trim().to_string();
            server.username = server.username.trim().to_string();
            server.note = server.note.trim().to_string();
            // 分组宽容归一化：坏分组降级 None，绝不因分组丢条目
            server.group = servers::normalize_group(server.group.as_deref());
            // 隧道值级归一化：坏隧道具名 eprintln 丢弃，条目保留
            servers::normalize_tunnels(server);
            if let Some(key_path) = &server.key_path {
                let trimmed = key_path.trim().to_string();
                server.key_path = (!trimmed.is_empty()).then_some(trimmed);
            }
            if server.validate_connection().is_err() {
                return false;
            }
            server.warn_invalid_shortcut();
            true
        });
        servers::clear_broken_jump_refs(&mut self.servers)
    }

    /// 重命名服务器后改写所有指向旧名的跳板引用（按名引用随改名同步，
    /// 跨机器改名断链由导入侧悬空清理兜底）。
    pub fn retarget_jump_refs(&mut self, old: &str, new: &str) {
        for server in &mut self.servers {
            if server.jump.as_deref() == Some(old) {
                server.jump = Some(new.to_string());
            }
        }
    }

    /// 用户配置目录下的设置文件路径。
    pub fn default_path() -> Option<PathBuf> {
        Some(config_dir()?.join(APP_DIR).join(SETTINGS_FILE))
    }
    /// 从指定路径加载设置；文件不存在时返回默认值（首次运行）。
    ///
    /// 解析失败会向上抛错，由调用方决定回退策略——避免把用户手改坏的
    /// 文件在下次保存时无声覆盖。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取设置失败: {}", path.display()))?;
        let mut settings: Settings =
            toml::from_str(&raw).with_context(|| format!("解析设置失败: {}", path.display()))?;
        settings.appearance.sanitize();
        settings.quake.sanitize();
        settings.reconnect.sanitize();
        settings.sanitize_totp();
        let cleared_jumps = settings.sanitize_servers();
        // 载入期可见回报：措辞与导入侧（workspace「无效跳板引用已清除 N（…）」）
        // 一致。启动早于 UI、无通知通道，沿用 eprintln 先例（解析失败告警、
        // 快捷键无效告警同款）。
        if !cleared_jumps.is_empty() {
            eprintln!(
                "无效跳板引用已清除 {}（{}）",
                cleared_jumps.len(),
                cleared_jumps.join("、")
            );
        }
        settings.sanitize_shell();
        Ok(settings)
    }

    /// 从默认路径加载；任何失败回退默认值并告警（启动路径不能被坏文件卡死）。
    pub fn load_or_default() -> Self {
        match Self::default_path() {
            Some(path) => Self::load(&path).unwrap_or_else(|error| {
                eprintln!("{error}，使用默认设置");
                Self::default()
            }),
            None => {
                eprintln!("无法定位用户配置目录，使用默认设置");
                Self::default()
            }
        }
    }

    /// 原子保存到指定路径（临时文件 + rename），自动创建父目录。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("序列化设置失败")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).with_context(|| format!("写入设置失败: {}", tmp.display()))?;
        // Windows 上 rename 不能覆盖已有文件，先移除再改名；Linux rename 本身原子。
        #[cfg(target_os = "windows")]
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("移除旧设置失败: {}", path.display()))?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换设置文件失败: {}", path.display()))?;
        Ok(())
    }

    /// 保存到默认路径；失败只告警不打断 UI。
    pub fn save(&self) {
        if let Some(path) = Self::default_path()
            && let Err(error) = self.save_to(&path)
        {
            eprintln!("{error:#}");
        }
    }

    /// 首次运行时生成默认配置文件，方便用户发现并手改。
    ///
    /// 返回是否真正创建（文件已存在则不动，坏文件也不会被覆盖）。
    pub fn ensure_settings_file(&self) -> bool {
        match Self::default_path() {
            Some(path) if !path.exists() => self.save_to(&path).is_ok(),
            _ => false,
        }
    }
}

impl Appearance {
    /// 把越界数值拉回面板允许的范围，防止坏文件产生 0 号字 / 负行高。
    pub fn sanitize(&mut self) {
        self.font_size = self.font_size.clamp(8.0, 32.0);
        self.line_height = self.line_height.clamp(0.8, 2.0);
        self.padding = self.padding.clamp(0.0, 24.0);
        self.scrollback = self.scrollback.clamp(1_000, 1_000_000);
        // 背景不透明度下限 0.15：再低终端内容基本不可读，只留微弱剪影。
        // clamp 不拦 NaN（TOML 规范支持 nan 字面量），先 is_finite 兜底回
        // 默认不透明，否则 NaN 会让下游 <1.0 / >=1.0 判定双双落空、渲染
        // 极性分裂（透明口径画不透明底、alpha 位算出 0）。
        if !self.background_opacity.is_finite() {
            self.background_opacity = 1.0;
        }
        self.background_opacity = self.background_opacity.clamp(0.15, 1.0);
        if self.font_family.trim().is_empty() {
            self.font_family = Self::default().font_family;
        }
        self.theme = self.theme.trim().to_string();
        if self.theme.is_empty() {
            self.theme = DEFAULT_THEME.into();
        }
        self.font_fallbacks = self
            .font_fallbacks
            .iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
    }
}

/// 应用配置目录（`<配置目录>/laterterm`），用于文件监听等。
pub fn app_config_dir() -> Option<PathBuf> {
    Some(config_dir()?.join(APP_DIR))
}

/// 应用状态目录（`<状态目录>/laterterm`），存放会话日志等可再生数据——
/// 与 settings.toml（用户配置、应随备份迁移）区分：状态目录删了可重建。
pub fn app_state_dir() -> Option<PathBuf> {
    Some(state_dir()?.join(APP_DIR))
}

/// 平台状态目录（Linux 优先，遵循 XDG Base Directory 的 state 规范）。
/// 与 [`config_dir`] 同构：Linux 读 `XDG_STATE_HOME`（绝对路径校验同口径），
/// 回落 `~/.local/state`；macOS / Windows 无 state 惯例，按配置目录同源处理。
fn state_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME")
            && Path::new(&xdg).is_absolute()
        {
            return Some(PathBuf::from(xdg));
        }
        home_dir().map(|home| home.join(".local").join("state"))
    }

    #[cfg(target_os = "macos")]
    {
        return home_dir().map(|home| home.join("Library").join("Application Support"));
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// 平台配置目录（Linux 优先，遵循 XDG）。
fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && Path::new(&xdg).is_absolute()
        {
            return Some(PathBuf::from(xdg));
        }
        home_dir().map(|home| home.join(".config"))
    }

    #[cfg(target_os = "macos")]
    {
        return home_dir().map(|home| home.join("Library").join("Application Support"));
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA").map(PathBuf::from);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// shell 路径结构性校验：非空、无控制字符/引号、绝对路径。
/// 与 laterterm-local 的 `is_valid_shell_path` 规则一致（见 sanitize_shell 注释）。
fn is_valid_shell_path(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\'' | '"'))
        && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "laterterm-config-test-{name}-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn partial_file_fills_defaults() {
        let raw = "[appearance]\nfont_size = 15.0\n";
        let settings: Settings = toml::from_str(raw).unwrap();
        assert_eq!(settings.appearance.font_size, 15.0);
        assert_eq!(settings.appearance.font_family, "JetBrainsMono Nerd Font");
        assert_eq!(settings.appearance.scrollback, 100_000);
    }

    #[test]
    fn panel_shortcuts_default_when_legacy_settings_omit_them() {
        for raw in ["", "[appearance]\nfont_size = 15.0\n", "[shortcuts]\n"] {
            let settings: Settings = toml::from_str(raw).unwrap();
            assert_eq!(settings.shortcuts, PanelShortcuts::default());
            assert_eq!(settings.shortcuts.commands_panel, None);
            assert_eq!(settings.shortcuts.servers_panel, None);
            settings.shortcuts.validate().unwrap();
            let saved = toml::to_string_pretty(&settings).unwrap();
            assert_eq!(toml::from_str::<Settings>(&saved).unwrap(), settings);
        }
        for raw in [
            "[shortcuts]\ncommands_panel = 'ctrl+shift+k'\n",
            "[shortcuts]\nservers_panel = 'ctrl+shift+s'\n",
        ] {
            let settings: Settings = toml::from_str(raw).unwrap();
            assert_ne!(settings.shortcuts, PanelShortcuts::default());
            settings.shortcuts.validate().unwrap();
            let saved = toml::to_string_pretty(&settings).unwrap();
            assert_eq!(toml::from_str::<Settings>(&saved).unwrap(), settings);
        }
    }

    #[test]
    fn panel_shortcuts_validate_using_normalizer_without_rewriting_text() {
        let shortcuts = PanelShortcuts {
            commands_panel: Some("  Control + Shift + K  ".into()),
            servers_panel: Some(" Super + Alt + S ".into()),
            zoom_pane: None,
        };
        let original = shortcuts.clone();
        shortcuts.validate().unwrap();
        assert_eq!(shortcuts, original);
        assert_eq!(
            normalize_shortcut(shortcuts.commands_panel.as_deref().unwrap()).unwrap(),
            "ctrl+shift+k"
        );
        assert_eq!(
            normalize_shortcut(shortcuts.servers_panel.as_deref().unwrap()).unwrap(),
            "alt+cmd+s"
        );
    }

    #[test]
    fn panel_shortcuts_reject_each_invalid_override_without_rewriting_it() {
        for shortcut in ["", "   ", "ctrl", "ctrl+control+k", "alt++k", "unknown+k"] {
            for shortcuts in [
                PanelShortcuts {
                    commands_panel: Some(shortcut.into()),
                    ..Default::default()
                },
                PanelShortcuts {
                    servers_panel: Some(shortcut.into()),
                    ..Default::default()
                },
                PanelShortcuts {
                    zoom_pane: Some(shortcut.into()),
                    ..Default::default()
                },
            ] {
                let original = shortcuts.clone();
                assert!(shortcuts.validate().is_err(), "{shortcuts:?}");
                assert_eq!(shortcuts, original);
            }
        }
    }

    // 窗格缩放（zoom_pane）：解析 / 校验 / 默认 None 三例——None = 默认
    // ctrl+shift+z 由 app 层 effective_zoom_shortcut 回落，配置侧只存 override。
    #[test]
    fn zoom_pane_parses_override_and_defaults_to_none() {
        for raw in ["", "[appearance]\nfont_size = 15.0\n", "[shortcuts]\n"] {
            let settings: Settings = toml::from_str(raw).unwrap();
            assert_eq!(settings.shortcuts.zoom_pane, None, "旧配置缺字段 = 默认键");
        }
        let settings: Settings = toml::from_str("[shortcuts]\nzoom_pane = 'ctrl+alt+z'\n").unwrap();
        assert_eq!(settings.shortcuts.zoom_pane.as_deref(), Some("ctrl+alt+z"));
        settings.shortcuts.validate().unwrap();
        // 保存往返：override 原文（含空格大小写）不清洗不丢失
        let saved = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&saved).unwrap(), settings);
    }

    #[test]
    fn zoom_pane_validate_uses_normalizer() {
        PanelShortcuts {
            zoom_pane: Some("  Ctrl + Shift + Z  ".into()),
            ..Default::default()
        }
        .validate()
        .unwrap();
        assert_eq!(
            normalize_shortcut("  Ctrl + Shift + Z  ").unwrap(),
            "ctrl+shift+z"
        );
    }

    /// shell_integration：旧配置缺字段自动得 true（serde default），显式
    /// false 保存往返不丢。
    #[test]
    fn shell_integration_defaults_on_and_roundtrips() {
        let settings: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(
            settings.appearance.shell_integration,
            "旧配置缺字段 = 默认开"
        );
        let settings: Settings =
            toml::from_str("[appearance]\nshell_integration = false\n").unwrap();
        assert!(!settings.appearance.shell_integration);
        let saved = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&saved).unwrap(), settings);
    }

    #[test]
    fn settings_load_save_preserves_panel_and_legacy_shortcut_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let raw = r#"
[shortcuts]
commands_panel = "  Control + Shift + K  "
servers_panel = "   "

[[commands]]
name = "命令"
template = "pwd"
shortcut = "  Ctrl + Shift + L  "

[[totp]]
name = "动态码"
shortcut = "   "

[[servers]]
name = "服务器"
host = "example.com"
username = "ops"
shortcut = "  legacy shortcut  "
"#;
        std::fs::write(&path, raw).unwrap();
        let settings = Settings::load(&path).unwrap();
        settings.save_to(&path).unwrap();
        let original: toml::Value = toml::from_str(raw).unwrap();
        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.get("shortcuts"), original.get("shortcuts"));
        for collection in ["commands", "totp", "servers"] {
            assert_eq!(
                saved[collection][0]["shortcut"], original[collection][0]["shortcut"],
                "{collection} 的历史快捷键必须保留原值"
            );
        }
        assert_eq!(Settings::load(&path).unwrap(), settings);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let path = temp_settings_path("missing");
        let settings = Settings::load(&path).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn load_sanitizes_out_of_range_values() {
        let path = temp_settings_path("sanitize");
        std::fs::write(
            &path,
            "[appearance]\nfont_size = 3.0\nline_height = 9.0\npadding = -5.0\nscrollback = 0\nfont_family = \"  \"\n",
        )
        .unwrap();
        let settings = Settings::load(&path).unwrap();
        let appearance = settings.appearance;
        assert_eq!(appearance.font_size, 8.0);
        assert_eq!(appearance.line_height, 2.0);
        assert_eq!(appearance.padding, 0.0);
        assert_eq!(appearance.scrollback, 1_000);
        assert_eq!(appearance.font_family, "JetBrainsMono Nerd Font");
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_settings_path("roundtrip");
        let mut settings = Settings::default();
        settings.appearance.font_size = 16.0;
        settings.appearance.scrollback = 50_000;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(settings, loaded);
        // 原子写不应留下临时文件
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn font_options_roundtrip_and_defaults() {
        // 旧版文件缺 ligatures/font_fallbacks 字段也能读出默认值
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.ligatures);
        assert!(legacy.appearance.font_fallbacks.is_empty());

        let path = temp_settings_path("font-options");
        let mut settings = Settings::default();
        settings.appearance.ligatures = false;
        settings.appearance.font_fallbacks = vec![
            " Sarasa Mono SC ".into(),
            "".into(),
            "Noto Sans Mono CJK SC".into(),
        ];
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.ligatures);
        // sanitize 应去空白、丢空项
        assert_eq!(
            loaded.appearance.font_fallbacks,
            vec!["Sarasa Mono SC", "Noto Sans Mono CJK SC"]
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn cursor_and_smooth_scrolling_defaults_and_false_roundtrip() {
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.cursor_blink);
        assert!(legacy.appearance.smooth_scrolling);

        let path = temp_settings_path("scroll-options");
        let mut settings = Settings::default();
        settings.appearance.cursor_blink = false;
        settings.appearance.smooth_scrolling = false;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.cursor_blink);
        assert!(!loaded.appearance.smooth_scrolling);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn copy_on_select_defaults_and_false_roundtrip() {
        // 旧 settings.toml 缺 copy_on_select 字段 → serde 补默认 true
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.copy_on_select);

        // 显式关闭后 roundtrip 保持 false
        let path = temp_settings_path("copy-on-select");
        let mut settings = Settings::default();
        settings.appearance.copy_on_select = false;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.copy_on_select);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn right_panel_pinned_defaults_and_false_roundtrip() {
        // 旧 settings.toml 缺 right_panel_pinned 字段 → serde 补默认 true（固定）
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.right_panel_pinned);

        // 取消固定（自动隐藏模式）后 roundtrip 保持 false
        let path = temp_settings_path("right-panel-pinned");
        let mut settings = Settings::default();
        settings.appearance.right_panel_pinned = false;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.right_panel_pinned);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn background_opacity_and_blur_defaults_roundtrip_and_clamp() {
        // 旧 settings.toml 缺 background_opacity/background_blur 字段
        // → serde 补默认 1.0 / false（与透明功能上线前的绘制逐字节一致）
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert_eq!(legacy.appearance.background_opacity, 1.0);
        assert!(!legacy.appearance.background_blur);

        // 显式值 roundtrip：0.7 + 模糊开，保存/加载相等
        let path = temp_settings_path("background-opacity");
        let mut settings = Settings::default();
        settings.appearance.background_opacity = 0.7;
        settings.appearance.background_blur = true;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.appearance.background_opacity, 0.7);
        assert!(loaded.appearance.background_blur);

        // 越界钳制：手改坏值载入期 sanitize 拉回 0.15~1.0
        std::fs::write(
            &path,
            "[appearance]\nbackground_opacity = 0.01\nbackground_blur = true\n",
        )
        .unwrap();
        let clamped = Settings::load(&path).unwrap();
        assert_eq!(clamped.appearance.background_opacity, 0.15);
        assert!(clamped.appearance.background_blur);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn background_opacity_nan_falls_back_to_opaque() {
        // TOML 规范支持 nan 字面量：clamp 不拦 NaN（比较恒 false），
        // sanitize 必须 is_finite 兜底，否则下游透明判定极性分裂。
        // sanitize 在 load 流程内执行，走临时文件路径验证。
        let path = temp_settings_path("background-opacity-nan");
        std::fs::write(&path, "[appearance]\nbackground_opacity = nan\n").unwrap();
        let nan = Settings::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(nan.appearance.background_opacity.is_finite());
        assert_eq!(nan.appearance.background_opacity, 1.0);
    }

    #[test]
    fn paste_and_toast_flags_default_true_and_false_roundtrip() {
        // 旧 settings.toml 缺 confirm_multiline_paste/copy_toast 字段 → serde 补默认 true
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.confirm_multiline_paste);
        assert!(legacy.appearance.copy_toast);

        // 显式关闭后 roundtrip 保持 false（不会被默认值覆盖）
        let path = temp_settings_path("paste-toast-flags");
        let mut settings = Settings::default();
        settings.appearance.confirm_multiline_paste = false;
        settings.appearance.copy_toast = false;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.confirm_multiline_paste);
        assert!(!loaded.appearance.copy_toast);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn confirm_close_running_defaults_true_and_false_roundtrip() {
        // 旧 settings.toml 缺 confirm_close_running 字段 → serde 补默认 true（开启确认）
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.appearance.confirm_close_running);

        // 显式关闭后 roundtrip 保持 false（不会被默认值覆盖）
        let path = temp_settings_path("confirm-close-running");
        let mut settings = Settings::default();
        settings.appearance.confirm_close_running = false;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.appearance.confirm_close_running);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn session_logging_defaults_off_and_false_roundtrip() {
        // 旧 settings.toml 缺 session_logging 字段 → serde 补默认 false：
        // 日志含终端输出（回显型密码提示可能入盘），全局默认必须收敛在「关」
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(!legacy.appearance.session_logging);

        // 显式开启后 roundtrip 保持 true（不会被默认值覆盖）
        let path = temp_settings_path("session-logging");
        let mut settings = Settings::default();
        settings.appearance.session_logging = true;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(loaded.appearance.session_logging);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn triggers_crud_upsert_remove_and_toml_roundtrip() {
        // 旧 settings.toml 缺 [[triggers]] 段 → 空表（不缺字段报错）
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.triggers.is_empty());

        // upsert 按名替换 + remove 删除（commands 同款语义）
        let mut settings = Settings::default();
        settings
            .upsert_trigger(triggers::TriggerRule {
                name: "错误".into(),
                pattern: "ERROR".into(),
                action: triggers::TriggerAction::Highlight,
                enabled: true,
            })
            .unwrap();
        settings
            .upsert_trigger(triggers::TriggerRule {
                name: "错误".into(),
                pattern: "FATAL".into(),
                action: triggers::TriggerAction::Notify,
                enabled: false,
            })
            .unwrap();
        assert_eq!(settings.triggers.len(), 1);
        assert_eq!(settings.triggers[0].pattern, "FATAL");
        assert!(!settings.triggers[0].enabled);
        assert!(
            settings
                .upsert_trigger(triggers::TriggerRule::default())
                .is_err()
        );
        assert!(settings.remove_trigger("错误"));
        assert!(!settings.remove_trigger("错误"));

        // TOML roundtrip：动作小写、未知动作兜底 Notify（triggers.rs 内已单测）
        let path = temp_settings_path("triggers");
        settings
            .upsert_trigger(triggers::TriggerRule {
                name: "完成".into(),
                pattern: "构建完成".into(),
                action: triggers::TriggerAction::Notify,
                enabled: true,
            })
            .unwrap();
        settings.save_to(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("[[triggers]]"), "{body}");
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.triggers, settings.triggers);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn cursor_style_defaults_roundtrip_and_unknown_falls_back() {
        // 旧文件缺 cursor_style 字段 → 容器级 serde(default) 得 Block
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert_eq!(legacy.appearance.cursor_style, CursorStyle::Block);

        // 合法值 roundtrip：TOML 输出 lowercase，保存/加载相等
        let path = temp_settings_path("cursor-style");
        let mut settings = Settings::default();
        settings.appearance.cursor_style = CursorStyle::Underline;
        settings.save_to(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("cursor_style = \"underline\""), "{body}");
        assert_eq!(Settings::load(&path).unwrap(), settings);

        // 手改坏值：serde(other) 兜底为 Block，整份配置不解析失败
        let patched: Settings = toml::from_str("[appearance]\ncursor_style = \"wave\"\n").unwrap();
        assert_eq!(patched.appearance.cursor_style, CursorStyle::Block);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reconnect_defaults_true_and_max_attempts_clamped() {
        // 旧 settings.toml 缺 [reconnect] 段 → 默认开 + 3 次（重连开关默认开）
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(legacy.reconnect.enabled);
        assert_eq!(legacy.reconnect.max_attempts, 3);

        // 显式值 roundtrip
        let path = temp_settings_path("reconnect");
        let mut settings = Settings::default();
        settings.reconnect.enabled = false;
        settings.reconnect.max_attempts = 7;
        settings.save_to(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(!loaded.reconnect.enabled);
        assert_eq!(loaded.reconnect.max_attempts, 7);

        // 越界钳制：0 保留（只提示不重试的合法语义），天文数字拉回 10
        std::fs::write(&path, "[reconnect]\nenabled = true\nmax_attempts = 999\n").unwrap();
        let clamped = Settings::load(&path).unwrap();
        assert!(clamped.reconnect.enabled);
        assert_eq!(clamped.reconnect.max_attempts, 10);
        std::fs::write(&path, "[reconnect]\nmax_attempts = 0\n").unwrap();
        let zero = Settings::load(&path).unwrap();
        assert_eq!(zero.reconnect.max_attempts, 0);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn quake_defaults_roundtrip_and_unknown_fields_tolerated() {
        // 旧文件缺 [quake] 段 → Quake 惯例默认：未启用、失焦自动收起
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert!(!legacy.quake.enabled);
        assert!(legacy.quake.auto_hide_on_blur);

        // 显式 false 值保存/加载相等（不会被默认值覆盖）
        let path = temp_settings_path("quake");
        let mut settings = Settings::default();
        settings.quake.enabled = true;
        settings.quake.auto_hide_on_blur = false;
        settings.save_to(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("[quake]"), "{body}");
        assert!(body.contains("auto_hide_on_blur = false"), "{body}");
        assert_eq!(Settings::load(&path).unwrap(), settings);

        // [quake] 内出现未知字段（未来版本新增）不破坏整份文件解析
        let forward: Settings =
            toml::from_str("[quake]\nenabled = true\nfuture_field = 42\n").unwrap();
        assert!(forward.quake.enabled);
        assert!(forward.quake.auto_hide_on_blur);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn shell_defaults_roundtrip_and_bad_values_sanitize_to_none() {
        // 旧文件缺 shell 字段 → 容器级 serde(default) 得 None（跟随 $SHELL 动态解析）
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert_eq!(legacy.shell, None);

        // 合法值 roundtrip：TOML 输出顶层 shell 行，保存/加载相等
        let path = temp_settings_path("shell");
        let settings = Settings {
            shell: Some("/usr/bin/fish".into()),
            ..Default::default()
        };
        settings.save_to(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("shell = \"/usr/bin/fish\""), "{body}");
        assert_eq!(Settings::load(&path).unwrap(), settings);
        // 原子写不应留下临时文件
        assert!(!path.with_extension("toml.tmp").exists());

        // 手改坏值：raw 写在 [appearance] 之前的顶层键（若拼进表内会被 serde
        // 当未知字段忽略，断言恒真——2026-11-06 修正）。from_str 不跑 sanitize
        // （只在 Settings::load 调用），故分两步：先钉坏值确实解析进顶层
        // shell 字段（防测试与 sanitize 路径脱钩），再走完整 load 钉 sanitize 生效。
        for (raw, value) in [
            ("shell = \"bash -l\"\n", "bash -l"), // 带参数——不是纯路径
            ("shell = \"\"\n", ""),               // 空串
            ("shell = \"relative/sh\"\n", "relative/sh"), // 相对路径
            ("shell = \"/bin/sh' -x\"\n", "/bin/sh' -x"), // 引号注入
        ] {
            let body = format!("{raw}[appearance]\nfont_size = 14.0\n");
            let parsed: Settings = toml::from_str(&body).unwrap();
            assert_eq!(
                parsed.shell.as_deref(),
                Some(value),
                "坏值应落在顶层 shell 键: {raw}"
            );
            std::fs::write(&path, &body).unwrap();
            let loaded = Settings::load(&path).unwrap();
            assert_eq!(loaded.shell, None, "载入期 sanitize 应归 None: {raw}");
            assert_eq!(
                loaded.appearance.font_size, 14.0,
                "[appearance] 不受坏值影响"
            );
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn parse_error_is_reported_not_swallowed() {
        let path = temp_settings_path("broken");
        std::fs::write(&path, "not a toml file [[").unwrap();
        assert!(Settings::load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
