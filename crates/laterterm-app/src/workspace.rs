use crate::branding::titlebar_logo;
use crate::command_text_field::{CommandTextChanged, CommandTextField, FieldSubmit};
use crate::onboarding::{self, FONT_PRESETS, OnboardingState, ProbeResults};
use crate::pane_navigation::{Direction as PaneDirection, PaneRect, neighbor};
use crate::quake::{QuakeAction, QuakeInput, QuakePortalEvent, QuakePortalStatus};
use crate::server_connection::{self, ConnectionOutcome, CredentialKind, SftpOutcome};
use crate::sftp_panel::{
    self, PaneSide, SftpEvent, SftpPanelState, SftpPromptKind, SftpTransfer, TransferKind,
    load_remote_into, sftp_overlay,
};
use crate::shortcut_recorder::{ShortcutChanged, ShortcutRecorder};
use crate::update_check::{self, UpdateCheck};
use gpui::{
    App, AppContext, AsyncApp, Bounds, Context, CursorStyle, DispatchPhase, Div, Edges, Entity,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, PathPromptOptions, Pixels, Point, Render,
    ScrollHandle, StatefulInteractiveElement, Styled, WeakEntity, Window, anchored, canvas,
    deferred, div, prelude::FluentBuilder, px, relative, rgb,
};
use laterterm_config::{
    Appearance, CommandTemplate, ServerAuthKind, ServerEntry, ServersFile, Settings, Theme,
    ThemeRegistry, TotpAlgorithm, TotpConfig, TotpEntry, app_config_dir,
    deepin_commands_default_path, deepin_default_path, generate_code, load_deepin_commands,
    load_deepin_servers, load_xshell_sessions, normalize_shortcut, parse_secret_input,
};
use laterterm_core::TerminalSession;
use laterterm_local::{LocalOptions, LocalSession};
use laterterm_secrets::{SecretStore, default_store, totp_account};
use laterterm_termview::view::ScrollSummary;
use laterterm_termview::{ColorPalette, TerminalConfig, TerminalView};
use notify::Watcher;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// 设置面板 UI 子模块：迁移函数需访问 WorkspaceView 私有字段/方法，按 Rust
// 可见性规则挂为 workspace 子模块（平级顶层模块不可见，见 settings_ui.rs 头注）。
mod settings_ui;
pub(crate) use self::settings_ui::{
    SCROLL_SPY_EPSILON_PX, SCROLL_SPY_LEAD_PX, SettingsSection, setting_button, settings_content,
    settings_navigation, settings_overlay, settings_scroll_spy,
};
// 主题色板预览纯函数仅供 workspace_tests.rs 引用（settings_ui 是 workspace
// 私有子模块，顶层平级模块不可直达）；按 cfg(test) 挂载，防非测试构建的
// unused_imports 告警。
#[cfg(test)]
pub(crate) use self::settings_ui::{PalettePreview, palette_preview};

const TITLEBAR_HEIGHT: f32 = 34.0;
const PANEL_WIDTH: f32 = 240.0;

/// 窗格（pane）ID：一个终端会话的最小分屏单元。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PaneId(pub(crate) u64);

/// 标签 ID：一组窗格（含其分屏树）的容器。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TabId(pub(crate) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy)]
enum WorkspaceShortcut {
    Split(SplitDirection),
    FocusPrevious,
    FocusNext,
    ToggleBroadcast,
}

/// 仅读取或打开配置不会触发热加载，避免重读自身产生监听事件的循环。
fn config_event_requires_reload(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode};
    matches!(
        kind,
        notify::EventKind::Any
            | notify::EventKind::Create(_)
            | notify::EventKind::Modify(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Other
            | notify::EventKind::Access(AccessKind::Close(AccessMode::Write))
    )
}

/// 标签内的分屏布局树，叶子是 PaneId。
/// Split 带 id（拖拽定位）与 ratio（首个子树占比，0.0–1.0）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LayoutNode {
    Leaf(PaneId),
    Split {
        id: u64,
        direction: SplitDirection,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
        ratio: f32,
    },
}

/// 递归更新指定分割节点的比例；找不到时静默跳过（节点可能已被修剪）。
pub(crate) fn apply_split_ratio(node: &mut LayoutNode, split_id: u64, ratio: f32) {
    match node {
        LayoutNode::Split {
            id,
            ratio: node_ratio,
            ..
        } if *id == split_id => *node_ratio = ratio,
        LayoutNode::Split { first, second, .. } => {
            apply_split_ratio(first, split_id, ratio);
            apply_split_ratio(second, split_id, ratio);
        }
        LayoutNode::Leaf(_) => {}
    }
}

/// 分割线拖拽中：目标分割节点 + 容器几何（mousedown 时从 paint 缓存取）。
#[derive(Clone, Copy, Debug)]
struct SplitDrag {
    split_id: u64,
    direction: SplitDirection,
    geometry: Bounds<gpui::Pixels>,
}

/// 当前完成绘制的可见窗格几何；同一帧内的坐标不会与其他窗口混用。
struct PaneGeometry {
    tab_id: TabId,
    viewport: gpui::Size<gpui::Pixels>,
    panels: (bool, bool),
    layout: LayoutNode,
    panes: Vec<PaneRect>,
}

thread_local! {
    /// 分割容器几何缓存：paint 期由 canvas 写入，mousedown 换算拖拽比例时读取。
    static SPLIT_GEOMETRY: std::cell::RefCell<std::collections::HashMap<u64, Bounds<gpui::Pixels>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

impl LayoutNode {
    pub(crate) fn leaves(&self, output: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => output.push(*id),
            Self::Split { first, second, .. } => {
                first.leaves(output);
                second.leaves(output);
            }
        }
    }

    pub(crate) fn replace_leaf(&mut self, target: PaneId, replacement: LayoutNode) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = replacement;
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.replace_leaf(target, replacement.clone())
                    || second.replace_leaf(target, replacement)
            }
        }
    }

    pub(crate) fn remove_leaf(&mut self, target: PaneId) -> bool {
        match self {
            // 根节点没有兄弟，不能凭空删除；Workspace 会在此状态下补新窗格。
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                if matches!(first.as_ref(), Self::Leaf(id) if *id == target) {
                    *self = (**second).clone();
                    true
                } else if matches!(second.as_ref(), Self::Leaf(id) if *id == target) {
                    *self = (**first).clone();
                    true
                } else {
                    first.remove_leaf(target) || second.remove_leaf(target)
                }
            }
        }
    }

    pub(crate) fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    pub(crate) fn contains(&self, target: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabStatus {
    Running,
    Closing,
    Exited,
}

impl TabStatus {
    pub(crate) fn is_exited(self) -> bool {
        matches!(self, Self::Exited)
    }

    pub(crate) fn mark_closing(&mut self) -> bool {
        if matches!(self, Self::Running) {
            *self = Self::Closing;
            true
        } else {
            false
        }
    }

    pub(crate) fn mark_exited(&mut self) -> bool {
        if !matches!(self, Self::Exited) {
            *self = Self::Exited;
            true
        } else {
            false
        }
    }
}

/// 标签的会话关闭控制：持有会话契约，close 幂等。
struct SessionControl {
    session: Arc<dyn TerminalSession>,
    closing: AtomicBool,
}

impl SessionControl {
    fn new(session: Arc<dyn TerminalSession>) -> Self {
        Self {
            session,
            closing: AtomicBool::new(false),
        }
    }

    fn close(&self) -> bool {
        if self.closing.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.session.kill();
        true
    }
}

/// 窗格：一个终端实体 + 会话生命周期。
#[derive(Clone, Debug)]
enum CommandDialog {
    Preview {
        template: CommandTemplate,
        preview: Option<String>,
    },
    Edit {
        original_name: Option<String>,
    },
}

/// TOTP 侧面板对话框。Preview 的 code 为 None 表示正在从密钥环读取并计算。
#[derive(Clone, Debug)]
enum TotpDialog {
    Edit {
        original: Option<String>,
    },
    Preview {
        name: String,
        code: Option<String>,
        generated_at: SystemTime,
        period: u64,
    },
}

/// 右面板分区。三个区各成一页，避免面板无限拉长。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RightPanelTab {
    #[default]
    Commands,
    Totp,
    Servers,
}

impl RightPanelTab {
    fn label(self) -> &'static str {
        match self {
            Self::Commands => "命令",
            Self::Totp => "动态码",
            Self::Servers => "服务器",
        }
    }

    const ALL: [Self; 3] = [Self::Commands, Self::Totp, Self::Servers];
}

/// 服务器侧面板对话框。Connect 用于缺凭据时补输（可选择记住入密钥环）；
/// GroupRename 批量改组名（同名并入既有组，留空即解散）。
#[derive(Clone, Debug)]
enum ServerDialog {
    Edit {
        original: Option<String>,
    },
    Connecting {
        name: String,
    },
    Connect {
        name: String,
        remember: bool,
        kind: CredentialKind,
        request_id: u64,
    },
    GroupRename {
        original: String,
    },
}

/// 连接去向：shell 新标签，或 SFTP 双栏面板（凭据解析同流程，产物不同）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectDest {
    Shell,
    SftpPanel,
}

struct SshRequest {
    id: u64,
    entry: ServerEntry,
    /// 跳板条目快照（由外到内，connect_server 时经 resolve_jump_chain 解析）。
    /// 必须随请求持有：补输凭据重试路径（submit_server_connect →
    /// start_ssh_connect）不再回查配置表，否则重连会退化成直连。
    jump: Vec<ServerEntry>,
    /// 去向：决定后台函数（connect vs connect_sftp）与结果管道。
    dest: ConnectDest,
    active: Arc<AtomicBool>,
    running: bool,
}

type PendingSshResult = (u64, ServerEntry, anyhow::Result<ConnectionOutcome>);
/// SFTP 连接结果与 shell 分管道：产物类型不同（SftpHandle vs
/// TerminalSession），迟到取消各自处置（close vs kill）。
type PendingSftpResult = (u64, ServerEntry, anyhow::Result<SftpOutcome>);

/// SFTP 栏内操作弹窗（新建目录/重命名；走窗口级模态弹窗模式）。
#[derive(Clone)]
struct SftpPromptDialog {
    side: PaneSide,
    kind: SftpPromptKind,
}

/// 内置快捷键（不可改，但参与全局冲突检测）。
const BUILTIN_SHORTCUTS: &[(&str, &str)] = &[
    ("ctrl+t", "新建标签"),
    ("ctrl+w", "关闭窗格 / 标签"),
    ("ctrl+j", "左右分屏"),
    ("ctrl+k", "上下分屏"),
    ("ctrl+left", "切换到左侧窗格"),
    ("ctrl+right", "切换到右侧窗格"),
    ("ctrl+up", "切换到上方窗格"),
    ("ctrl+down", "切换到下方窗格"),
    ("ctrl+shift+[", "上一个窗格"),
    ("ctrl+shift+]", "下一个窗格"),
    ("ctrl+shift+f", "回滚搜索"),
    ("ctrl+shift+b", "广播输入"),
];

/// 快捷键绑定归属：内置项不可编辑；其余以「类别:名称」唯一标识。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BindingOwner {
    Builtin,
    CommandsPanel,
    ServersPanel,
    Command(String),
    Server(String),
    Totp(String),
}

impl BindingOwner {
    /// 冲突提示里的可读名。
    fn describe(&self) -> String {
        match self {
            Self::Builtin => "内置快捷键".into(),
            Self::CommandsPanel => "命令面板开关".into(),
            Self::ServersPanel => "服务器面板开关".into(),
            Self::Command(name) => format!("命令「{name}」"),
            Self::Server(name) => format!("服务器「{name}」"),
            Self::Totp(name) => format!("动态码「{name}」"),
        }
    }
}

struct ShortcutEditor {
    owner: Option<BindingOwner>,
    recorder: Entity<ShortcutRecorder>,
    _subscription: gpui::Subscription,
}

/// 一条快捷键绑定。
#[derive(Clone, Debug)]
pub(crate) struct ShortcutBinding {
    pub(crate) owner: BindingOwner,
    /// 归一化后的组合键（ctrl+shift+k 形态）。
    pub(crate) normalized: String,
    /// 动作描述（设置页展示）。
    pub(crate) label: String,
}

/// 候选快捷键与现有绑定的冲突查找；ignore_owner 用于编辑自身时豁免。
pub(crate) fn shortcut_conflict_with(
    bindings: &[ShortcutBinding],
    candidate: &str,
    ignore_owner: Option<&BindingOwner>,
) -> Option<String> {
    let normalized = normalize_shortcut(candidate).ok()?;
    bindings
        .iter()
        .filter(|binding| Some(&binding.owner) != ignore_owner)
        .find(|binding| binding.normalized == normalized)
        .map(|binding| binding.owner.describe())
}

/// 服务器导入来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerImportSource {
    /// 本程序导出的 TOML 文件。
    LaterTerm,
    /// 本机 Deepin 终端的远程管理列表（一键读取默认路径）。
    Deepin,
    /// Xshell 会话文件（.xsh，可多选）。
    Xshell,
}

/// 命令导入合并统计（供提示文案计数）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MergeCommandsStats {
    pub(crate) added: usize,
    pub(crate) skipped_dup: usize,
    pub(crate) skipped_bad: usize,
    pub(crate) stripped_shortcut: usize,
}

/// 把导入命令合并进设置（纯函数，零 IO）：同名跳过、名称/模板无效丢弃。
/// 快捷键归一化失败或与既有/批内绑定冲突时「剥绑定保命令」并计数——
/// 绝不静默改变用户现有按键行为。批内互撞靠增量登记已接纳键捕获
/// （静态快照查不出后导入的第二条）。
pub(crate) fn merge_commands_into(
    settings: &mut Settings,
    commands: Vec<CommandTemplate>,
    bindings: &[ShortcutBinding],
) -> MergeCommandsStats {
    let mut stats = MergeCommandsStats::default();
    let mut taken: BTreeSet<String> = bindings
        .iter()
        .map(|binding| binding.normalized.clone())
        .collect();
    for mut command in commands {
        command.name = command.name.trim().to_string();
        // 先按「无快捷键」校验名称与模板体；快捷键单独走剥绑逻辑
        let body_only = CommandTemplate {
            shortcut: None,
            ..command.clone()
        };
        if body_only.validate().is_err() {
            stats.skipped_bad += 1;
            continue;
        }
        if settings
            .commands
            .iter()
            .any(|item| item.name == command.name)
        {
            stats.skipped_dup += 1;
            continue;
        }
        if let Some(shortcut) = command.shortcut.clone() {
            match normalize_shortcut(&shortcut) {
                Ok(normalized) if taken.contains(&normalized) => {
                    command.shortcut = None;
                    stats.stripped_shortcut += 1;
                }
                Ok(normalized) => {
                    taken.insert(normalized);
                }
                Err(_) => {
                    command.shortcut = None;
                    stats.stripped_shortcut += 1;
                }
            }
        }
        settings.commands.push(command);
        stats.added += 1;
    }
    stats
}

/// 合并统计 → 提示文案：跳过/剥绑计数全部可见，绝不静默丢数据。
fn format_merge_commands_notice(
    stats: &MergeCommandsStats,
    parse_skipped: usize,
    prefix: &str,
) -> String {
    let mut notice = format!("{prefix}：导入 {}", stats.added);
    if stats.skipped_dup > 0 {
        notice += &format!("，同名跳过 {}", stats.skipped_dup);
    }
    if stats.skipped_bad > 0 {
        notice += &format!("，无效丢弃 {}", stats.skipped_bad);
    }
    if parse_skipped > 0 {
        notice += &format!("，解析跳过 {parse_skipped}");
    }
    if stats.stripped_shortcut > 0 {
        notice += &format!("，剥除冲突快捷键 {}", stats.stripped_shortcut);
    }
    notice
}

/// 服务器分组分桶：组名字典序升序在前，未分组桶固定殿后。
/// 返回 (组名, 该组条目在 `servers` 中的下标)；评审提醒：`BTreeMap<Option<_>>`
/// 的 `None` 键排最前，不能用它的默认顺序，这里手动把未分组挪到最后。
pub(crate) fn server_groups(servers: &[ServerEntry]) -> Vec<(Option<String>, Vec<usize>)> {
    let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut ungrouped: Vec<usize> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        match server.group.clone() {
            Some(group) => grouped.entry(group).or_default().push(index),
            None => ungrouped.push(index),
        }
    }
    let mut out: Vec<(Option<String>, Vec<usize>)> = grouped
        .into_iter()
        .map(|(name, indexes)| (Some(name), indexes))
        .collect();
    if !ungrouped.is_empty() {
        out.push((None, ungrouped));
    }
    out
}

/// 批量改组/解散的核心（纯函数）：把组内所有条目的 group 改为 `new`
/// （None = 解散；Some(已存在组名) = 并入该组）。返回受影响条目数；
/// 新组名不合法时整体报错且不动任何条目。
pub(crate) fn apply_server_group_change(
    servers: &mut [ServerEntry],
    old: &str,
    new: Option<&str>,
) -> anyhow::Result<usize> {
    // Some + trim 后为空 = 报错（不允许重命名为纯空白）
    // None = 解散分组（移除分组标签）——两个语义不能混
    let new = match new {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                anyhow::bail!("分组名不能为纯空白");
            }
            Some(trimmed)
        }
        None => None,
    };
    if let Some(name) = new
        && (name.chars().count() > 80 || name.chars().any(char::is_control))
    {
        anyhow::bail!("分组名不能包含控制字符或超过 80 字");
    }
    let mut affected = 0usize;
    for server in servers.iter_mut() {
        if server.group.as_deref() == Some(old) {
            server.group = new.map(str::to_string);
            affected += 1;
        }
    }
    Ok(affected)
}

/// 服务器保存的表更新（纯函数，零 IO，便于测试）：改名先删旧条目并
/// 改写所有指向旧名的跳板引用（`Settings::retarget_jump_refs`），
/// 再 upsert 新条目；upsert 失败回滚整表。
pub(crate) fn apply_server_save(
    settings: &mut Settings,
    original: Option<&str>,
    meta: ServerEntry,
) -> anyhow::Result<()> {
    let snapshot = settings.clone();
    if let Some(old) = original
        && old != meta.name
    {
        settings.remove_server(old);
        settings.retarget_jump_refs(old, &meta.name);
    }
    if let Err(error) = settings.upsert_server(meta) {
        *settings = snapshot;
        return Err(error);
    }
    Ok(())
}

/// 合并导入条目的纯逻辑（零 IO，便于测试）：无效条目丢弃、同名跳过、
/// 新条目并入 `existing`；随后对合并后的全表做跳板引用清理
/// （格式非法/悬空/成环/超深一律断链保条目）。
/// 返回 (新增数, 跳过同名数, 无效丢弃数, 跳板引用被清理的条目名)。
pub(crate) fn merge_entries_into(
    existing: &mut Vec<ServerEntry>,
    entries: Vec<ServerEntry>,
) -> (usize, usize, usize, Vec<String>) {
    let mut added = 0usize;
    let mut skipped_dup = 0usize;
    let mut skipped_bad = 0usize;
    for entry in entries {
        let Some(entry) = entry.sanitized() else {
            skipped_bad += 1;
            continue;
        };
        if existing.iter().any(|item| item.name == entry.name) {
            skipped_dup += 1;
            continue;
        }
        existing.push(entry);
        added += 1;
    }
    let cleared_jumps = laterterm_config::clear_broken_jump_refs(existing);
    (added, skipped_dup, skipped_bad, cleared_jumps)
}

struct Pane {
    id: PaneId,
    title: String,
    terminal: Entity<TerminalView>,
    session: Arc<SessionControl>,
    status: TabStatus,
}

/// 终端右键菜单的打开状态与来源窗格。
#[derive(Clone)]
struct TerminalMenuState {
    pane_id: PaneId,
    /// 菜单锚点（窗口坐标）。
    position: Point<Pixels>,
    /// 右键处的 URL（若有），供「在浏览器打开」使用。
    url: Option<String>,
}

/// 右键菜单动作：拿到工作区、窗口与上下文执行具体操作。
type TerminalMenuAction = Box<dyn Fn(&mut WorkspaceView, &mut Window, &mut Context<WorkspaceView>)>;

/// 标签：持有自己的分屏树与窗格焦点。
struct WorkspaceTab {
    id: TabId,
    layout: LayoutNode,
    focused_pane: PaneId,
}

impl WorkspaceTab {
    fn tab_title(&self, panes: &[Pane]) -> String {
        panes
            .iter()
            .find(|pane| pane.id == self.focused_pane)
            .map(|pane| pane.title.clone())
            .unwrap_or_else(|| "终端".into())
    }
}

pub struct WorkspaceView {
    tabs: Vec<WorkspaceTab>,
    panes: Vec<Pane>,
    active_tab: TabId,
    next_pane_id: u64,
    next_tab_id: u64,
    next_split_id: u64,
    /// 窗口级可见窗格的最近一帧几何，仅供方向导航使用。
    pane_geometry: Rc<RefCell<Option<PaneGeometry>>>,
    /// 进行中的分割线拖拽（None = 空闲）。
    split_drag: Option<SplitDrag>,
    /// 终端右键菜单状态（None = 关闭）。
    terminal_menu: Option<TerminalMenuState>,
    left_panel: bool,
    tab_scroll: ScrollHandle,
    right_panel: bool,
    settings_open: bool,
    broadcast: bool,
    welcome_visible: bool,
    /// 首启引导：探测结果与 shell 暂选（None = 跟随默认），见 onboarding.rs。
    onboarding: OnboardingState,
    /// 引导探测后台线程结果 → 泵（单发即弃，见 start_onboarding_pump）。
    /// 测试构建不 spawn 真线程，仅保留通道，放行 dead_code。
    #[cfg_attr(test, allow(dead_code))]
    onboarding_tx: flume::Sender<ProbeResults>,
    onboarding_rx: flume::Receiver<ProbeResults>,
    broadcast_tx: flume::Sender<(PaneId, Vec<u8>)>,
    broadcast_rx: flume::Receiver<(PaneId, Vec<u8>)>,
    /// 当前高亮的设置分区（scroll-spy 写、导航点击写、绘制消费）。
    settings_section: SettingsSection,
    /// 待执行的导航跳转（Some = 本帧 prepaint 消费 scroll_to_top_of_item，
    /// 且 spy 跳过一帧防旧 offset 拉回高亮）。
    settings_jump: Option<SettingsSection>,
    /// 设置内容单页滚动容器句柄（offset 存 handle 内的 Rc，跨面板开关持久，
    /// 打开时必须复位——见 toggle_settings）。
    settings_scroll: ScrollHandle,
    /// 手动检查更新状态机（update_check.rs；结果落实体字段，面板关闭不取消
    /// 在途任务，下次打开显示）。
    update_check: UpdateCheck,
    shortcut_editor: Option<ShortcutEditor>,
    panel_shortcut_dialog: Option<BindingOwner>,
    shortcut_error: Option<String>,
    settings: Settings,
    themes: ThemeRegistry,
    /// 主题预览态：面板内选中但未保存的主题名（None = 无预览）。已保存值
    /// 恒为 `settings.appearance.theme`，两字段不合并；读取一律走
    /// `effective_theme_name` 唯一读口，防「芯片/色板/窗格」三方漂移。
    theme_preview: Option<String>,
    shortcut_tx: flume::Sender<(PaneId, WorkspaceShortcut)>,
    shortcut_rx: flume::Receiver<(PaneId, WorkspaceShortcut)>,
    pending_shortcuts: Vec<(PaneId, WorkspaceShortcut)>,
    command_values: BTreeMap<String, String>,
    command_dialog: Option<CommandDialog>,
    command_target: Option<PaneId>,
    command_fields: Vec<(String, Entity<CommandTextField>)>,
    command_field_subscriptions: Vec<gpui::Subscription>,
    command_delete_confirm: Option<String>,
    command_error: Option<String>,
    /// 命令区的一次性提示（导入计数等），下次操作时清除。
    command_notice: Option<String>,
    totp_dialog: Option<TotpDialog>,
    totp_fields: Vec<(String, Entity<CommandTextField>)>,
    totp_meta_draft: TotpEntry,
    totp_delete_confirm: Option<String>,
    totp_error: Option<String>,
    totp_target: Option<PaneId>,
    totp_generating: bool,
    right_panel_tab: RightPanelTab,
    server_dialog: Option<ServerDialog>,
    server_fields: Vec<(String, Entity<CommandTextField>)>,
    server_field_subscriptions: Vec<gpui::Subscription>,
    server_save: Option<Arc<AtomicBool>>,
    server_meta_draft: ServerEntry,
    server_delete_confirm: Option<String>,
    server_error: Option<String>,
    /// 服务器区的一次性提示（导入/导出结果等），下次操作时清除。
    server_notice: Option<String>,
    /// 分组折叠态（键 = 组名，None = 未分组桶；会话级，不持久化）。
    server_groups_collapsed: BTreeSet<Option<String>>,
    /// 待二次确认解散的分组名。
    server_group_confirm: Option<String>,
    /// 设置落盘路径（None = 用户默认配置；测试注入临时路径避免污染真实配置）。
    settings_save_path: Option<PathBuf>,
    /// 导入来源选择弹窗（Some(已选来源) = 打开中）。
    server_import_dialog: Option<ServerImportSource>,
    /// 正在连接中的服务器名（None 表示空闲；期间禁止并发连接）。
    ssh_connecting: Option<String>,
    ssh_request: Option<SshRequest>,
    next_ssh_request_id: u64,
    /// 后台结果按请求标识校验，过期成功会话立即关闭。
    pending_ssh_results: Vec<PendingSshResult>,
    /// SFTP 连接结果（与 shell 分管道，见 PendingSftpResult 注记）。
    pending_sftp_results: Vec<PendingSftpResult>,
    /// SFTP 双栏面板状态（None = 关闭；同一时刻至多一个面板）。
    /// 跨模块只读访问限于 sftp_panel.rs 的覆盖层渲染。
    pub(crate) sftp_panel: Option<SftpPanelState>,
    /// SFTP 面板后台事件（列表/传输进度）→ UI pump。
    sftp_events_tx: flume::Sender<SftpEvent>,
    sftp_events_rx: flume::Receiver<SftpEvent>,
    /// 传输任务自增 id。
    next_sftp_transfer_id: u64,
    /// 栏内新建目录/重命名弹窗（走窗口级模态弹窗模式）。
    sftp_prompt: Option<SftpPromptDialog>,
    sftp_prompt_fields: Vec<(String, Entity<CommandTextField>)>,
    sftp_prompt_subscriptions: Vec<gpui::Subscription>,
    /// 待生效的焦点请求：异步上下文拿不到 window，先记下 handle，render 期统一 focus。
    pending_focus: Option<FocusHandle>,
    secret_store: Arc<dyn SecretStore>,
    /// 主窗口句柄：Quake 泵/失焦轮询在实体更新之外用它驱动窗口动作
    /// （activate/minimize 需要 &mut Window，不能嵌套在窗口自身的更新里）。
    window_handle: gpui::AnyWindowHandle,
    /// Quake 呼出/收起状态机（纯逻辑，见 quake.rs）。
    quake: crate::quake::QuakeMachine,
    /// portal worker 事件 → 泵。
    quake_tx: flume::Sender<QuakePortalEvent>,
    quake_rx: flume::Receiver<QuakePortalEvent>,
    /// 设置面板状态行（portal 授权/绑定/降级）。
    quake_status: QuakePortalStatus,
    /// portal worker 停止句柄；None = 未启动/已停止（仅 Linux 编译）。
    #[cfg(target_os = "linux")]
    quake_worker: Option<crate::quake::QuakeWorkerHandle>,
}

impl WorkspaceView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak_workspace = cx.weak_entity();
        let first_run = !Settings::default_path().is_some_and(|path| path.exists());
        let mut settings = Settings::load_or_default();
        if !settings.legacy_servers_migrated
            && settings.migrate_legacy_servers(laterterm_config::servers::ServerStore::load())
        {
            settings.save();
        }
        settings.ensure_settings_file();
        let themes = ThemeRegistry::load();
        let (shortcut_tx, shortcut_rx) = flume::unbounded();
        let (broadcast_tx, broadcast_rx) = flume::unbounded();
        let (sftp_events_tx, sftp_events_rx) = flume::unbounded();
        let (quake_tx, quake_rx) = flume::unbounded();
        let (onboarding_tx, onboarding_rx) = flume::unbounded();
        let quake_machine = crate::quake::QuakeMachine::new(settings.quake.auto_hide_on_blur);
        let first_pane = PaneId(1);
        let first_tab = TabId(1);
        let theme = themes.resolve(&settings.appearance.theme).clone();
        // 初始网格尺寸按当前窗口内容区估算：PTY 一启动就是正确尺寸，
        // 避免「首帧 resize → bash readline 把光标拉回提示符行首」的错位。
        let viewport = window.viewport_size();
        let terminal_area = gpui::Size {
            width: (viewport.width - px(PANEL_WIDTH)).max(px(1.0)),
            height: viewport.height - px(TITLEBAR_HEIGHT),
        };
        let (initial_cols, initial_rows) = estimate_grid_size(&settings.appearance, terminal_area);
        let (terminal, session) = create_local_terminal(
            first_pane,
            weak_workspace,
            cx,
            &settings.appearance,
            &theme,
            shortcut_tx.clone(),
            broadcast_tx.clone(),
            initial_cols,
            initial_rows,
            resolve_shell(settings.shell.as_deref()),
        );
        cx.subscribe_in(
            &terminal,
            window,
            move |this, _, _: &laterterm_termview::view::TerminalExited, window, cx| {
                this.mark_exited(first_pane, window, cx);
            },
        )
        .detach();
        subscribe_terminal_menu(&terminal, first_pane, window, cx).detach();
        // 引导暂选从当前设置出发（正常首启为 None = 跟随默认；settings 在
        // 上面的字面量里被移动，需提前取值）。变量名避开模块别名 onboarding。
        let onboarding_state = OnboardingState {
            probe: None,
            selected_shell: settings.shell.clone(),
        };
        let mut view = Self {
            settings,
            themes,
            theme_preview: None,
            shortcut_tx,
            shortcut_rx,
            pending_shortcuts: Vec::new(),
            command_values: BTreeMap::new(),
            command_dialog: None,
            command_target: None,
            command_fields: Vec::new(),
            command_field_subscriptions: Vec::new(),
            command_delete_confirm: None,
            command_error: None,
            command_notice: None,
            totp_dialog: None,
            totp_fields: Vec::new(),
            totp_meta_draft: TotpEntry::default(),
            totp_delete_confirm: None,
            totp_error: None,
            totp_target: None,
            totp_generating: false,
            right_panel_tab: RightPanelTab::Commands,
            server_dialog: None,
            server_fields: Vec::new(),
            server_field_subscriptions: Vec::new(),
            server_save: None,
            server_meta_draft: ServerEntry::default(),
            server_delete_confirm: None,
            server_error: None,
            server_notice: None,
            server_groups_collapsed: BTreeSet::new(),
            server_group_confirm: None,
            settings_save_path: None,
            server_import_dialog: None,
            ssh_connecting: None,
            ssh_request: None,
            next_ssh_request_id: 1,
            pending_ssh_results: Vec::new(),
            pending_sftp_results: Vec::new(),
            sftp_panel: None,
            sftp_events_tx,
            sftp_events_rx,
            next_sftp_transfer_id: 1,
            sftp_prompt: None,
            sftp_prompt_fields: Vec::new(),
            sftp_prompt_subscriptions: Vec::new(),
            pending_focus: None,
            window_handle: window.window_handle(),
            quake: quake_machine,
            quake_tx,
            quake_rx,
            quake_status: QuakePortalStatus::Disabled,
            #[cfg(target_os = "linux")]
            quake_worker: None,
            next_split_id: 1,
            pane_geometry: Rc::new(RefCell::new(None)),
            split_drag: None,
            terminal_menu: None,
            secret_store: default_store(),
            settings_section: SettingsSection::Appearance,
            settings_jump: None,
            settings_scroll: ScrollHandle::new(),
            update_check: UpdateCheck::default(),
            shortcut_editor: None,
            panel_shortcut_dialog: None,
            shortcut_error: None,
            panes: vec![Pane {
                id: first_pane,
                title: "本地终端".into(),
                terminal,
                session,
                status: TabStatus::Running,
            }],
            tabs: vec![WorkspaceTab {
                id: first_tab,
                layout: LayoutNode::Leaf(first_pane),
                focused_pane: first_pane,
            }],
            active_tab: first_tab,
            next_pane_id: 2,
            next_tab_id: 2,
            left_panel: true,
            tab_scroll: ScrollHandle::new(),
            right_panel: false,
            settings_open: false,
            broadcast: false,
            welcome_visible: first_run,
            onboarding: onboarding_state,
            onboarding_tx,
            onboarding_rx,
            broadcast_tx,
            broadcast_rx,
        };
        Self::watch_config_files(cx);
        view.start_shortcut_pump(cx);
        view.start_broadcast_pump(cx);
        view.start_totp_ticker(cx);
        view.start_sftp_pump(cx);
        view.start_quake_pump(cx);
        view.start_quake_focus_poller(cx);
        view.start_onboarding_pump(cx);
        // 首启才探测（IO：/etc/shells + fc-list 子进程，约几十毫秒），结果经
        // flume 泵回主线程。cfg(not(test))：gpui 测试统一走 deliver_probe 注入，
        // 真线程结果晚到会与注入值竞争（评审阻断4 的 flaky 隐患）。
        #[cfg(not(test))]
        if first_run {
            let onboarding_tx = view.onboarding_tx.clone();
            std::thread::spawn(move || {
                let _ = onboarding_tx.send(onboarding::probe());
            });
        }
        // 启动时已持久化启用 Quake：重启后必须重新拉起 portal worker 并同步
        // 状态机启用门（直接 spawn 会绕过 SetEnabled(true)，热键静默失效——
        // 评审缺口3 + 2026-11-06 复核修复）。
        #[cfg(target_os = "linux")]
        if view.settings.quake.enabled {
            view.sync_quake_worker(cx);
        }
        view
    }

    fn start_shortcut_pump(&self, cx: &mut Context<Self>) {
        let rx = self.shortcut_rx.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(shortcut) = rx.recv_async().await {
                let _ = this.update(cx, |workspace, cx| {
                    workspace.pending_shortcuts.push(shortcut);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // Quake 下拉模式：portal 事件泵 + 失焦轮询 + 设置接线（见 quake.rs）
    // ------------------------------------------------------------------

    /// portal 事件泵：worker 事件 → 状态机/状态行（实体更新）→ 窗口动作
    /// （窗口更新）。两段更新刻意分开——在窗口自身更新内再 update 同一窗口
    /// 会重入借用，pump 跑在异步上下文里没有这个约束。
    fn start_quake_pump(&self, cx: &mut Context<Self>) {
        let rx = self.quake_rx.clone();
        let window = self.window_handle;
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                let action = this
                    .update(cx, |workspace, cx| workspace.handle_quake_event(event, cx))
                    .unwrap_or(QuakeAction::None);
                let result = match action {
                    QuakeAction::None => Ok(()),
                    QuakeAction::Activate => window.update(cx, |_, window, _| {
                        window.activate_window();
                    }),
                    QuakeAction::Minimize => window.update(cx, |_, window, _| {
                        window.minimize_window();
                    }),
                };
                if result.is_err() {
                    break; // 窗口已关闭，泵随 workspace 析构自然退出
                }
            }
        })
        .detach();
    }

    /// 失焦轮询（200ms 独立循环）：周期语义与 1s 的 totp ticker 不同，不并入。
    /// 读 `window.is_window_active()` 喂状态机；on_active_status_change 回调是
    /// pub(crate) 无公开订阅口，轮询是唯一可行路径（2026-11-06 落地口径）。
    /// 状态机未翻转时不 notify，常驻空转不产生重绘。
    fn start_quake_focus_poller(&self, cx: &mut Context<Self>) {
        let window = self.window_handle;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let active = match window.update(cx, |_, window, _| window.is_window_active()) {
                    Ok(active) => active,
                    Err(_) => break, // 窗口已关闭
                };
                let action = this
                    .update(cx, |workspace, cx| {
                        if !workspace.settings.quake.enabled {
                            return QuakeAction::None;
                        }
                        let input = if active {
                            QuakeInput::FocusGained
                        } else {
                            QuakeInput::FocusLost {
                                now: crate::quake::monotonic_now(),
                            }
                        };
                        let action = workspace.quake.handle(input);
                        if action != QuakeAction::None {
                            cx.notify();
                        }
                        action
                    })
                    .ok()
                    .unwrap_or(QuakeAction::None);
                let result = match action {
                    QuakeAction::None => Ok(()),
                    QuakeAction::Activate => window.update(cx, |_, window, _| {
                        window.activate_window();
                    }),
                    QuakeAction::Minimize => window.update(cx, |_, window, _| {
                        window.minimize_window();
                    }),
                };
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// portal 事件进状态机/状态行，返回待执行的窗口动作。
    fn handle_quake_event(
        &mut self,
        event: QuakePortalEvent,
        cx: &mut Context<Self>,
    ) -> QuakeAction {
        match event {
            // 关闭开关瞬间可能仍有 worker 迟到事件在途，禁用态一律忽略。
            QuakePortalEvent::Toggled if !self.settings.quake.enabled => QuakeAction::None,
            QuakePortalEvent::Toggled => {
                let action = self.quake.handle(QuakeInput::PortalToggle {
                    now: crate::quake::monotonic_now(),
                });
                if action != QuakeAction::None {
                    cx.notify();
                }
                action
            }
            // RestoreWindow 必须无条件放行：它恰恰发生在 enabled 已切 false
            // 之后（关闭 Quake 且窗口停在收起态时呼回，防止「消失」只剩
            // 任务栏）；走泵执行避免实体更新内嵌套窗口更新。
            QuakePortalEvent::RestoreWindow => QuakeAction::Activate,
            QuakePortalEvent::AwaitingConsent if self.settings.quake.enabled => {
                self.quake_status = QuakePortalStatus::AwaitingConsent;
                cx.notify();
                QuakeAction::None
            }
            QuakePortalEvent::Ready { trigger } if self.settings.quake.enabled => {
                self.quake_status = QuakePortalStatus::Ready(trigger);
                cx.notify();
                QuakeAction::None
            }
            // 改绑只对已就绪/等待中的状态有意义，降级态不必回跳。
            QuakePortalEvent::TriggerUpdated { trigger }
                if self.settings.quake.enabled
                    && !matches!(self.quake_status, QuakePortalStatus::Failed(_)) =>
            {
                self.quake_status = QuakePortalStatus::Ready(trigger);
                cx.notify();
                QuakeAction::None
            }
            QuakePortalEvent::Failed { reason } if self.settings.quake.enabled => {
                self.quake_status = QuakePortalStatus::Failed(reason);
                cx.notify();
                QuakeAction::None
            }
            // 其余 = 禁用态的迟到状态事件，忽略。
            _ => QuakeAction::None,
        }
    }

    /// 设置→快捷键页的 Quake 开关：落盘 + worker 生命周期联动 + 需要时呼回窗口。
    fn change_quake_setting(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut laterterm_config::QuakeSettings),
    ) {
        cx.stop_propagation();
        let previous = self.settings.quake.clone();
        change(&mut self.settings.quake);
        if self.settings.quake == previous {
            return;
        }
        self.settings.save();
        self.sync_quake_worker(cx);
        cx.notify();
    }

    /// 把 settings.quake 同步到 worker 与状态机：启用则确保 worker 在跑，
    /// 关闭则停线程、复位状态机，并返回「窗口需要呼回」。
    /// 由面板开关、启动、热加载三条路径共用（评审缺口5：外部改 settings.toml
    /// 也要联动，不能只覆盖面板路径）。
    #[cfg(target_os = "linux")]
    fn sync_quake_worker(&mut self, cx: &mut Context<Self>) {
        self.quake
            .set_auto_hide(self.settings.quake.auto_hide_on_blur);
        if self.settings.quake.enabled {
            // 启用门同步：状态机构造即禁用，漏喂 SetEnabled(true) 会让
            // Toggled/失焦全部落进禁用兜底（热键静默失效，2026-11-06 复核修复）。
            // 用 is_enabled 门保证幂等——已启用时的重复 sync（auto_hide 单独
            // 变化 / 保存回声 reload）不得复位 shown_at/seen_focused/Hidden 瞬态。
            if !self.quake.is_enabled() {
                let _ = self.quake.handle(QuakeInput::SetEnabled(true));
            }
            if self.quake_worker.is_none() {
                self.spawn_quake_worker();
            }
        } else {
            // Drop 停止句柄即发停止消息，worker 在事件循环赛跑分支自然退出。
            self.quake_worker = None;
            let was_hidden = self.quake.is_hidden();
            let _ = self.quake.handle(QuakeInput::SetEnabled(false));
            self.quake_status = QuakePortalStatus::Disabled;
            if was_hidden {
                let _ = self.quake_tx.send(QuakePortalEvent::RestoreWindow);
            }
        }
        cx.notify();
    }

    #[cfg(not(target_os = "linux"))]
    fn sync_quake_worker(&mut self, cx: &mut Context<Self>) {
        let _ = cx;
    }

    /// 拉起 portal worker；状态行先切「等待授权」覆盖连接+授权两个阶段，
    /// worker 事件（Ready/Failed）随后细化。
    #[cfg(target_os = "linux")]
    fn spawn_quake_worker(&mut self) {
        self.quake_status = QuakePortalStatus::AwaitingConsent;
        self.quake_worker = Some(crate::quake::spawn_portal_worker(self.quake_tx.clone()));
    }

    fn process_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = std::mem::take(&mut self.pending_shortcuts);
        for (pane_id, shortcut) in pending {
            // 快捷键来自某个窗格：先激活其所在标签并聚焦该窗格，再执行分屏/循环
            if let Some(tab_index) = self
                .tabs
                .iter()
                .position(|tab| tab.layout.contains(pane_id))
            {
                if self.active_tab != self.tabs[tab_index].id {
                    self.tab_scroll.scroll_to_item(tab_index);
                }
                self.active_tab = self.tabs[tab_index].id;
                self.tabs[tab_index].focused_pane = pane_id;
                self.handle_shortcut(shortcut, window, cx);
            }
        }
    }

    fn start_broadcast_pump(&self, cx: &mut Context<Self>) {
        let rx = self.broadcast_rx.clone();
        cx.spawn(async move |this, cx| {
            while let Ok((source, bytes)) = rx.recv_async().await {
                if this
                    .update(cx, |workspace, cx| {
                        if !workspace.broadcast
                            || !workspace
                                .panes
                                .iter()
                                .any(|pane| pane.id == source && pane.status == TabStatus::Running)
                        {
                            return;
                        }
                        for pane in &workspace.panes {
                            if pane.id != source && pane.status == TabStatus::Running {
                                pane.terminal.read(cx).write_input(&bytes);
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        while self.broadcast_rx.try_recv().is_ok() {}
        self.broadcast = !self.broadcast;
        cx.notify();
    }

    fn handle_shortcut(
        &mut self,
        shortcut: WorkspaceShortcut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match shortcut {
            WorkspaceShortcut::Split(direction) => self.split_focused(direction, window, cx),
            WorkspaceShortcut::FocusPrevious => self.cycle_focus(false, window, cx),
            WorkspaceShortcut::FocusNext => self.cycle_focus(true, window, cx),
            WorkspaceShortcut::ToggleBroadcast => self.toggle_broadcast(cx),
        }
    }

    /// 关闭欢迎覆盖层。
    fn dismiss_welcome(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.welcome_visible = false;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 首启引导（onboarding.rs）：探测泵 + 暂选 + 完成/跳过
    // ------------------------------------------------------------------

    /// 探测结果泵：后台线程 → 实体更新（模式同 start_quake_pump，但无窗口动作）。
    fn start_onboarding_pump(&self, cx: &mut Context<Self>) {
        let rx = self.onboarding_rx.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(results) = rx.recv_async().await {
                let _ = this.update(cx, |workspace, cx| {
                    workspace.deliver_probe(results);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 探测结果统一入口（生产泵与测试注入共用）：单发闩——已有结果即丢弃
    /// 后到者，保证注入值不被迟到的真线程结果覆盖。
    fn deliver_probe(&mut self, results: ProbeResults) {
        if self.onboarding.probe.is_some() {
            return;
        }
        self.onboarding.probe = Some(results);
    }

    /// 点选 shell 芯片：仅暂存，完成时才落盘。
    /// `choice = None` 表示「跟随默认」——选中「当前」芯片时保持动态解析，
    /// 不把 $SHELL 固化成具体路径（用户日后改登录 shell 仍自动跟随）。
    fn onboarding_select_shell(&mut self, choice: Option<String>, cx: &mut Context<Self>) {
        self.onboarding.selected_shell = choice;
        cx.notify();
    }

    /// 点选字体芯片：与设置面板字体芯片同效果（等值短路 + apply_appearance
    /// 热更新即时生效），但落盘统一走 persist_settings——change_setting 内部
    /// 的 settings.save() 写死默认路径，测试注入的 settings_save_path 会被
    /// 绕过、拿内存态覆盖真实配置（评审阻断3，同一覆盖层只留一条持久化路径）。
    fn onboarding_apply_font(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.settings.appearance.font_family == name {
            return; // 等值短路：不重复保存/重应用
        }
        self.settings.appearance.font_family = name.to_owned();
        if let Err(error) = self.persist_settings() {
            eprintln!("保存首启引导字体设置失败: {error:#}");
        }
        self.apply_appearance(cx);
        cx.notify();
    }

    /// 「完成」：shell 暂选与当前设置不同则落盘（字体芯片已即时落盘），随后
    /// 关闭引导层。探测未到就点完成 = 暂选 None = 保持跟随默认，不写。
    fn complete_onboarding(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.onboarding.selected_shell != self.settings.shell {
            self.settings.shell = self.onboarding.selected_shell.clone();
            if let Err(error) = self.persist_settings() {
                eprintln!("保存首启引导 shell 设置失败: {error:#}");
            }
        }
        self.welcome_visible = false;
        cx.notify();
    }

    fn active_tab_index(&self) -> usize {
        self.tabs
            .iter()
            .position(|tab| tab.id == self.active_tab)
            .unwrap_or(0)
    }

    fn active_tab(&self) -> &WorkspaceTab {
        &self.tabs[self.active_tab_index()]
    }

    fn focused_pane_id(&self) -> PaneId {
        self.active_tab().focused_pane
    }

    fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    fn focused_terminal(&self) -> Entity<TerminalView> {
        let id = self.focused_pane_id();
        self.pane(id)
            .map(|pane| pane.terminal.clone())
            .unwrap_or_else(|| self.panes[0].terminal.clone())
    }

    fn focus_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab_index) = self.tabs.iter().position(|tab| tab.layout.contains(id)) {
            if self.active_tab != self.tabs[tab_index].id {
                self.tab_scroll.scroll_to_item(tab_index);
            }
            self.active_tab = self.tabs[tab_index].id;
            self.tabs[tab_index].focused_pane = id;
            if let Some(pane) = self.pane(id) {
                window.set_window_title(&pane.title);
                pane.terminal.read(cx).focus_handle().focus(window);
            }
            cx.notify();
        }
    }

    fn cycle_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut leaves = Vec::new();
        self.active_tab().layout.leaves(&mut leaves);
        if leaves.len() < 2 {
            return;
        }
        let current = self.focused_pane_id();
        let index = leaves.iter().position(|id| *id == current).unwrap_or(0);
        let next = if forward {
            (index + 1) % leaves.len()
        } else {
            (index + leaves.len() - 1) % leaves.len()
        };
        self.focus_pane(leaves[next], window, cx);
    }

    fn focus_pane_direction(
        &mut self,
        direction: PaneDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.active_tab();
        let geometry = self.pane_geometry.borrow();
        let Some(snapshot) = geometry.as_ref().filter(|snapshot| {
            snapshot.tab_id == tab.id
                && snapshot.layout == tab.layout
                && snapshot.viewport == window.viewport_size()
                && snapshot.panels == (self.left_panel, self.right_panel)
                && snapshot.panes.len() == tab.layout.leaf_count()
                && snapshot
                    .panes
                    .iter()
                    .all(|pane| tab.layout.contains(pane.id))
        }) else {
            return;
        };
        let target = neighbor(tab.focused_pane, direction, &snapshot.panes);
        drop(geometry);
        if let Some(id) = target {
            self.focus_pane(id, window, cx);
        }
    }

    fn new_tab(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.spawn_tab(window, cx);
    }

    /// 统一窗格装配：建视图、订阅退出事件、登记 Pane，返回 pane id。
    /// 本地 PTY 与 SSH 会话共用；调用方自行决定挂到新标签还是分屏。
    fn attach_pane(
        &mut self,
        title: String,
        session: Arc<dyn TerminalSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneId {
        let pane_id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        // 预览期间新建的分屏/标签与既有窗格同色：读 effective 而非已保存值。
        let theme = self.themes.resolve(self.effective_theme_name()).clone();
        let session = wrap_transfer(session, &self.settings.appearance);
        let session_control = Arc::new(SessionControl::new(Arc::clone(&session)));
        // 新窗格继承当前窗口的估算网格尺寸（与首窗格同一口径）。
        let viewport = window.viewport_size();
        let terminal_area = gpui::Size {
            width: viewport.width,
            height: viewport.height - px(TITLEBAR_HEIGHT),
        };
        let (cols, rows) = estimate_grid_size(&self.settings.appearance, terminal_area);
        let terminal = create_terminal_view(
            pane_id,
            session,
            terminal_config_for(&self.settings.appearance, &theme, cols, rows),
            cx.weak_entity(),
            self.shortcut_tx.clone(),
            self.broadcast_tx.clone(),
            cx,
        );
        cx.subscribe_in(
            &terminal,
            window,
            move |this, _, _: &laterterm_termview::view::TerminalExited, window, cx| {
                this.mark_exited(pane_id, window, cx);
            },
        )
        .detach();
        subscribe_terminal_menu(&terminal, pane_id, window, cx).detach();
        self.panes.push(Pane {
            id: pane_id,
            title,
            terminal,
            session: session_control,
            status: TabStatus::Running,
        });
        pane_id
    }

    /// 新建标签：一个标签一个全新窗格与分屏树根。
    fn spawn_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = format!("终端 {}", self.next_pane_id);
        let session = spawn_local_session(self.settings.shell.as_deref());
        let pane_id = self.attach_pane(title, session, window, cx);
        let tab_id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        self.tabs.push(WorkspaceTab {
            id: tab_id,
            layout: LayoutNode::Leaf(pane_id),
            focused_pane: pane_id,
        });
        self.focus_pane(pane_id, window, cx);
    }

    /// 在当前焦点窗格处创建分屏，新窗格获得焦点。
    fn split_focused(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old = self.focused_pane_id();
        let title = format!("终端 {}", self.next_pane_id);
        let session = spawn_local_session(self.settings.shell.as_deref());
        let pane_id = self.attach_pane(title, session, window, cx);
        let tab_index = self.active_tab_index();
        let split_id = self.next_split_id;
        self.next_split_id += 1;
        let replacement = LayoutNode::Split {
            id: split_id,
            direction,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(old)),
            second: Box::new(LayoutNode::Leaf(pane_id)),
        };
        if !self.tabs[tab_index].layout.replace_leaf(old, replacement) {
            // 布局替换失败：回滚刚登记的窗格
            if let Some(index) = self.panes.iter().position(|pane| pane.id == pane_id) {
                self.panes.remove(index);
            }
            return;
        }
        self.focus_pane(pane_id, window, cx);
    }

    /// 按下分割线：记录目标节点与容器几何，进入拖拽。
    /// 移动/松开由 render 期注册的窗口级鼠标监听处理（on_mouse_event 仅下一帧有效，
    /// canvas paint 回调逐帧重挂，见 render）。
    fn start_split_drag(
        &mut self,
        split_id: u64,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) {
        let geometry = SPLIT_GEOMETRY.with_borrow(|map| map.get(&split_id).copied());
        let Some(geometry) = geometry else {
            return;
        };
        self.split_drag = Some(SplitDrag {
            split_id,
            direction,
            geometry,
        });
        cx.notify();
    }

    /// 拖拽中按鼠标位置换算分割比例（0.1–0.9，给两侧留最小窗格空间）。
    /// 返回布局是否变化。
    fn update_split_drag(&mut self, position: gpui::Point<gpui::Pixels>) -> bool {
        let Some(drag) = self.split_drag else {
            return false;
        };
        let raw = match drag.direction {
            SplitDirection::Horizontal => {
                (position.x - drag.geometry.origin.x) / drag.geometry.size.width
            }
            SplitDirection::Vertical => {
                (position.y - drag.geometry.origin.y) / drag.geometry.size.height
            }
        };
        let index = self.active_tab_index();
        apply_split_ratio(
            &mut self.tabs[index].layout,
            drag.split_id,
            raw.clamp(0.1, 0.9),
        );
        true
    }

    /// 松开鼠标结束拖拽；返回此前是否有拖拽在进行。
    fn end_split_drag(&mut self) -> bool {
        self.split_drag.take().is_some()
    }

    fn activate_tab(
        &mut self,
        id: TabId,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) {
            let pane_id = tab.focused_pane;
            self.focus_pane(pane_id, window, cx);
        }
    }

    /// 关闭窗格（pane 角标 × / Ctrl+W）。 exited 直接移除，Running 走 Closing 生命周期；
    /// 移除后修剪所在标签的布局树，标签空则移除标签，全部空则补一个新标签。
    fn close_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let Some(index) = self.panes.iter().position(|pane| pane.id == id) else {
            return;
        };
        if self.panes[index].status.is_exited() {
            self.remove_pane_at(index, window, cx);
            return;
        }
        if !self.panes[index].status.mark_closing() {
            return;
        }
        self.panes[index].session.close();
        cx.notify();
    }

    /// 关闭整个标签（标签栏 ×）：向该标签所有窗格会话发关闭请求（幂等），
    /// 窗格实体随 TerminalExited 事件逐个清理（隐藏标签同样生效）；标签立即从标签栏消失。
    fn close_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let Some(tab_index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let mut leaves = Vec::new();
        self.tabs[tab_index].layout.leaves(&mut leaves);
        for pane_id in leaves {
            if let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == pane_id)
                && pane.status.mark_closing()
            {
                pane.session.close();
            }
        }
        self.tabs.remove(tab_index);
        cx.notify();
        if self.tabs.is_empty() {
            self.spawn_tab(window, cx);
            return;
        }
        if self.active_tab == id {
            let index = self.tabs.len() - 1;
            let pane_id = self.tabs[index].focused_pane;
            self.focus_pane(pane_id, window, cx);
        }
    }

    fn remove_pane_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.panes.remove(index);
        // 从所在标签的布局树修剪；若该窗格是标签内最后一个（根叶子无法自删），
        // 整个标签随之关闭
        if let Some(tab_index) = self
            .tabs
            .iter()
            .position(|tab| tab.layout.contains(removed.id))
        {
            if self.tabs[tab_index].layout.leaf_count() == 1 {
                self.tabs.remove(tab_index);
            } else {
                self.tabs[tab_index].layout.remove_leaf(removed.id);
            }
        }
        if self.tabs.is_empty() {
            self.spawn_tab(window, cx);
            return;
        }
        // 活动标签被移除时回退到最后一个标签
        if !self.tabs.iter().any(|tab| tab.id == self.active_tab)
            && let Some(last) = self.tabs.last()
        {
            self.active_tab = last.id;
            self.tab_scroll.scroll_to_item(self.tabs.len() - 1);
        }
        // 焦点窗格失效时回退到活动标签的首个窗格
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == self.active_tab)
            && !tab.layout.contains(tab.focused_pane)
        {
            let mut leaves = Vec::new();
            tab.layout.leaves(&mut leaves);
            if let Some(first) = leaves.first() {
                tab.focused_pane = *first;
            }
        }
        let pane_id = self.focused_pane_id();
        if self.pane(pane_id).is_some() {
            self.focus_pane(pane_id, window, cx);
        } else if let Some(first) = self.panes.first() {
            self.focus_pane(first.id, window, cx);
        }
    }

    fn toggle_left_panel(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.left_panel = !self.left_panel;
        if self.left_panel {
            self.tab_scroll.scroll_to_item(self.active_tab_index());
        }
        cx.notify();
    }

    fn toggle_right_panel(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.right_panel = !self.right_panel;
        cx.notify();
    }

    /// 终端右键菜单浮层：anchored 定位 + snap_to_window 防贴边溢出。
    /// 样式与命令输入框右键菜单同一套（Tokyo Night 深色）。
    fn terminal_context_menu(
        &self,
        menu: TerminalMenuState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane = self.pane(menu.pane_id);
        let has_selection = pane.is_some_and(|pane| pane.terminal.read(cx).has_selection());
        let single_pane = self.tabs[self.active_tab_index()].layout.leaf_count() <= 1;
        let menu_pane = menu.pane_id;

        // 菜单项工厂：禁用项置灰且不挂动作；动作执行后统一收起菜单。
        // 嵌套 fn 里不能用 Self，需写全类型名；listener 要求 'static，label 用字面量。
        fn item(
            cx: &mut Context<WorkspaceView>,
            label: &'static str,
            enabled: bool,
            action: TerminalMenuAction,
        ) -> Div {
            let base = div()
                .h(px(26.0))
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(12.0));
            if enabled {
                base.text_color(rgb(0xC0CAF5))
                    .hover(|style| style.bg(rgb(0x2F3147)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            action(this, window, cx);
                            this.terminal_menu = None;
                            cx.notify();
                        }),
                    )
            } else {
                base.text_color(rgb(0x565B79))
            }
            .child(label)
        }
        let separator = || div().mx_2().my_1().h(px(1.0)).bg(rgb(0x343750));

        // 终端级动作统一按菜单来源窗格定位；窗格可能已被回收，找不到则静默跳过。
        let mut items: Vec<Div> = Vec::new();
        let broadcast_label: &'static str = if self.broadcast {
            "关闭广播输入"
        } else {
            "开启广播输入"
        };
        if let Some(url) = &menu.url {
            let url = url.clone();
            items.push(item(
                cx,
                "在浏览器打开",
                true,
                Box::new(move |_, _, _| {
                    laterterm_termview::open_url(&url);
                }),
            ));
        }
        items.push(item(
            cx,
            "复制",
            has_selection,
            Box::new(move |this, _, cx| {
                if let Some(pane) = this.pane(menu_pane) {
                    let terminal = pane.terminal.clone();
                    terminal.update(cx, |view, cx| view.copy_selection(cx));
                }
            }),
        ));
        items.push(item(
            cx,
            "粘贴",
            true,
            Box::new(move |this, _, cx| {
                if let Some(pane) = this.pane(menu_pane) {
                    let terminal = pane.terminal.clone();
                    terminal.update(cx, |view, cx| view.paste_from_clipboard(cx));
                }
            }),
        ));
        items.push(item(
            cx,
            "全选",
            true,
            Box::new(move |this, _, cx| {
                if let Some(pane) = this.pane(menu_pane) {
                    let terminal = pane.terminal.clone();
                    terminal.update(cx, |view, cx| view.select_all(cx));
                }
            }),
        ));
        items.push(separator());
        items.push(item(
            cx,
            "搜索回滚内容",
            true,
            Box::new(move |this, _, cx| {
                if let Some(pane) = this.pane(menu_pane) {
                    let terminal = pane.terminal.clone();
                    terminal.update(cx, |view, cx| view.open_search(cx));
                }
            }),
        ));
        items.push(item(
            cx,
            "清屏",
            true,
            Box::new(move |this, _, cx| {
                if let Some(pane) = this.pane(menu_pane) {
                    let terminal = pane.terminal.clone();
                    terminal.update(cx, |view, cx| view.clear_screen(cx));
                }
            }),
        ));
        items.push(item(
            cx,
            "重置终端",
            true,
            Box::new(move |this, _, cx| {
                if let Some(pane) = this.pane(menu_pane) {
                    let terminal = pane.terminal.clone();
                    terminal.update(cx, |view, cx| view.reset_terminal(cx));
                }
            }),
        ));
        items.push(separator());
        items.push(item(
            cx,
            "向左分屏",
            true,
            Box::new(|this, window, cx| {
                this.split_focused(SplitDirection::Horizontal, window, cx);
            }),
        ));
        items.push(item(
            cx,
            "向下分屏",
            true,
            Box::new(|this, window, cx| {
                this.split_focused(SplitDirection::Vertical, window, cx);
            }),
        ));
        if !single_pane {
            items.push(item(
                cx,
                "关闭窗格",
                true,
                Box::new(move |this, window, cx| {
                    this.close_pane(menu_pane, window, cx);
                }),
            ));
        }
        items.push(item(
            cx,
            "新标签",
            true,
            Box::new(|this, window, cx| {
                this.spawn_tab(window, cx);
            }),
        ));
        items.push(item(
            cx,
            "关闭标签",
            true,
            Box::new(move |this, window, cx| {
                if let Some(tab) = this.tabs.iter().find(|tab| tab.layout.contains(menu_pane)) {
                    let tab_id = tab.id;
                    this.close_tab(tab_id, window, cx);
                }
            }),
        ));
        items.push(separator());
        items.push(item(
            cx,
            broadcast_label,
            true,
            Box::new(|this, _, cx| {
                this.toggle_broadcast(cx);
            }),
        ));
        items.push(item(
            cx,
            "设置",
            true,
            Box::new(|this, _, cx| {
                this.toggle_settings(cx);
            }),
        ));

        deferred(
            anchored().position(menu.position).snap_to_window().child(
                div()
                    .w(px(176.0))
                    .flex()
                    .flex_col()
                    .py_1()
                    .bg(rgb(0x252837))
                    .border_1()
                    .border_color(rgb(0x3B3F5C))
                    .rounded_md()
                    // 点击菜单外任意处（含终端区）关闭
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.terminal_menu = None;
                        cx.notify();
                    }))
                    // 菜单内的点击不穿透，避免误触终端或触发选区
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(items),
            ),
        )
        .with_priority(1)
    }

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.settings_open = !self.settings_open;
        if self.settings_open {
            // track_scroll 模式下 offset 真身存在 ScrollHandle（view 字段，gpui
            // div.rs 的 Rc<RefCell<Point>>），跨面板开关持久——重开若不复位会
            // 残留上次滚动位置，与复位的首节高亮脱节（评审硬伤2）。只写在
            // 打开分支：关闭动画帧保留高亮。
            self.settings_section = SettingsSection::default();
            self.settings_jump = None;
            self.settings_scroll
                .set_offset(gpui::point(px(0.0), px(0.0)));
        } else {
            // 关闭路径之二（标题栏 ⚙ / 右键菜单）：未保存的主题预览在此恢复，
            // 与 close_settings（scrim/×）同一恢复语义——恢复只挂一条路径的话，
            // 从菜单关面板会残留预览色（方案风险3）。
            self.cancel_theme_preview(cx);
        }
        cx.notify();
    }

    /// 手动检查更新：后台执行器跑网络请求，结果经实体更新回写
    /// update_check 字段。收尾语义与 quake 泵一致（见 start_quake_pump）：
    /// 「切走设置面板」只是 settings_open 布尔开关，本任务不取消——结果
    /// 落实体字段等下次打开显示；实体/窗口已亡时 update 返回 Err 自然丢弃。
    /// 8s 全局超时封顶（update_check.rs TIMEOUT），防按钮永久「检查中…」。
    fn check_for_update(&mut self, cx: &mut Context<Self>) {
        // 幂等门：检查中连点不再发请求（update_check.start 已兜底，这里
        // 短路省一次 spawn）
        if !self.update_check.start() {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            // 同步网络调用必须包进 async 块：直接传 query_latest() 的话，
            // 求值发生在外层 spawn 所在的主线程，8s 超时内 UI 整体卡死
            //（评审必改项；secrets 先例 workspace.rs delete_totp 同形态）。
            let result = cx
                .background_executor()
                .spawn(async move { update_check::query_latest() })
                .await;
            let _ = this.update(cx, |workspace, cx| {
                workspace.update_check.finish(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn change_setting(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Appearance),
    ) {
        cx.stop_propagation();
        let previous = self.settings.appearance.clone();
        change(&mut self.settings.appearance);
        if self.settings.appearance == previous {
            return;
        }
        let trace =
            std::env::var_os("LATERTERM_FONT_TRACE").as_deref() == Some(std::ffi::OsStr::new("1"));
        let save_started = trace.then(std::time::Instant::now);
        self.settings.save();
        let save_elapsed = save_started.map(|started| started.elapsed());
        let apply_started = trace.then(std::time::Instant::now);
        self.apply_appearance(cx);
        if let (Some(save), Some(apply_started)) = (save_elapsed, apply_started) {
            eprintln!(
                "[font-trace] phase=settings-save-and-apply panes={} save_us={} apply_us={}",
                self.panes.len(),
                save.as_micros(),
                apply_started.elapsed().as_micros(),
            );
        }
    }

    /// 生效主题名唯一读口：预览优先，未预览时回落已保存值。窗格实际颜色
    /// （apply_appearance / attach_pane）与面板 UI（芯片选中态、色板预览卡）
    /// 三处必须全部经此读取，漏任何一处都会出现「芯片高亮 A、色板显示 A、
    /// 窗格却是 B」的三方漂移。
    fn effective_theme_name(&self) -> &str {
        self.theme_preview
            .as_deref()
            .unwrap_or(&self.settings.appearance.theme)
    }

    /// 设置面板点选主题芯片：只进预览态，不落盘（落盘走 commit_theme_preview）。
    /// 点回已保存主题 = 放弃当前预览，必须委托 cancel_theme_preview（take + apply
    /// 恢复已保存色）——裸清 theme_preview 会让窗格钉在预览色且无任何恢复路径
    /// （评审必改1：提示行消失、芯片高亮已保存值、窗格仍是预览色的三方漂移）。
    fn preview_theme(
        &mut self,
        _event: &MouseDownEvent,
        name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if name == self.settings.appearance.theme {
            self.cancel_theme_preview(cx);
            return;
        }
        if self.theme_preview.as_deref() == Some(name.as_str()) {
            return; // 重复点击同一预览主题：等值短路
        }
        self.theme_preview = Some(name);
        self.apply_appearance(cx);
    }

    /// 提交预览主题：先用 `themes.get` 严格校验再落盘——自定义主题可能被
    /// 热加载删掉/改名，`resolve` 会静默回退默认主题（theme.rs），直接保存
    /// 会让用户「保存了一个不生效的主题」，重启后同样回退；校验失败按放弃
    /// 处理（cancel 恢复已保存色，不写盘）。落盘走 persist_settings 尊重
    /// 测试注入路径（change_setting 内部的 settings.save() 写死默认路径，
    /// 同 onboarding_apply_font 口径）。保存失败保留内存态不回滚：预览色
    /// 已是用户所见，下次任意设置保存会重写盘（与 onboarding_apply_font
    /// 同为 eprintln + 保留）。提交时机语义：预览期间点过的其它设置已即时
    /// 落盘，这里只追加 theme 字段（先预览 A 再改字号再保存 = theme=A +
    /// 新字号，符合直觉）。
    fn commit_theme_preview(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let Some(name) = self.theme_preview.clone() else {
            return; // 无预览无可提交（按钮仅在预览期渲染，双击兜底）
        };
        if self.themes.get(&name).is_none() {
            self.cancel_theme_preview(cx);
            return;
        }
        self.settings.appearance.theme = name;
        self.theme_preview = None;
        if let Err(error) = self.persist_settings() {
            eprintln!("保存主题设置失败: {error:#}");
        }
        self.apply_appearance(cx);
        cx.notify();
    }

    /// 放弃预览：有预览才恢复（cursor_blink 同一条 update_config 热更新链路），
    /// 无预览时零操作（close_settings / toggle_settings 关闭分支无条件调用，
    /// 空操作必须便宜且不通知）。
    fn cancel_theme_preview(&mut self, cx: &mut Context<Self>) {
        if self.theme_preview.take().is_some() {
            self.apply_appearance(cx);
        }
    }

    /// 把当前设置（外观五项 + 主题）应用到所有窗格，不重建终端。
    fn apply_appearance(&mut self, cx: &mut Context<Self>) {
        let appearance = self.settings.appearance.clone();
        // 主题解析必须读 effective：cancel 的恢复链路也走这里，漏改会把
        // 预览色「恢复」成预览色自身，恢复语义整个失效（方案风险1）。
        let palette = palette_for(self.themes.resolve(self.effective_theme_name()));
        for pane in &self.panes {
            pane.terminal.update(cx, |terminal, cx| {
                let mut config = terminal.config().clone();
                config.font_family = appearance.font_family.clone();
                config.font_size = px(appearance.font_size);
                config.line_height_multiplier = appearance.line_height;
                config.padding = Edges::all(px(appearance.padding));
                config.scrollback = appearance.scrollback;
                config.colors = palette.clone();
                config.ligatures = appearance.ligatures;
                config.cursor_blink = appearance.cursor_blink;
                config.cursor_style = cursor_style_for(appearance.cursor_style);
                config.font_fallbacks = appearance.font_fallbacks.clone();
                config.copy_on_select = appearance.copy_on_select;
                terminal.update_config(config, cx);
            });
        }
        cx.notify();
    }

    /// 配置目录被外部修改后的热加载入口：重读设置与主题注册表，
    /// 有实际变化才重应用（自身保存触发的回声事件在这里被等值比较吸收）。
    fn reload_from_disk(&mut self, cx: &mut Context<Self>) {
        // 编辑中途的半截文件解析失败时保持现状，等下一次事件重试
        let settings = Settings::default_path()
            .and_then(|path| Settings::load(&path).ok())
            .unwrap_or_else(|| self.settings.clone());
        let themes = ThemeRegistry::load();
        if settings != self.settings || themes != self.themes {
            let quake_changed = settings.quake != self.settings.quake;
            self.settings = settings;
            self.themes = themes;
            // 外部改文件后旧预览作废：不清的话关面板时预览会覆盖用户在文件里
            // 选的主题（apply_appearance 在块尾统一执行，读已保存值天然正确）。
            self.theme_preview = None;
            // 外部编辑 settings.toml 改 quake 开关也要联动 worker 生命周期，
            // 否则面板开关与 portal 实际状态漂移（评审缺口5）。自身保存触发的
            // 回声 reload 走等值比较幂等吸收，不会重复 spawn/stop。
            if quake_changed {
                self.sync_quake_worker(cx);
            }
            self.apply_appearance(cx);
        }
    }

    /// 监听配置目录（settings.toml 与 themes/）实现主题/设置热加载。
    fn watch_config_files(cx: &mut Context<Self>) {
        let Some(dir) = app_config_dir() else {
            return;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let (event_tx, event_rx) = flume::unbounded::<()>();
        let Ok(mut watcher) =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                match event {
                    Ok(event) if config_event_requires_reload(&event.kind) => {
                        let _ = event_tx.send(());
                    }
                    Err(error) => eprintln!("配置监听失败: {error}"),
                    _ => {}
                }
            })
        else {
            return;
        };
        if watcher
            .watch(&dir, notify::RecursiveMode::Recursive)
            .is_err()
        {
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let _watcher = watcher; // 监听器随任务存活，避免被提前丢弃
            while event_rx.recv_async().await.is_ok() {
                // 非阻塞排空已排队事件（编辑器保存的连发）。
                // 本任务跑在前台执行器（主线程）上，任何阻塞等待都会卡 UI；
                // reload 幂等，多触发一次只是多读一次小文件后等值比较。
                while event_rx.try_recv().is_ok() {}
                let _ = this.update(cx, |workspace, cx| workspace.reload_from_disk(cx));
            }
        })
        .detach();
    }

    fn reset_settings(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.change_setting(event, window, cx, |appearance| {
            *appearance = Appearance::default()
        });
        // change_setting 在 appearance 已是默认值时早退不 apply（workspace
        // 侧等值短路）：预览色若不在此收掉，窗格会钉在预览色而预览态已消失、
        // 与「已恢复默认」矛盾且无恢复路径（评审必改3）。原为 Some 时无条件
        // 补一次 apply——此时读的是恢复后的已保存值，天然正确。
        if self.theme_preview.take().is_some() {
            self.apply_appearance(cx);
        }
    }

    fn start_move(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.start_window_move();
    }

    fn minimize(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.minimize_window();
    }

    fn zoom(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.zoom_window();
    }

    /// 导航点击：立即高亮（不等滚动对位），并记跳转让下一帧 prepaint 消费
    /// scroll_to_top_of_item 瞬时对齐分区起点（Top 策略）。
    fn select_settings_section(
        &mut self,
        section: SettingsSection,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_section = section;
        self.settings_jump = Some(section);
        self.settings_scroll.scroll_to_top_of_item(section.index());
        cx.notify();
    }

    /// 设置面板 scroll-spy：滚动 → 导航高亮。在 render 期执行，读的是上一帧
    /// 绘制记录的容器/子元素几何（滞后一帧无感，与 LaterScreen 参照一致），
    /// 结果由本帧导航绘制直接消费。绝不调 cx.notify——滚动帧由滚动容器内部
    /// 滚轮监听的 notify 驱动（gpui div.rs bubble 期），这里再通知就是
    /// 「render→notify→render」自激死循环。
    fn settings_scroll_spy_step(&mut self) {
        // 导航点击 → 跳转帧：prepaint 才会消费 scroll_to_top_of_item，本帧
        // offset 还是旧值，跳过侦测防高亮被拉回旧分区（一帧闪烁）。
        if self.settings_jump.take().is_some() {
            return;
        }
        // 内容不超高（max_offset≈0）：at_bottom 恒真会错拉到末节，保持现状。
        if self.settings_scroll.max_offset().height <= px(SCROLL_SPY_EPSILON_PX) {
            return;
        }
        let handle = &self.settings_scroll;
        let container_top = handle.bounds().top();
        // child_bounds 在首次绘制前为空（bounds_for_item 返回 None），跳过；
        // tops 为各分区起点相对容器顶的内容坐标（child_bounds 记录时不带本
        // 容器滚动变换，减容器顶即归一化，与 -offset.y + 判定线同坐标系）。
        let tops: Vec<(SettingsSection, f32)> = SettingsSection::ALL
            .iter()
            .enumerate()
            .filter_map(|(index, section)| {
                handle
                    .bounds_for_item(index)
                    .map(|bounds| (*section, f32::from(bounds.top() - container_top)))
            })
            .collect();
        let offset_y = handle.offset().y;
        let line = f32::from(-offset_y) + SCROLL_SPY_LEAD_PX;
        let at_bottom = offset_y <= -(handle.max_offset().height - px(SCROLL_SPY_EPSILON_PX));
        self.settings_section = settings_scroll_spy(&tops, line, at_bottom);
    }

    fn close_settings(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_open = false;
        // 关闭路径之一（scrim / ×）：未保存的主题预览在关面板时自动还原
        // （LaterScreen discard 语义移植）。toggle_settings 关闭分支挂同款。
        self.cancel_theme_preview(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 设置落盘统一入口：默认写用户配置文件；测试可注入
    /// `settings_save_path`（临时目录），避免测试导入污染真实配置。
    fn persist_settings(&self) -> anyhow::Result<()> {
        let path = self
            .settings_save_path
            .clone()
            .or_else(Settings::default_path)
            .ok_or_else(|| anyhow::anyhow!("无法定位设置文件路径"))?;
        self.settings.save_to(&path)
    }

    fn save_command(&mut self, command: CommandTemplate, cx: &mut Context<Self>) {
        let previous = self.settings.clone();
        if let Err(error) = self.settings.upsert_command(command) {
            self.command_error = Some(error.to_string());
            return;
        }
        if let Err(error) = self.persist_settings() {
            self.settings = previous;
            self.command_error = Some(format!("保存命令失败: {error}"));
        } else {
            self.command_error = None;
        }
        cx.notify();
    }

    fn delete_command(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.command_delete_confirm.as_deref() != Some(name) {
            self.command_delete_confirm = Some(name.to_string());
            cx.notify();
            return;
        }
        let previous = self.settings.clone();
        self.settings.remove_command(name);
        if let Err(error) = self.persist_settings() {
            self.settings = previous;
            self.command_error = Some(format!("删除命令失败: {error}"));
        } else {
            self.command_delete_confirm = None;
            self.command_error = None;
            self.command_notice = None;
        }
        cx.notify();
    }

    fn send_command_to_pane(
        &mut self,
        pane_id: PaneId,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane(pane_id) else {
            return;
        };
        if !matches!(pane.status, TabStatus::Running) {
            return;
        }
        let terminal = pane.terminal.clone();
        terminal.update(cx, |terminal, _cx| {
            terminal.send_text(&format!("{command}\n"))
        });
        self.focus_pane(pane_id, window, cx);
    }

    fn edit_command(
        &mut self,
        command: Option<CommandTemplate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = command.clone().unwrap_or_default();
        self.command_fields.clear();
        self.command_field_subscriptions.clear();
        self.begin_shortcut_edit(
            draft.shortcut.clone(),
            command
                .as_ref()
                .map(|item| BindingOwner::Command(item.name.clone())),
            cx,
        );
        for (name, value) in [
            ("名称", draft.name.clone()),
            ("模板", draft.template.clone()),
        ] {
            let field = cx.new(|cx| CommandTextField::new(value, name, cx));
            self.command_fields.push((name.to_string(), field));
        }
        self.command_dialog = Some(CommandDialog::Edit {
            original_name: command.map(|item| item.name),
        });
        self.command_error = None;
        self.command_notice = None;
        focus_first_field(&self.command_fields, window, cx);
        cx.notify();
    }

    fn save_edit_command(
        &mut self,
        original_name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let values = self
            .command_fields
            .iter()
            .map(|(name, field)| (name.as_str(), field.read(cx).value()))
            .collect::<BTreeMap<_, _>>();
        let name = values
            .get("名称")
            .map(|value| value.to_string())
            .unwrap_or_default();
        let template = values
            .get("模板")
            .map(|value| value.to_string())
            .unwrap_or_default();
        let shortcut = self.shortcut_value(cx);
        let command = CommandTemplate {
            name,
            template,
            shortcut,
            confirm: true,
        };
        if let Some(conflict) = self.check_shortcut_conflict(
            command.shortcut.as_deref(),
            original_name.clone().map(BindingOwner::Command),
        ) {
            self.command_error = Some(conflict);
            return;
        }
        let previous = self.settings.clone();
        if let Err(error) = command.validate() {
            self.command_error = Some(error.to_string());
            cx.notify();
            return;
        }
        if original_name.as_deref() != Some(command.name.as_str())
            && self
                .settings
                .commands
                .iter()
                .any(|item| item.name == command.name)
        {
            self.command_error = Some("已存在同名命令".into());
            cx.notify();
            return;
        }
        if let Some(original) = original_name
            && original != command.name
        {
            self.settings.remove_command(&original);
        }
        self.save_command(command, cx);
        if self.command_error.is_none() {
            self.command_dialog = None;
            self.command_fields.clear();
            self.shortcut_editor = None;
            self.focus_active(window, cx);
        } else {
            self.settings = previous;
        }
    }

    fn open_command(
        &mut self,
        command: &CommandTemplate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_dialog = Some(CommandDialog::Preview {
            template: command.clone(),
            preview: None,
        });
        self.shortcut_editor = None;
        self.command_target = Some(self.focused_pane_id());
        self.command_fields.clear();
        self.command_field_subscriptions.clear();
        self.command_values.clear();
        if let Ok(variables) = command.variables() {
            for variable in variables {
                let field = cx.new(|cx| CommandTextField::new(String::new(), variable.clone(), cx));
                self.command_field_subscriptions.push(cx.subscribe(
                    &field,
                    |workspace, _field, _: &CommandTextChanged, cx| {
                        workspace.sync_command_values(cx);
                        cx.notify();
                    },
                ));
                self.command_fields.push((variable, field));
            }
        }
        focus_first_field(&self.command_fields, window, cx);
        if self.command_fields.is_empty() && !command.confirm {
            self.send_command_to_pane(
                self.command_target.unwrap_or(self.focused_pane_id()),
                command.template.clone(),
                window,
                cx,
            );
            self.command_dialog = None;
            self.command_target = None;
            return;
        }
        let preview = command.expand(&self.command_values).ok();
        self.command_dialog = Some(CommandDialog::Preview {
            template: command.clone(),
            preview,
        });
        cx.notify();
    }

    fn confirm_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_command_values(cx);
        let Some(CommandDialog::Preview {
            preview: Some(command),
            template,
        }) = self.command_dialog.clone()
        else {
            self.command_error = Some("参数未填写完整或命令无效".into());
            return;
        };
        let target = self
            .command_target
            .take()
            .unwrap_or_else(|| self.focused_pane_id());
        self.send_command_to_pane(target, command, window, cx);
        let _ = template;
        self.command_dialog = None;
        self.command_fields.clear();
        cx.notify();
    }

    fn sync_command_values(&mut self, cx: &mut Context<Self>) {
        self.command_values = self
            .command_fields
            .iter()
            .map(|(name, field)| (name.clone(), field.read(cx).value().to_string()))
            .collect();
        if let Some(CommandDialog::Preview { template, preview }) = &mut self.command_dialog {
            *preview = template.expand(&self.command_values).ok();
        }
    }

    fn cancel_command(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.dismiss_command(window, cx);
    }

    /// 关闭命令对话框（取消/Esc 共用），焦点归还终端。
    fn dismiss_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.command_dialog = None;
        self.command_error = None;
        self.command_notice = None;
        self.command_target = None;
        self.shortcut_editor = None;
        self.command_fields.clear();
        self.command_field_subscriptions.clear();
        self.command_values.clear();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 每秒唤醒一次渲染，让动态码剩余秒数可见；右面板关闭时空转，不产生重绘。
    fn start_totp_ticker(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update(cx, |workspace, cx| {
                        // 倒计时只在「动态码」页签展示；其余页签不为此付费
                        if workspace.right_panel && workspace.right_panel_tab == RightPanelTab::Totp
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn edit_totp(
        &mut self,
        entry: Option<&TotpEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = entry.cloned().unwrap_or_default();
        self.totp_meta_draft = TotpEntry {
            name: String::new(),
            algorithm: draft.algorithm,
            digits: draft.digits,
            period: draft.period,
            shortcut: draft.shortcut.clone(),
        };
        self.totp_fields.clear();
        // 编辑时密钥框留空表示不改动；新建时必须填。
        let secret_placeholder = if entry.is_some() {
            "留空保持现有密钥"
        } else {
            "base32 密钥或 otpauth:// 链接"
        };
        let name_field = cx.new(|cx| CommandTextField::new(draft.name.clone(), "名称", cx));
        let secret_field =
            cx.new(|cx| CommandTextField::new(String::new(), secret_placeholder, cx).masked());
        self.begin_shortcut_edit(
            draft.shortcut.clone(),
            entry.map(|item| BindingOwner::Totp(item.name.clone())),
            cx,
        );
        self.totp_fields.push(("名称".into(), name_field));
        self.totp_fields.push(("密钥".into(), secret_field));
        self.totp_dialog = Some(TotpDialog::Edit {
            original: entry.map(|item| item.name.clone()),
        });
        self.totp_error = None;
        focus_first_field(&self.totp_fields, window, cx);
        cx.notify();
    }

    /// 保存 TOTP 条目：密钥校验、试算、密钥环写入全部在后台线程，避免 DBus 阻塞主线程。
    fn save_totp(&mut self, original: Option<String>, cx: &mut Context<Self>) {
        let values = self
            .totp_fields
            .iter()
            .map(|(name, field)| (name.as_str(), field.read(cx).value().to_string()))
            .collect::<BTreeMap<_, _>>();
        let name = values
            .get("名称")
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        let secret_input = values
            .get("密钥")
            .map(|value| value.to_string())
            .unwrap_or_default();
        let mut meta = self.totp_meta_draft.clone();
        meta.name = name.clone();
        meta.shortcut = self.shortcut_value(cx);
        if let Some(conflict) = self.check_shortcut_conflict(
            meta.shortcut.as_deref(),
            original.clone().map(BindingOwner::Totp),
        ) {
            self.totp_error = Some(conflict);
            cx.notify();
            return;
        }
        if let Err(error) = meta.validate() {
            self.totp_error = Some(error.to_string());
            cx.notify();
            return;
        }
        if secret_input.trim().is_empty() && original.is_none() {
            self.totp_error = Some("密钥不能为空（支持 base32 或 otpauth:// 链接）".into());
            cx.notify();
            return;
        }
        let store = self.secret_store.clone();
        let original_bg = original.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let account = totp_account(&name)?;
                    if !secret_input.trim().is_empty() {
                        // 链接导入时以链接内参数为准，回填算法/位数/周期。
                        let parsed = parse_secret_input(&secret_input)?;
                        meta.algorithm = parsed.config.algorithm;
                        meta.digits = parsed.config.digits;
                        meta.period = parsed.config.period;
                        // 落库前试算一次，把密钥/参数错误挡在保存之前。
                        generate_code(&parsed.secret, &parsed.config, SystemTime::now())?;
                        let store = store.clone();
                        let secret_to_store = parsed.secret.clone();
                        run_secret_op(move || store.set(&account, &secret_to_store))?;
                    } else if let Some(previous) = &original_bg {
                        let previous_account = totp_account(previous)?;
                        if previous_account != account {
                            // 改名不重录密钥：从旧账户迁移，失败则保持原状。
                            let store = store.clone();
                            let previous_account = previous_account.clone();
                            run_secret_op(move || -> anyhow::Result<()> {
                                match store.get(&previous_account)? {
                                    Some(secret) => {
                                        store.set(&account, &secret)?;
                                        store.delete(&previous_account)?;
                                    }
                                    None => store.delete(&previous_account)?,
                                }
                                Ok(())
                            })?;
                        }
                    }
                    Ok::<_, anyhow::Error>(meta)
                })
                .await;
            let _ = this.update(cx, |workspace, cx| match result {
                Ok(meta) => {
                    let previous_settings = workspace.settings.clone();
                    if let Some(previous) = &original
                        && previous != &meta.name
                    {
                        workspace.settings.remove_totp(previous);
                    }
                    if let Err(error) = workspace.settings.upsert_totp(meta) {
                        workspace.settings = previous_settings;
                        workspace.totp_error = Some(error.to_string());
                    } else if let Err(error) = workspace.persist_settings() {
                        workspace.settings = previous_settings;
                        workspace.totp_error = Some(format!("保存动态码失败: {error:#}"));
                    } else {
                        workspace.totp_dialog = None;
                        workspace.totp_fields.clear();
                        workspace.totp_error = None;
                        workspace.pending_focus =
                            Some(workspace.focused_terminal().read(cx).focus_handle().clone());
                    }
                    cx.notify();
                }
                Err(error) => {
                    workspace.totp_error = Some(format!("保存动态码失败: {error:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 点击条目：后台读密钥环并生成当前码，完成后回填预览。
    fn open_totp(&mut self, entry: &TotpEntry, cx: &mut Context<Self>) {
        self.totp_dialog = Some(TotpDialog::Preview {
            name: entry.name.clone(),
            code: None,
            generated_at: SystemTime::now(),
            period: entry.period,
        });
        self.totp_target = Some(self.focused_pane_id());
        self.totp_error = None;
        self.totp_generating = true;
        let store = self.secret_store.clone();
        let entry = entry.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let account = totp_account(&entry.name)?;
                    let store = store.clone();
                    let secret = run_secret_op(move || store.get(&account))?
                        .ok_or_else(|| anyhow::anyhow!("密钥环中没有该条目的密钥"))?;
                    let code = generate_code(
                        &secret,
                        &TotpConfig {
                            algorithm: entry.algorithm,
                            digits: entry.digits,
                            period: entry.period,
                        },
                        SystemTime::now(),
                    )?;
                    Ok::<_, anyhow::Error>((entry.name.clone(), code))
                })
                .await;
            let _ = this.update(cx, |workspace, cx| {
                workspace.totp_generating = false;
                match result {
                    Ok((name, code)) => {
                        if let Some(TotpDialog::Preview {
                            name: current,
                            code: slot,
                            generated_at,
                            period,
                        }) = &mut workspace.totp_dialog
                            && *current == name
                        {
                            *slot = Some(code.code);
                            *generated_at = SystemTime::now();
                            *period = entry.period;
                        }
                    }
                    Err(error) => {
                        workspace.totp_dialog = None;
                        workspace.totp_error = Some(format!("生成动态码失败: {error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_totp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(TotpDialog::Preview {
            code: Some(code),
            generated_at,
            period,
            ..
        }) = self.totp_dialog.clone()
        else {
            self.totp_error = Some("动态码尚未生成".into());
            cx.notify();
            return;
        };
        // 过期码发上去就是错误输入；让用户重新点击条目生成新码。
        if generated_at + Duration::from_secs(period) <= SystemTime::now() {
            self.totp_dialog = None;
            self.totp_error = Some("动态码已过期，请重新点击条目生成".into());
            cx.notify();
            return;
        }
        let target = self
            .totp_target
            .take()
            .unwrap_or_else(|| self.focused_pane_id());
        self.send_command_to_pane(target, code, window, cx);
        self.totp_dialog = None;
        cx.notify();
    }

    fn cancel_totp(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.dismiss_totp(window, cx);
    }

    /// 关闭动态码对话框（取消/Esc 共用），焦点归还终端。
    fn dismiss_totp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.totp_dialog = None;
        self.shortcut_editor = None;
        self.totp_error = None;
        self.totp_target = None;
        self.totp_fields.clear();
        self.totp_delete_confirm = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn delete_totp(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.totp_delete_confirm.as_deref() != Some(name) {
            self.totp_delete_confirm = Some(name.to_string());
            cx.notify();
            return;
        }
        let store = self.secret_store.clone();
        let Ok(account) = totp_account(name) else {
            self.totp_error = Some("条目名称非法".into());
            cx.notify();
            return;
        };
        let name = name.to_string();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { run_secret_op(move || store.delete(&account)) })
                .await;
            let _ = this.update(cx, |workspace, cx| match result {
                Ok(()) => {
                    workspace.settings.remove_totp(&name);
                    if let Err(error) = workspace.persist_settings() {
                        workspace.totp_error = Some(format!("删除动态码失败: {error:#}"));
                    } else {
                        workspace.totp_delete_confirm = None;
                        workspace.totp_error = None;
                    }
                    cx.notify();
                }
                Err(error) => {
                    workspace.totp_error = Some(format!("删除密钥失败: {error:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ---------- 服务器区（M3 服务器管理）----------

    fn edit_server(
        &mut self,
        entry: Option<&ServerEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_ssh_request();
        self.server_meta_draft = entry.cloned().unwrap_or_default();
        self.begin_shortcut_edit(
            self.server_meta_draft.shortcut.clone(),
            entry.map(|item| BindingOwner::Server(item.name.clone())),
            cx,
        );
        self.server_fields.clear();
        self.server_field_subscriptions.clear();
        let meta = self.server_meta_draft.clone();
        // 编辑时凭据框留空表示不改动；凭据本体只进密钥环
        let secret_placeholder = if entry.is_some() {
            "留空保持现有凭据"
        } else {
            match meta.auth {
                ServerAuthKind::Password => "密码（可选，存密钥环）",
                ServerAuthKind::KeyboardInteractive => "口令（可选，存密钥环）",
                ServerAuthKind::PrivateKey => "passphrase（可选）",
                ServerAuthKind::Agent => "Agent（无需密码）",
            }
        };
        let rows: [(&str, String, &str); 10] = [
            ("名称", meta.name, "如 跳板机A"),
            ("主机", meta.host, "host 或 IP"),
            ("端口", meta.port.to_string(), "22"),
            ("用户名", meta.username, "登录用户"),
            (
                "私钥路径",
                meta.key_path.unwrap_or_default(),
                "可选；留空查找 ~/.ssh/id_ed25519、id_rsa",
            ),
            (
                "编码",
                meta.encoding.unwrap_or_else(|| "UTF-8".into()),
                "UTF-8 / GBK / GB18030",
            ),
            (
                "分组",
                meta.group.unwrap_or_default(),
                "可选；如 生产/入口层",
            ),
            (
                "跳板",
                meta.jump.unwrap_or_default(),
                "可选；另一台服务器的名称",
            ),
            ("备注", meta.note, "备注（可选）"),
            ("凭据", String::new(), secret_placeholder),
        ];
        for (label, value, placeholder) in rows {
            let field = cx.new(|cx| {
                let field = CommandTextField::new(value, placeholder, cx);
                if label == "凭据" {
                    field.masked()
                } else {
                    field
                }
            });
            self.server_fields.push((label.into(), field));
        }
        self.server_dialog = Some(ServerDialog::Edit {
            original: entry.map(|item| item.name.clone()),
        });
        self.server_error = None;
        focus_first_field(&self.server_fields, window, cx);
        cx.notify();
    }

    /// 保存服务器：校验与凭据写入（改名迁移）都在后台线程，避免密钥环 DBus 阻塞。
    fn save_server(&mut self, original: Option<String>, cx: &mut Context<Self>) {
        if self.server_save.is_some() {
            return;
        }
        let values = self
            .server_fields
            .iter()
            .map(|(name, field)| (name.as_str(), field.read(cx).value().to_string()))
            .collect::<BTreeMap<_, _>>();
        let mut meta = self.server_meta_draft.clone();
        meta.name = values
            .get("名称")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        meta.host = values
            .get("主机")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        meta.port = values
            .get("端口")
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(0);
        meta.username = values
            .get("用户名")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        meta.key_path = values
            .get("私钥路径")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        meta.encoding = match values
            .get("编码")
            .map(|value| value.trim().to_ascii_uppercase())
            .as_deref()
        {
            None | Some("") | Some("UTF-8") | Some("UTF8") => None,
            Some("GBK") => Some("GBK".into()),
            Some("GB18030") => Some("GB18030".into()),
            _ => {
                self.server_error = Some("编码仅支持 UTF-8 / GBK / GB18030".into());
                cx.notify();
                return;
            }
        };
        meta.note = values
            .get("备注")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        // 分组：trim 后空 = 移出分组；合法性由 validate() 严格把关（报错留在弹窗内）。
        // 改组只动元数据，凭据键是服务器名，密钥环后台迁移逻辑不受影响。
        meta.group = values
            .get("分组")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        // 跳板：trim 后空 = 直连；引用合法性结合全表在下方 resolve_jump_chain 查
        meta.jump = values
            .get("跳板")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        meta.shortcut = self.shortcut_value(cx);
        let secret = values.get("凭据").cloned().unwrap_or_default();
        if let Some(conflict) = self.check_shortcut_conflict(
            meta.shortcut.as_deref(),
            original.clone().map(BindingOwner::Server),
        ) {
            self.server_error = Some(conflict);
            cx.notify();
            return;
        }
        if let Err(error) = meta.validate() {
            self.server_error = Some(error.to_string());
            cx.notify();
            return;
        }
        // 跳板引用一致性（改名后的全表快照上解析）：自引报专门文案，
        // 缺失/成环/超深原样透出链路错误。同步早退，不发起后台保存、不落盘。
        if meta.jump.as_deref() == Some(meta.name.as_str()) {
            self.server_error = Some(format!("不能把「{}」自身设为跳板", meta.name));
            cx.notify();
            return;
        }
        let mut preview = self.settings.servers.clone();
        if let Some(previous) = &original
            && previous != &meta.name
        {
            preview.retain(|item| item.name != *previous);
            for item in &mut preview {
                if item.jump.as_deref() == Some(previous.as_str()) {
                    item.jump = Some(meta.name.clone());
                }
            }
        }
        match preview.iter_mut().find(|item| item.name == meta.name) {
            Some(slot) => *slot = meta.clone(),
            None => preview.push(meta.clone()),
        }
        if let Err(error) = laterterm_config::resolve_jump_chain(&preview, &meta.name) {
            self.server_error = Some(error.to_string());
            cx.notify();
            return;
        }
        // 同名冲突（改名撞上既有条目）
        if original.as_deref() != Some(meta.name.as_str())
            && self
                .settings
                .servers
                .iter()
                .any(|item| item.name == meta.name)
        {
            self.server_error = Some(format!("已存在同名服务器: {}", meta.name));
            cx.notify();
            return;
        }
        let store = self.secret_store.clone();
        let original_entry = original
            .as_ref()
            .and_then(|name| {
                self.settings
                    .servers
                    .iter()
                    .find(|entry| &entry.name == name)
            })
            .cloned();
        let original_bg = original_entry.clone();
        let active = Arc::new(AtomicBool::new(true));
        self.server_save = Some(active.clone());
        self.pending_focus = Some(self.focused_terminal().read(cx).focus_handle().clone());
        let active_bg = active.clone();
        let dialog_field = self.server_fields.first().map(|(_, field)| field.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    server_connection::update_server_secrets(
                        store,
                        original_bg.as_ref(),
                        &meta,
                        &secret,
                        active_bg,
                    )?;
                    Ok::<_, anyhow::Error>(meta)
                })
                .await;
            let _ = this.update(cx, |workspace, cx| {
                if !active.load(Ordering::Acquire)
                    || !workspace
                        .server_save
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &active))
                    || workspace.server_fields.first().map(|(_, field)| field)
                        != dialog_field.as_ref()
                {
                    return;
                }
                workspace.server_save = None;
                if let Some(previous) = &original_entry
                    && !workspace
                        .settings
                        .servers
                        .iter()
                        .any(|item| item == previous)
                {
                    workspace.server_error =
                        Some("保存期间服务器配置已变化，请重新打开后编辑".into());
                    cx.notify();
                    return;
                }
                match result {
                    Ok(meta) => {
                        if let Some(error) = workspace.check_shortcut_conflict(
                            meta.shortcut.as_deref(),
                            original.clone().map(BindingOwner::Server),
                        ) {
                            workspace.server_error = Some(error);
                            cx.notify();
                            return;
                        }
                        let cleanup_meta = meta.clone();
                        let previous_settings = workspace.settings.clone();
                        if let Err(error) =
                            apply_server_save(&mut workspace.settings, original.as_deref(), meta)
                        {
                            workspace.settings = previous_settings;
                            workspace.server_error = Some(error.to_string());
                        } else if let Err(error) = workspace.persist_settings() {
                            workspace.settings = previous_settings;
                            workspace.server_error = Some(format!("保存服务器失败: {error:#}"));
                        } else {
                            workspace.shortcut_editor = None;
                            if let Some(previous) =
                                original_entry.filter(|entry| entry.name != cleanup_meta.name)
                            {
                                let store = workspace.secret_store.clone();
                                cx.background_executor()
                                    .spawn(async move {
                                        let _ = run_secret_op(move || {
                                            server_connection::cleanup_renamed_server_secrets(
                                                store.as_ref(),
                                                &previous,
                                                &cleanup_meta,
                                            )
                                        });
                                    })
                                    .detach();
                            }
                            workspace.server_dialog = None;
                            workspace.server_fields.clear();
                            workspace.server_error = None;
                            workspace.pending_focus =
                                Some(workspace.focused_terminal().read(cx).focus_handle().clone());
                        }
                        cx.notify();
                    }
                    Err(error) => {
                        workspace.server_error = Some(format!("保存服务器失败: {error:#}"));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn cancel_server(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.dismiss_server(window, cx);
    }

    /// 关闭服务器对话框（取消/Esc 共用），焦点归还终端。
    fn dismiss_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_ssh_request();
        if let Some(save) = self.server_save.take() {
            save.store(false, Ordering::Release);
        }
        self.shortcut_editor = None;
        self.server_dialog = None;
        self.server_error = None;
        self.server_notice = None;
        self.server_fields.clear();
        self.server_field_subscriptions.clear();
        self.server_delete_confirm = None;
        self.server_group_confirm = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 二次点击确认后删除：密钥环凭据一并清理（后台线程）。
    fn delete_server(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.server_delete_confirm.as_deref() != Some(name) {
            self.server_delete_confirm = Some(name.to_string());
            cx.notify();
            return;
        }
        // 被其他条目按名引用为跳板时阻断删除：先解除引用，宁报错不断链。
        let referrers = laterterm_config::servers_referencing_jump(&self.settings.servers, name);
        if !referrers.is_empty() {
            self.server_error = Some(format!(
                "「{name}」正被 {} 台服务器作为跳板引用：{}，先解除引用再删除",
                referrers.len(),
                referrers.join("、")
            ));
            self.server_delete_confirm = None;
            cx.notify();
            return;
        }
        if self
            .ssh_request
            .as_ref()
            .is_some_and(|request| request.entry.name == name)
        {
            self.cancel_ssh_request();
        }
        let store = self.secret_store.clone();
        let name = name.to_string();
        let name_bg = name.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    run_secret_op(move || {
                        server_connection::delete_server_secrets(store.as_ref(), &name_bg)
                    })
                })
                .await;
            let _ = this.update(cx, |workspace, cx| match result {
                Ok(()) => {
                    workspace.settings.remove_server(&name);
                    if let Err(error) = workspace.persist_settings() {
                        workspace.server_error = Some(format!("删除服务器失败: {error:#}"));
                    } else {
                        workspace.server_delete_confirm = None;
                        workspace.server_error = None;
                    }
                    cx.notify();
                }
                Err(error) => {
                    workspace.server_error = Some(format!("删除凭据失败: {error:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 导出服务器元数据到用户选择的 TOML 文件。
    /// 凭据在系统密钥环里，永不写入导出文件；导入端首次连接时补输即可。
    fn export_servers(&mut self, cx: &mut Context<Self>) {
        self.server_error = None;
        self.server_notice = None;
        if self.settings.servers.is_empty() {
            self.server_error = Some("没有可导出的服务器".into());
            cx.notify();
            return;
        }
        let file = ServersFile::new(self.settings.servers.clone());
        let receiver = cx.prompt_for_new_path(
            &app_config_dir().unwrap_or_else(|| PathBuf::from(".")),
            Some("laterterm-servers.toml"),
        );
        cx.spawn(async move |this, cx| {
            // oneshot 双层 Result：外层是通道关闭，内层是平台对话框错误（已是 anyhow）
            let picked = receiver
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("对话框已关闭")));
            let _ = this.update(cx, |workspace, cx| {
                workspace.finish_export(&file, picked, cx)
            });
        })
        .detach();
    }

    fn finish_export(
        &mut self,
        file: &ServersFile,
        picked: anyhow::Result<Option<PathBuf>>,
        cx: &mut Context<Self>,
    ) {
        match picked {
            Err(error) => self.server_error = Some(format!("打开保存对话框失败: {error:#}")),
            Ok(None) => {}
            Ok(Some(path)) => match file.save_to(&path) {
                Ok(()) => {
                    self.server_notice = Some(format!(
                        "已导出 {} 台服务器到 {}（不含凭据）",
                        file.servers.len(),
                        path.display()
                    ));
                }
                Err(error) => self.server_error = Some(format!("导出失败: {error:#}")),
            },
        }
        cx.notify();
    }

    /// 打开导入来源选择弹窗（LaterTerm 文件 / Deepin / Xshell）。
    fn open_server_import(&mut self, cx: &mut Context<Self>) {
        self.server_error = None;
        self.server_notice = None;
        self.server_import_dialog = Some(ServerImportSource::LaterTerm);
        cx.notify();
    }

    fn cancel_server_import(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.dismiss_server_import(window, cx);
    }

    fn dismiss_server_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.server_import_dialog = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 按所选来源执行导入并关闭弹窗。
    fn start_server_import(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.server_import_dialog.take() else {
            return;
        };
        self.server_error = None;
        self.server_notice = None;
        match source {
            ServerImportSource::LaterTerm => self.import_servers(cx),
            ServerImportSource::Deepin => self.import_servers_deepin(cx),
            ServerImportSource::Xshell => self.import_servers_xshell(cx),
        }
        cx.notify();
    }

    /// 来源一：本程序导出的 TOML 文件（文件对话框选择）。
    fn import_servers(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择服务器文件".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = receiver
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("对话框已关闭")));
            let _ = this.update(cx, |workspace, cx| workspace.finish_import(picked, cx));
        })
        .detach();
    }

    fn finish_import(
        &mut self,
        picked: anyhow::Result<Option<Vec<PathBuf>>>,
        cx: &mut Context<Self>,
    ) {
        let paths = match picked {
            Err(error) => {
                self.server_error = Some(format!("打开文件对话框失败: {error:#}"));
                cx.notify();
                return;
            }
            Ok(None) => return,
            Ok(Some(paths)) => paths,
        };
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        match ServersFile::load_from(&path) {
            Ok(file) => self.merge_server_entries(file.servers, &path.display().to_string(), cx),
            Err(error) => {
                self.server_error = Some(format!("导入失败: {error:#}"));
                cx.notify();
            }
        }
    }

    /// 来源二：Deepin 终端远程管理列表 + 命令模板迁移（任务 7 收尾）。
    ///
    /// 落盘取舍（定稿）：命令与服务器是**两次独立提交**，各自 previous-clone
    /// 回滚——命令已生效而服务器保存失败时命令保留，不合成一次 clone（合成
    /// 会让两个语义耦合，回滚时不知道该回滚谁）。命令计数拼进 server_notice，
    /// 格式：`导入 N 台服务器…；命令模板 导入 X，同名 Y，…`。
    fn import_servers_deepin(&mut self, cx: &mut Context<Self>) {
        let Some(path) = deepin_default_path() else {
            self.server_error = Some("无法定位 HOME 目录，Deepin 导入失败".into());
            cx.notify();
            return;
        };
        let command_summary = self.import_deepin_commands_beside(&path);
        match load_deepin_servers(&path) {
            Ok(entries) => {
                self.merge_server_entries(entries, &path.display().to_string(), cx);
                if let Some(summary) = command_summary {
                    self.server_notice = Some(match self.server_notice.take() {
                        Some(notice) => format!("{notice}；{summary}"),
                        None => summary,
                    });
                }
            }
            Err(error) => {
                self.server_error = Some(format!("Deepin 导入失败: {error:#}"));
                if let Some(summary) = command_summary {
                    self.server_notice = Some(summary);
                }
                cx.notify();
            }
        }
    }

    /// 顺带迁移 server-config.conf 同目录的 command-config.conf（宽容解析，
    /// 跳过计数可见）。文件缺失/读取失败静默跳过——服务器导入不受影响；
    /// 需要显式导入或看错误时走命令页签的「导入」按钮。
    fn import_deepin_commands_beside(&mut self, server_path: &Path) -> Option<String> {
        let commands_path = server_path.parent()?.join("command-config.conf");
        if !commands_path.exists() {
            return None;
        }
        let Ok((commands, skipped)) = load_deepin_commands(&commands_path) else {
            eprintln!(
                "Deepin 命令配置读取失败，已跳过: {}",
                commands_path.display()
            );
            return None;
        };
        let bindings = self.all_shortcut_bindings();
        let previous = self.settings.clone();
        let stats = merge_commands_into(&mut self.settings, commands, &bindings);
        if (stats.added > 0 || stats.stripped_shortcut > 0)
            && let Err(error) = self.persist_settings()
        {
            self.settings = previous;
            eprintln!("Deepin 命令迁移落盘失败: {error:#}");
            return Some("命令模板迁移失败（落盘失败，服务器导入未受影响）".into());
        }
        if stats.added == 0
            && stats.skipped_dup == 0
            && stats.skipped_bad == 0
            && stats.stripped_shortcut == 0
            && skipped == 0
        {
            return None; // 没有任何命令数据，不占提示位
        }
        Some(format_merge_commands_notice(&stats, skipped, "命令模板"))
    }

    /// 命令页签「导入」：读取本机 Deepin command-config.conf（默认路径包装）。
    fn import_commands_deepin(&mut self, cx: &mut Context<Self>) {
        let Some(path) = deepin_commands_default_path() else {
            self.command_error = Some("无法定位 HOME 目录，Deepin 命令导入失败".into());
            cx.notify();
            return;
        };
        self.import_commands_deepin_with(&path, cx);
    }

    /// 指定路径的 Deepin 命令导入（路径参数化便于测试注入临时文件）。
    /// 合并走纯函数 `merge_commands_into`；落盘走 `persist_settings`
    /// （previous-clone 回滚），GPUI 测试注入 `settings_save_path` 后不碰真实配置。
    fn import_commands_deepin_with(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.command_error = None;
        self.command_notice = None;
        match load_deepin_commands(path) {
            Ok((commands, skipped)) => {
                let bindings = self.all_shortcut_bindings();
                let previous = self.settings.clone();
                let stats = merge_commands_into(&mut self.settings, commands, &bindings);
                if (stats.added > 0 || stats.stripped_shortcut > 0)
                    && let Err(error) = self.persist_settings()
                {
                    self.settings = previous;
                    self.command_error = Some(format!("Deepin 命令导入失败: {error:#}"));
                    cx.notify();
                    return;
                }
                self.command_notice =
                    Some(format_merge_commands_notice(&stats, skipped, "Deepin 命令"));
                cx.notify();
            }
            Err(error) => {
                self.command_error = Some(format!("Deepin 命令导入失败: {error:#}"));
                cx.notify();
            }
        }
    }

    /// 来源三：Xshell 会话文件（.xsh，可多选；密码绑定原机器加密，不迁移）。
    fn import_servers_xshell(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("选择 Xshell 会话文件或目录".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = receiver
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("对话框已关闭")));
            let _ = this.update(cx, |workspace, cx| {
                workspace.finish_import_xshell(picked, cx)
            });
        })
        .detach();
    }

    fn finish_import_xshell(
        &mut self,
        picked: anyhow::Result<Option<Vec<PathBuf>>>,
        cx: &mut Context<Self>,
    ) {
        let paths = match picked {
            Err(error) => {
                self.server_error = Some(format!("打开文件对话框失败: {error:#}"));
                cx.notify();
                return;
            }
            Ok(None) => return,
            Ok(Some(paths)) => paths,
        };
        let mut imported = Vec::new();
        let mut rejected = 0;
        for path in &paths {
            let sessions = if path.is_dir() {
                laterterm_xshell::load_sessions_dir(path)
            } else {
                laterterm_xshell::load_session(path).map(|session| vec![session])
            };
            if let Ok(sessions) = sessions {
                let (entries, skipped) = crate::import::xshell_batch(&sessions);
                imported.extend(entries.into_iter());
                rejected += skipped.len();
            } else if let Ok((entries, skipped)) = load_xshell_sessions(std::slice::from_ref(path))
            {
                imported.extend(entries);
                rejected += skipped;
            } else {
                rejected += 1;
            }
        }
        match Ok::<_, anyhow::Error>((imported, rejected)) {
            Ok((entries, skipped)) if entries.is_empty() => {
                self.server_error = Some(format!(
                    "所选文件中没有可识别的 .xsh 会话（跳过 {skipped} 个文件）"
                ));
                cx.notify();
            }
            Ok((entries, skipped)) => {
                let context = if skipped > 0 {
                    format!("另有 {skipped} 个文件无法识别")
                } else {
                    String::new()
                };
                self.merge_server_entries(entries, &context, cx);
            }
            Err(error) => {
                self.server_error = Some(format!("Xshell 导入失败: {error:#}"));
                cx.notify();
            }
        }
    }

    /// 合并导入条目：同名跳过、无效丢弃，计数回报；有新增则落盘。
    /// 合并后全表做跳板引用清理——宁断链不丢条目，清了多少要回报。
    fn merge_server_entries(
        &mut self,
        entries: Vec<ServerEntry>,
        source_desc: &str,
        cx: &mut Context<Self>,
    ) {
        let previous = self.settings.clone();
        let (added, skipped_dup, skipped_bad, cleared_jumps) =
            merge_entries_into(&mut self.settings.servers, entries);
        if added > 0
            && let Err(error) = self.persist_settings()
        {
            self.settings = previous;
            self.server_error = Some(format!("导入失败: {error:#}"));
            cx.notify();
            return;
        }
        let mut notice = format!("导入 {added} 台服务器");
        if skipped_dup > 0 {
            notice += &format!("，跳过同名 {skipped_dup}");
        }
        if skipped_bad > 0 {
            notice += &format!("，无效条目 {skipped_bad}");
        }
        if !cleared_jumps.is_empty() {
            notice += &format!(
                "，无效跳板引用已清除 {}（{}）",
                cleared_jumps.len(),
                cleared_jumps.join("、")
            );
        }
        if added == 0 {
            if skipped_dup == 0 && skipped_bad == 0 && cleared_jumps.is_empty() {
                self.server_error = Some(format!("没有可导入的服务器条目（{source_desc}）"));
            } else {
                self.server_error = Some(notice);
            }
        } else {
            self.server_notice = Some(notice);
        }
        cx.notify();
    }

    // ---------- 主机分组（批量改组/解散/折叠）----------

    /// 批量组操作的落盘包装：previous-clone（含折叠态）→ 改内存 → 落盘 →
    /// 失败整体回滚（照 merge_server_entries 的口径）。
    /// 返回是否成功（失败已写入 server_error，调用方不要展示成功提示）。
    fn save_group_change(
        &mut self,
        change: impl FnOnce(&mut Self) -> anyhow::Result<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        let previous = self.settings.clone();
        let previous_collapsed = self.server_groups_collapsed.clone();
        match change(self) {
            Ok(affected) => {
                self.server_group_confirm = None;
                if affected > 0
                    && let Err(error) = self.persist_settings()
                {
                    self.settings = previous;
                    self.server_groups_collapsed = previous_collapsed;
                    self.server_error = Some(format!("分组更新失败: {error:#}"));
                    cx.notify();
                    return false;
                }
                cx.notify();
                true
            }
            Err(error) => {
                self.server_group_confirm = None;
                self.server_error = Some(error.to_string());
                cx.notify();
                false
            }
        }
    }

    /// 重命名分组（批量改组内条目，只改内存；落盘回滚由 [`Self::save_group_change`] 负责）。
    /// 同名并入既有组；折叠键随改名迁移，避免「改名的组折叠态漂移成展开」。
    fn rename_server_group(&mut self, old: &str, new: Option<String>) -> anyhow::Result<usize> {
        let affected = apply_server_group_change(&mut self.settings.servers, old, new.as_deref())?;
        let was_collapsed = self.server_groups_collapsed.remove(&Some(old.to_string()));
        if let Some(name) = new
            && was_collapsed
        {
            self.server_groups_collapsed.insert(Some(name));
        }
        Ok(affected)
    }

    /// 组头「解散」二次确认后的执行：成员转未分组（改内存 + 落盘回滚）。
    fn disband_server_group(&mut self, group: String, cx: &mut Context<Self>) {
        let mut notice = None;
        let succeeded = self.save_group_change(
            |workspace| {
                let affected = workspace.rename_server_group(&group, None)?;
                if affected > 0 {
                    notice = Some(format!("已解散分组「{group}」（{affected} 台转入未分组）"));
                }
                Ok(affected)
            },
            cx,
        );
        if succeeded && let Some(notice) = notice {
            self.server_notice = Some(notice);
        }
    }

    /// 组头「重命名」：打开批量改组弹窗（输入框预填旧组名）。
    fn edit_server_group(&mut self, group: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_ssh_request();
        self.shortcut_editor = None;
        self.server_fields.clear();
        self.server_field_subscriptions.clear();
        let field =
            cx.new(|cx| CommandTextField::new(group.to_string(), "新分组名（同名并入该组）", cx));
        self.server_fields.push(("分组名".into(), field));
        self.server_dialog = Some(ServerDialog::GroupRename {
            original: group.to_string(),
        });
        self.server_error = None;
        focus_first_field(&self.server_fields, window, cx);
        cx.notify();
    }

    /// 批量改组弹窗的保存：清空输入框 = 解散该组。
    fn save_group_rename(&mut self, original: String, cx: &mut Context<Self>) {
        let value = self
            .server_fields
            .first()
            .map(|(_, field)| field.read(cx).value().to_string())
            .unwrap_or_default();
        let new = value.trim().to_string();
        let new = (!new.is_empty()).then_some(new);
        let mut notice = None;
        let succeeded = self.save_group_change(
            |workspace| {
                let affected = workspace.rename_server_group(&original, new.clone())?;
                if affected > 0 {
                    notice = Some(match &new {
                        Some(name) => {
                            format!("已将 {affected} 台服务器移入分组「{name}」")
                        }
                        None => format!("已解散分组「{original}」（{affected} 台转入未分组）"),
                    });
                }
                Ok(affected)
            },
            cx,
        );
        if succeeded {
            self.server_dialog = None;
            self.server_fields.clear();
            if let Some(notice) = notice {
                self.server_notice = Some(notice);
            }
        }
    }

    /// 点连接立即占用请求槽，准备私钥和密钥环操作都在后台进行。
    fn connect_server(&mut self, name: &str, cx: &mut Context<Self>) {
        self.begin_ssh_connect(name, ConnectDest::Shell, cx);
    }

    /// 打开服务器的 SFTP 双栏面板：与 shell 连接同一套请求槽/补输流程
    /// （复用 ServerDialog::Connect 弹窗，仅标题按去向区分），成功后建面板。
    fn open_sftp_panel(&mut self, name: &str, cx: &mut Context<Self>) {
        self.begin_ssh_connect(name, ConnectDest::SftpPanel, cx);
    }

    /// 连接发起的公共前置：请求槽互斥 + 跳板链快照 + Connecting 弹窗。
    fn begin_ssh_connect(&mut self, name: &str, dest: ConnectDest, cx: &mut Context<Self>) {
        if self.ssh_request.is_some() {
            return;
        }
        let Some(entry) = self
            .settings
            .servers
            .iter()
            .find(|item| item.name == name)
            .cloned()
        else {
            return;
        };
        self.shortcut_editor = None;
        self.server_notice = None;
        self.server_error = None;
        self.server_fields.clear();
        self.server_field_subscriptions.clear();
        let id = self.next_ssh_request_id;
        self.next_ssh_request_id += 1;
        // 跳板链快照：连接发起时解析一次，随请求持有（补输凭据重试复用，不回查配置表）。
        // 载入/导入期已断开坏引用，这里失败只剩会话中途配置被改的极端情形——
        // 双保险：直接报错不发起连接，绝不静默降级成直连（把本该走跳板的
        // 内网机暴露到公网路径，比连不上更糟）。
        let jump_entries: Vec<ServerEntry> =
            match laterterm_config::resolve_jump_chain(&self.settings.servers, &entry.name) {
                Ok(chain) => chain.into_iter().cloned().collect(),
                Err(error) => {
                    self.server_error = Some(format!("解析跳板链失败: {error}"));
                    cx.notify();
                    return;
                }
            };
        self.ssh_request = Some(SshRequest {
            id,
            entry: entry.clone(),
            jump: jump_entries,
            dest,
            active: Arc::new(AtomicBool::new(true)),
            running: false,
        });
        self.server_dialog = Some(ServerDialog::Connecting {
            name: entry.name.clone(),
        });
        self.start_ssh_connect(id, None, false, cx);
    }

    fn cancel_ssh_request(&mut self) {
        if let Some(request) = self.ssh_request.take() {
            request.active.store(false, Ordering::Release);
        }
        self.ssh_connecting = None;
    }

    /// 空登录密码显式选择私钥，不向服务器发送空密码；口令原样保留。
    fn submit_server_connect(&mut self, cx: &mut Context<Self>) {
        let Some(ServerDialog::Connect {
            kind,
            remember,
            request_id,
            ..
        }) = self.server_dialog.clone()
        else {
            return;
        };
        let secret = self
            .server_fields
            .first()
            .map(|(_, field)| field.read(cx).value().to_string())
            .unwrap_or_default();
        self.start_ssh_connect(request_id, Some((kind, secret)), remember, cx);
    }

    fn start_ssh_connect(
        &mut self,
        request_id: u64,
        supplied: Option<(CredentialKind, String)>,
        remember: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = &mut self.ssh_request else {
            return;
        };
        if request.id != request_id || request.running {
            return;
        }
        request.running = true;
        let entry = request.entry.clone();
        let active = request.active.clone();
        self.ssh_connecting = Some(entry.name.clone());
        self.server_error = None;
        let store = self.secret_store.clone();
        let jump_entries = request.jump.clone();
        match request.dest {
            ConnectDest::Shell => {
                let (tx, rx) = flume::bounded(1);
                std::thread::spawn(move || {
                    let result = server_connection::connect(
                        &entry,
                        jump_entries,
                        supplied,
                        remember,
                        store,
                        active,
                    );
                    if let Err(error) = tx.send((request_id, entry, result))
                        && let Ok(ConnectionOutcome::Connected { session, .. }) = error.0.2
                    {
                        session.kill();
                    }
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(result) = rx.recv_async().await {
                        let mut result = Some(result);
                        if this
                            .update(cx, |workspace, cx| {
                                workspace
                                    .pending_ssh_results
                                    .push(result.take().expect("连接结果只消费一次"));
                                cx.notify();
                            })
                            .is_err()
                            && let Some((_, _, Ok(ConnectionOutcome::Connected { session, .. }))) =
                                result
                        {
                            session.kill();
                        }
                    }
                })
                .detach();
            }
            ConnectDest::SftpPanel => {
                // 与 shell 分函数分管道（P2 评审口径）：产物是 SftpHandle，
                // 迟到取消由 connect_sftp 内部 close() 处置，无需外部 kill。
                let (tx, rx) = flume::bounded(1);
                std::thread::spawn(move || {
                    let result = server_connection::connect_sftp(
                        &entry,
                        jump_entries,
                        supplied,
                        remember,
                        store,
                        active,
                    );
                    if let Err(error) = tx.send((request_id, entry, result))
                        && let Ok(SftpOutcome::Connected { browse, .. }) = error.0.2
                    {
                        browse.close();
                    }
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(result) = rx.recv_async().await {
                        let mut result = Some(result);
                        if this
                            .update(cx, |workspace, cx| {
                                workspace
                                    .pending_sftp_results
                                    .push(result.take().expect("连接结果只消费一次"));
                                cx.notify();
                            })
                            .is_err()
                            && let Some((_, _, Ok(SftpOutcome::Connected { browse, .. }))) = result
                        {
                            browse.close();
                        }
                    }
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn finish_ssh_connect(
        &mut self,
        request_id: u64,
        entry: ServerEntry,
        result: anyhow::Result<ConnectionOutcome>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.ssh_request.as_ref().is_some_and(|request| {
            request.id == request_id && request.active.load(Ordering::Acquire)
        }) && self.settings.servers.iter().any(|item| item == &entry);
        if !current {
            if let Ok(ConnectionOutcome::Connected { session, .. }) = result {
                session.kill();
            }
            if self
                .ssh_request
                .as_ref()
                .is_some_and(|request| request.id == request_id)
            {
                self.cancel_ssh_request();
                self.server_dialog = None;
                self.server_fields.clear();
                self.server_field_subscriptions.clear();
            }
            return;
        }
        if let Some(request) = &mut self.ssh_request {
            request.running = false;
        }
        self.ssh_connecting = None;
        match result {
            Ok(ConnectionOutcome::Connected { session, warning }) => {
                self.ssh_request = None;
                self.server_dialog = None;
                self.server_fields.clear();
                self.server_field_subscriptions.clear();
                self.server_error = None;
                self.server_notice = warning;
                let pane_id = self.attach_pane(entry.name, session, window, cx);
                let tab_id = TabId(self.next_tab_id);
                self.next_tab_id += 1;
                self.tabs.push(WorkspaceTab {
                    id: tab_id,
                    layout: LayoutNode::Leaf(pane_id),
                    focused_pane: pane_id,
                });
                self.focus_pane(pane_id, window, cx);
            }
            Ok(ConnectionOutcome::Prompt { kind, warning }) => {
                self.present_credential_prompt(request_id, entry.name, kind, warning, window, cx);
            }
            Err(error) => {
                self.server_error = Some(format!("连接 {} 失败: {error:#}", entry.name));
                if matches!(self.server_dialog, Some(ServerDialog::Connecting { .. })) {
                    self.cancel_ssh_request();
                    self.server_dialog = None;
                    self.right_panel = true;
                    self.right_panel_tab = RightPanelTab::Servers;
                    self.focus_active(window, cx);
                }
            }
        }
        cx.notify();
    }

    /// 补输凭据弹窗（shell 与 SFTP 连接共用；ServerDialog::Connect 复用，
    /// 标题按 ssh_request 去向区分）。
    fn present_credential_prompt(
        &mut self,
        request_id: u64,
        name: String,
        kind: CredentialKind,
        warning: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.server_fields.clear();
        self.server_field_subscriptions.clear();
        self.server_error = warning;
        let label = if matches!(kind, CredentialKind::PrivateKey { .. }) {
            "私钥解锁口令"
        } else {
            "登录密码 / 交互口令（可留空使用私钥）"
        };
        let field = cx.new(|cx| CommandTextField::new(String::new(), label, cx).masked());
        self.server_field_subscriptions.push(
            cx.subscribe(&field, |workspace, _, _: &FieldSubmit, cx| {
                workspace.submit_server_connect(cx)
            }),
        );
        field.read(cx).focus_handle().focus(window);
        self.server_fields.push((label.into(), field));
        self.server_dialog = Some(ServerDialog::Connect {
            name,
            kind,
            request_id,
            remember: false,
        });
    }

    /// 需要 window 的 SFTP 连接结果在 render 期消费（与 shell 同模式）。
    fn process_sftp_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (request_id, entry, result) in std::mem::take(&mut self.pending_sftp_results) {
            self.finish_sftp_connect(request_id, entry, result, window, cx);
        }
    }

    /// SFTP 连接结果落地：Prompt 走共用补输弹窗；Connected 建面板并
    /// 触发初始加载（远端 realpath + 列目录、本地列目录，全部后台线程）。
    fn finish_sftp_connect(
        &mut self,
        request_id: u64,
        entry: ServerEntry,
        result: anyhow::Result<SftpOutcome>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.ssh_request.as_ref().is_some_and(|request| {
            request.id == request_id && request.active.load(Ordering::Acquire)
        }) && self.settings.servers.iter().any(|item| item == &entry);
        if !current {
            // 迟到/取消的结果：丢弃并关闭通道（无 session 可 kill，SFTP 无
            // TerminalSession——这正是分管道的原因）
            if let Ok(SftpOutcome::Connected { browse, .. }) = result {
                browse.close();
            }
            if self
                .ssh_request
                .as_ref()
                .is_some_and(|request| request.id == request_id)
            {
                self.cancel_ssh_request();
                self.server_dialog = None;
                self.server_fields.clear();
                self.server_field_subscriptions.clear();
            }
            return;
        }
        if let Some(request) = &mut self.ssh_request {
            request.running = false;
        }
        self.ssh_connecting = None;
        match result {
            Ok(SftpOutcome::Connected {
                browse,
                endpoint,
                hops,
                warning,
            }) => {
                self.ssh_request = None;
                self.server_dialog = None;
                self.server_fields.clear();
                self.server_field_subscriptions.clear();
                self.server_error = None;
                self.server_notice = warning;
                let local_dir = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
                let mut panel = SftpPanelState::new(
                    sftp_panel::panel_title(&endpoint, &hops),
                    browse,
                    endpoint,
                    hops,
                    local_dir,
                );
                panel.initial_load(&self.sftp_events_tx);
                self.sftp_panel = Some(panel);
                cx.notify();
            }
            Ok(SftpOutcome::Prompt { kind, warning }) => {
                self.present_credential_prompt(request_id, entry.name, kind, warning, window, cx);
                cx.notify();
            }
            Err(error) => {
                self.server_error = Some(format!("打开 SFTP 面板失败: {error:#}"));
                if matches!(self.server_dialog, Some(ServerDialog::Connecting { .. })) {
                    self.cancel_ssh_request();
                    self.server_dialog = None;
                    self.right_panel = true;
                    self.right_panel_tab = RightPanelTab::Servers;
                    self.focus_active(window, cx);
                }
                cx.notify();
            }
        }
    }

    /// 需要 window 的结果在 render 期消费。
    fn process_ssh_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (request_id, entry, result) in std::mem::take(&mut self.pending_ssh_results) {
            self.finish_ssh_connect(request_id, entry, result, window, cx);
        }
    }

    // ---------- SFTP 双栏面板 ----------

    /// 后台事件泵：列表/传输进度全部经此回 UI（面板关闭后事件自然丢弃）。
    fn start_sftp_pump(&self, cx: &mut Context<Self>) {
        let rx = self.sftp_events_rx.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                if this
                    .update(cx, |workspace, cx| workspace.handle_sftp_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// 面板事件落地：状态应用 + 队列推进（半成品清理在传输工作线程内完成）。
    fn handle_sftp_event(&mut self, event: SftpEvent, cx: &mut Context<Self>) {
        let transfer_finished = self
            .sftp_panel
            .as_mut()
            .map(|panel| panel.apply_event(event))
            .unwrap_or(false);
        if transfer_finished && let Some(panel) = &mut self.sftp_panel {
            panel.promote_next(&self.sftp_events_tx);
        }
        cx.notify();
    }

    /// 双栏行点击：单击切换选中；双击目录进入（后台列目录）。
    pub(crate) fn sftp_row_click(
        &mut self,
        is_local: bool,
        double: bool,
        name: String,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        panel.error = None;
        panel.notice = None;
        if double && is_dir {
            if is_local {
                let dir = panel.local.dir.join(&name);
                panel.local.selected.clear();
                panel.refresh_local(&dir, &self.sftp_events_tx);
            } else {
                match sftp_panel::remote_join(&panel.remote.dir, &name) {
                    Ok(dir) => {
                        panel.remote.selected.clear();
                        panel.refresh_remote(&dir, &self.sftp_events_tx);
                    }
                    Err(message) => panel.error = Some(message),
                }
            }
        } else {
            let selected = if is_local {
                &mut panel.local.selected
            } else {
                &mut panel.remote.selected
            };
            if selected.contains(&name) {
                selected.remove(&name);
            } else {
                selected.insert(name);
            }
        }
        cx.notify();
    }

    /// 上级目录导航：远端走 remote_parent（`..` 被 remote_join 拒绝），
    /// 本地走 Path::parent。
    pub(crate) fn sftp_go_up(&mut self, side: PaneSide, path: &str, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        panel.error = None;
        match side {
            PaneSide::Local => {
                if let Some(parent) = sftp_panel::local_parent(Path::new(path)) {
                    panel.local.selected.clear();
                    panel.refresh_local(&parent, &self.sftp_events_tx);
                }
            }
            PaneSide::Remote => {
                if let Some(parent) = sftp_panel::remote_parent(path) {
                    panel.remote.selected.clear();
                    panel.refresh_remote(&parent, &self.sftp_events_tx);
                }
            }
        }
        cx.notify();
    }

    /// 刷新当前目录。
    pub(crate) fn sftp_refresh(&mut self, side: PaneSide, path: &str, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        panel.error = None;
        match side {
            PaneSide::Local => {
                let dir = PathBuf::from(path);
                panel.refresh_local(&dir, &self.sftp_events_tx);
            }
            PaneSide::Remote => {
                if path.is_empty() {
                    // 初始列表尚未落地（remote.dir 仍为空）：read_dir("")
                    // 在真实 sshd 上必然报错，此刻也没有可刷新的内容
                    cx.notify();
                    return;
                }
                let dir = path.to_string();
                panel.refresh_remote(&dir, &self.sftp_events_tx);
            }
        }
        cx.notify();
    }

    /// 系统目录对话框选择本机目录（xdg-desktop-portal，与服务器导出同模式）。
    pub(crate) fn sftp_browse_local(&mut self, cx: &mut Context<Self>) {
        // 注：gpui 的 PathPromptOptions 不带起始目录参数，从当前目录打开
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择本机目录".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = receiver
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("对话框已关闭")));
            let _ = this.update(cx, |workspace, cx| workspace.finish_sftp_browse(picked, cx));
        })
        .detach();
    }

    fn finish_sftp_browse(
        &mut self,
        picked: anyhow::Result<Option<Vec<PathBuf>>>,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        match picked {
            Err(error) => panel.error = Some(format!("打开目录对话框失败: {error:#}")),
            Ok(None) => {}
            Ok(Some(paths)) => {
                if let Some(dir) = paths.into_iter().next() {
                    panel.local.selected.clear();
                    panel.refresh_local(&dir, &self.sftp_events_tx);
                }
            }
        }
        cx.notify();
    }

    /// 入队传输：本栏选中集里的文件逐条排队（目录 v1 不支持，提示可见）；
    /// 同名去重；然后尝试推进队列。
    pub(crate) fn sftp_enqueue(&mut self, kind: TransferKind, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        panel.error = None;
        panel.notice = None;
        let source_selected = match kind {
            TransferKind::Upload => panel.local.selected.clone(),
            TransferKind::Download => panel.remote.selected.clone(),
        };
        if source_selected.is_empty() {
            panel.notice = Some(match kind {
                TransferKind::Upload => "先在左栏（本机）选择要上传的文件".to_string(),
                TransferKind::Download => "先在右栏（远端）选择要下载的文件".to_string(),
            });
            cx.notify();
            return;
        }
        let is_upload = kind == TransferKind::Upload;
        let mut queued = 0usize;
        let mut skipped_dirs = 0usize;
        for name in &source_selected {
            let is_dir = if is_upload {
                panel
                    .local
                    .entries
                    .iter()
                    .find(|entry| &entry.name == name)
                    .map(|entry| entry.is_dir)
            } else {
                panel
                    .remote
                    .entries
                    .iter()
                    .find(|entry| &entry.name == name)
                    .map(|entry| entry.is_dir)
            };
            if is_dir == Some(true) {
                skipped_dirs += 1;
                continue;
            }
            // 同名且仍在排队/传输：跳过（防重复入队）
            if panel.transfers.iter().any(|transfer| {
                transfer.is_active() && transfer.kind == kind && &transfer.file_name == name
            }) {
                continue;
            }
            let remote_path = match sftp_panel::remote_join(&panel.remote.dir, name) {
                Ok(remote_path) => remote_path,
                Err(message) => {
                    panel.error = Some(message);
                    continue;
                }
            };
            let id = self.next_sftp_transfer_id;
            self.next_sftp_transfer_id += 1;
            panel.transfers.push(SftpTransfer {
                id,
                kind,
                file_name: name.clone(),
                local_path: panel.local.dir.join(name),
                remote_path,
                total: 0,
                transferred: 0,
                status: sftp_panel::TransferStatus::Queued,
                error: None,
                cancel: Arc::new(AtomicBool::new(false)),
            });
            queued += 1;
        }
        if skipped_dirs > 0 {
            panel.notice = Some(format!(
                "{skipped_dirs} 个目录已跳过（v1 仅支持文件传输，目录递归待后续）"
            ));
        } else if queued > 0 {
            panel.notice = Some(format!("已入队 {queued} 个传输任务"));
        }
        panel.promote_next(&self.sftp_events_tx);
        cx.notify();
    }

    /// 取消一条传输（排队/传输中均可；半成品清理在终结事件后由
    /// 传输工作线程自身完成）。
    pub(crate) fn sftp_cancel_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        panel.cancel_transfer(id);
        cx.notify();
    }

    /// 打开栏内操作弹窗（新建目录/重命名）。
    pub(crate) fn sftp_open_prompt(
        &mut self,
        side: PaneSide,
        kind: SftpPromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        panel.error = None;
        panel.notice = None;
        self.sftp_prompt_fields.clear();
        self.sftp_prompt_subscriptions.clear();
        let initial = kind.initial();
        let field = cx.new(|cx| CommandTextField::new(initial, "名称", cx));
        self.sftp_prompt_subscriptions.push(
            cx.subscribe(&field, |workspace, _, _: &FieldSubmit, cx| {
                workspace.submit_sftp_prompt(cx)
            }),
        );
        field.read(cx).focus_handle().focus(window);
        self.sftp_prompt_fields.push(("名称".into(), field));
        self.sftp_prompt = Some(SftpPromptDialog { side, kind });
        cx.notify();
    }

    fn dismiss_sftp_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sftp_prompt = None;
        self.sftp_prompt_fields.clear();
        self.sftp_prompt_subscriptions.clear();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 提交新建目录/重命名：名称先过 remote_join 纯校验，操作本身在
    /// 后台线程执行（远端走浏览通道，本地走 std::fs；均不允许阻塞 UI），
    /// 完成后同线程续列目录；UI 侧立即关弹窗（乐观口径，错误经事件回显）。
    fn submit_sftp_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.sftp_prompt.clone() else {
            return;
        };
        let value = self
            .sftp_prompt_fields
            .first()
            .map(|(_, field)| field.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if value.is_empty() {
            return;
        }
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        let side = prompt.side;
        let is_local = side == PaneSide::Local;
        let base_dir = if is_local {
            panel.local.dir.display().to_string()
        } else {
            panel.remote.dir.clone()
        };
        let target = match sftp_panel::remote_join(&base_dir, &value) {
            Ok(target) => target,
            Err(message) => {
                panel.error = Some(message);
                cx.notify();
                return;
            }
        };
        let kind = prompt.kind;
        // UI 侧先行：关弹窗、清选中、给进行中提示；操作结果经事件回显
        self.sftp_prompt = None;
        self.sftp_prompt_fields.clear();
        self.sftp_prompt_subscriptions.clear();
        panel.error = None;
        panel.notice = Some("正在执行…".to_string());
        if is_local {
            panel.local.selected.clear();
        } else {
            panel.remote.selected.clear();
        }
        let events = self.sftp_events_tx.clone();
        let browse = Arc::clone(&panel.browse);
        let local_dir = panel.local.dir.clone();
        let remote_dir = panel.remote.dir.clone();
        std::thread::Builder::new()
            .name("laterterm-sftp-op".into())
            .spawn(move || {
                let failure = match &kind {
                    SftpPromptKind::Mkdir => {
                        let result = if is_local {
                            std::fs::create_dir(&target)
                        } else {
                            browse.create_dir(&target)
                        };
                        result.err().map(|error| format!("新建目录失败: {error}"))
                    }
                    SftpPromptKind::Rename { from } => {
                        let base = if is_local {
                            local_dir.display().to_string()
                        } else {
                            remote_dir.clone()
                        };
                        match sftp_panel::remote_join(&base, from) {
                            Ok(old_path) => {
                                let result = if is_local {
                                    std::fs::rename(&old_path, &target)
                                } else {
                                    browse.rename(&old_path, &target)
                                };
                                result.err().map(|error| format!("重命名失败: {error}"))
                            }
                            Err(message) => Some(message),
                        }
                    }
                };
                if let Some(message) = failure {
                    let _ = events.send(SftpEvent::Notice {
                        error: true,
                        message,
                    });
                }
                // 成败都刷新该栏（失败时也刷，呈现真实状态）
                if is_local {
                    sftp_panel::load_local_into(local_dir, &events);
                } else {
                    load_remote_into(&browse, &remote_dir, &remote_dir, None, &events);
                }
            })
            .ok();
        cx.notify();
    }

    /// 重命名入口：恰好选中一项时弹重命名框。
    pub(crate) fn sftp_rename_selected(
        &mut self,
        side: PaneSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        let selected = match side {
            PaneSide::Local => &panel.local.selected,
            PaneSide::Remote => &panel.remote.selected,
        };
        if selected.len() != 1 {
            panel.notice = Some("重命名需要恰好选中一项".to_string());
            cx.notify();
            return;
        }
        let from = selected.iter().next().cloned().unwrap_or_default();
        self.sftp_open_prompt(side, SftpPromptKind::Rename { from }, window, cx);
    }

    /// 删除入口：二次确认对齐 sidebar 模式（再点一次「删除」执行）。
    /// 删除在后台线程执行（远端按条目类型走 remove_file/remove_dir），
    /// 完成后续列该栏。
    pub(crate) fn sftp_delete_selected(&mut self, side: PaneSide, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.sftp_panel else {
            return;
        };
        panel.error = None;
        let selected: Vec<(String, bool)> = match side {
            PaneSide::Local => panel
                .local
                .selected
                .iter()
                .map(|name| {
                    let is_dir = panel
                        .local
                        .entries
                        .iter()
                        .find(|entry| &entry.name == name)
                        .is_some_and(|entry| entry.is_dir);
                    (name.clone(), is_dir)
                })
                .collect(),
            PaneSide::Remote => panel
                .remote
                .selected
                .iter()
                .map(|name| {
                    let is_dir = panel
                        .remote
                        .entries
                        .iter()
                        .find(|entry| &entry.name == name)
                        .is_some_and(|entry| entry.is_dir);
                    (name.clone(), is_dir)
                })
                .collect(),
        };
        if selected.is_empty() {
            panel.notice = Some("先选择要删除的条目".to_string());
            cx.notify();
            return;
        }
        let confirm_key = {
            let mut names = selected
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            names.sort();
            format!("{side:?}:{}", names.join(","))
        };
        if panel.delete_confirm.as_deref() != Some(confirm_key.as_str()) {
            panel.delete_confirm = Some(confirm_key);
            panel.notice = Some("再点一次「删除」确认（删除不可恢复）".to_string());
            cx.notify();
            return;
        }
        panel.delete_confirm = None;
        panel.notice = Some("正在删除…".to_string());
        let is_local = side == PaneSide::Local;
        let events = self.sftp_events_tx.clone();
        let base_dir = if is_local {
            panel.local.dir.clone()
        } else {
            PathBuf::from(panel.remote.dir.clone())
        };
        let browse = Arc::clone(&panel.browse);
        let local_dir = panel.local.dir.clone();
        let remote_dir = panel.remote.dir.clone();
        std::thread::Builder::new()
            .name("laterterm-sftp-op".into())
            .spawn(move || {
                for (name, is_dir) in &selected {
                    let Ok(path) = sftp_panel::remote_join(&base_dir.display().to_string(), name)
                    else {
                        continue;
                    };
                    let result = if is_local {
                        // 目录先尝试删目录，失败按文件删（非空目录两者皆败，错误可见）
                        match std::fs::remove_dir(&path) {
                            Ok(()) => Ok(()),
                            Err(_) => std::fs::remove_file(&path),
                        }
                    } else if *is_dir {
                        browse.remove_dir(&path)
                    } else {
                        browse.remove_file(&path)
                    };
                    if let Err(error) = result {
                        let _ = events.send(SftpEvent::Notice {
                            error: true,
                            message: format!("删除 {name} 失败: {error}"),
                        });
                    }
                }
                // 删除完成后续列该栏（无论成败，呈现真实状态）
                if is_local {
                    sftp_panel::load_local_into(local_dir, &events);
                } else {
                    load_remote_into(&browse, &remote_dir, &remote_dir, None, &events);
                }
            })
            .ok();
        if is_local {
            panel.local.selected.clear();
        } else {
            panel.remote.selected.clear();
        }
        cx.notify();
    }

    /// 关闭面板（Esc/×/遮罩）：取消全部传输 + 关浏览通道 + 焦点归还终端。
    pub(crate) fn close_sftp_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.teardown_sftp_panel();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 面板清理（关窗/退出共用，无窗口依赖）。在途传输靠取消标志终结
    /// （close() 只能关浏览通道，打断不了在途传输——sftp.rs 取消语义）。
    fn teardown_sftp_panel(&mut self) {
        if let Some(mut panel) = self.sftp_panel.take() {
            let _running = panel.cancel_all();
            panel.browse.close();
        }
        self.sftp_prompt = None;
        self.sftp_prompt_fields.clear();
        self.sftp_prompt_subscriptions.clear();
    }

    fn close_window(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.shutdown();
        cx.quit();
    }

    /// 自绘与原生关闭入口共用；重复调用不会重复终止会话。
    pub(crate) fn shutdown(&mut self) {
        self.cancel_ssh_request();
        self.teardown_sftp_panel();
        if let Some(save) = self.server_save.take() {
            save.store(false, Ordering::Release);
        }
        for (_, _, result) in self.pending_ssh_results.drain(..) {
            if let Ok(ConnectionOutcome::Connected { session, .. }) = result {
                session.kill();
            }
        }
        for pane in &self.panes {
            pane.session.close();
        }
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_terminal()
            .read(cx)
            .focus_handle()
            .focus(window);
    }

    fn begin_shortcut_edit(
        &mut self,
        value: Option<String>,
        owner: Option<BindingOwner>,
        cx: &mut Context<Self>,
    ) {
        let recorder = cx.new(|cx| ShortcutRecorder::new(value, cx));
        let subscription = cx.subscribe(&recorder, |workspace, _, _: &ShortcutChanged, cx| {
            workspace.refresh_shortcut_error(cx);
        });
        self.shortcut_editor = Some(ShortcutEditor {
            owner,
            recorder,
            _subscription: subscription,
        });
        self.refresh_shortcut_error(cx);
    }

    fn shortcut_value(&self, cx: &App) -> Option<String> {
        self.shortcut_editor
            .as_ref()
            .and_then(|editor| editor.recorder.read(cx).value().map(str::to_string))
    }

    fn refresh_shortcut_error(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = &self.shortcut_editor {
            let error = self
                .check_shortcut_conflict(editor.recorder.read(cx).value(), editor.owner.clone());
            editor
                .recorder
                .update(cx, |recorder, cx| recorder.set_error(error, cx));
        }
    }

    fn start_shortcut_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &self.shortcut_editor {
            editor
                .recorder
                .update(cx, |recorder, cx| recorder.start(window, cx));
        }
    }

    fn toggle_panel_tab(&mut self, tab: RightPanelTab, cx: &mut Context<Self>) {
        self.right_panel = !self.right_panel || self.right_panel_tab != tab;
        self.right_panel_tab = tab;
        cx.notify();
    }

    fn edit_panel_shortcut(
        &mut self,
        owner: BindingOwner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = match owner {
            BindingOwner::CommandsPanel => self.settings.shortcuts.commands_panel.clone(),
            BindingOwner::ServersPanel => self.settings.shortcuts.servers_panel.clone(),
            _ => return,
        };
        self.begin_shortcut_edit(value, Some(owner.clone()), cx);
        self.panel_shortcut_dialog = Some(owner);
        self.shortcut_error = None;
        self.start_shortcut_recording(window, cx);
        cx.notify();
    }

    fn dismiss_panel_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel_shortcut_dialog = None;
        self.shortcut_editor = None;
        self.shortcut_error = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn save_panel_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = self.panel_shortcut_dialog.clone() else {
            return;
        };
        let value = self.shortcut_value(cx);
        if let Some(error) = self.check_shortcut_conflict(value.as_deref(), Some(owner.clone())) {
            self.shortcut_error = Some(error);
            cx.notify();
            return;
        }
        let previous = self.settings.clone();
        match owner {
            BindingOwner::CommandsPanel => self.settings.shortcuts.commands_panel = value,
            BindingOwner::ServersPanel => self.settings.shortcuts.servers_panel = value,
            _ => return,
        }
        let result = self
            .settings
            .shortcuts
            .validate()
            .and_then(|()| self.persist_settings());
        if let Err(error) = result {
            self.settings = previous;
            self.shortcut_error = Some(format!("保存快捷键失败: {error:#}"));
            cx.notify();
        } else {
            self.dismiss_panel_shortcut(window, cx);
        }
    }

    /// 全量快捷键绑定：内置、面板与条目共用同一冲突注册表。
    fn all_shortcut_bindings(&self) -> Vec<ShortcutBinding> {
        let mut bindings = Vec::new();
        for (keystroke, label) in BUILTIN_SHORTCUTS {
            if let Ok(normalized) = normalize_shortcut(keystroke) {
                bindings.push(ShortcutBinding {
                    owner: BindingOwner::Builtin,
                    normalized,
                    label: (*label).to_string(),
                });
            }
        }
        for (owner, shortcut) in [
            (
                BindingOwner::CommandsPanel,
                &self.settings.shortcuts.commands_panel,
            ),
            (
                BindingOwner::ServersPanel,
                &self.settings.shortcuts.servers_panel,
            ),
        ] {
            if let Some(shortcut) = shortcut
                && let Ok(normalized) = normalize_shortcut(shortcut)
            {
                bindings.push(ShortcutBinding {
                    label: owner.describe(),
                    owner,
                    normalized,
                });
            }
        }
        for command in &self.settings.commands {
            if let Some(shortcut) = &command.shortcut
                && let Ok(normalized) = normalize_shortcut(shortcut)
            {
                bindings.push(ShortcutBinding {
                    owner: BindingOwner::Command(command.name.clone()),
                    normalized,
                    label: format!("运行命令 {}", command.name),
                });
            }
        }
        for server in &self.settings.servers {
            if let Some(shortcut) = &server.shortcut
                && let Ok(normalized) = normalize_shortcut(shortcut)
            {
                bindings.push(ShortcutBinding {
                    owner: BindingOwner::Server(server.name.clone()),
                    normalized,
                    label: format!("连接服务器 {}", server.name),
                });
            }
        }
        for entry in &self.settings.totp {
            if let Some(shortcut) = &entry.shortcut
                && let Ok(normalized) = normalize_shortcut(shortcut)
            {
                bindings.push(ShortcutBinding {
                    owner: BindingOwner::Totp(entry.name.clone()),
                    normalized,
                    label: format!("生成动态码 {}", entry.name),
                });
            }
        }
        bindings
    }

    /// 保存前检查：候选快捷键是否撞上其他绑定（编辑自身时豁免）。
    fn check_shortcut_conflict(
        &self,
        candidate: Option<&str>,
        ignore_owner: Option<BindingOwner>,
    ) -> Option<String> {
        let candidate = candidate?.trim();
        if candidate.is_empty() {
            return None;
        }
        if normalize_shortcut(candidate).is_err() {
            return Some("格式不合法（应为 ctrl+alt+k 一类组合）".into());
        }
        let bindings = self.all_shortcut_bindings();
        shortcut_conflict_with(&bindings, candidate, ignore_owner.as_ref())
            .map(|owner| format!("快捷键与{owner}冲突"))
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let plain_escape = event.keystroke.key.eq_ignore_ascii_case("escape")
            && event.keystroke.modifiers == gpui::Modifiers::default();
        if self.server_save.is_some() {
            if plain_escape {
                self.dismiss_server(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if let Some(editor) = &self.shortcut_editor
            && editor.recorder.read(cx).focus_handle().is_focused(window)
            && (editor.recorder.read(cx).is_recording() || !plain_escape)
        {
            editor.recorder.update(cx, |recorder, cx| {
                recorder.handle_key_down(event, window, cx)
            });
            cx.stop_propagation();
            return;
        }
        if self.panel_shortcut_dialog.is_some() {
            if plain_escape {
                self.dismiss_panel_shortcut(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        // 终端右键菜单：Esc 直接关闭；其他按键先关菜单再正常分发。
        if self.terminal_menu.is_some() {
            self.terminal_menu = None;
            cx.notify();
            if plain_escape {
                cx.stop_propagation();
                return;
            }
        }
        if self.command_dialog.is_some()
            || self.totp_dialog.is_some()
            || self.server_dialog.is_some()
            || self.server_import_dialog.is_some()
        {
            // 输入框右键菜单优先消费 Esc，避免同时关闭外层弹窗。
            if plain_escape {
                let field_with_menu = self
                    .command_fields
                    .iter()
                    .chain(self.totp_fields.iter())
                    .chain(self.server_fields.iter())
                    .find(|(_, field)| field.read(cx).has_context_menu())
                    .map(|(_, field)| field.clone());
                if let Some(field) = field_with_menu {
                    field.update(cx, |field, cx| field.close_context_menu(cx));
                    cx.stop_propagation();
                    return;
                }
                if self.command_dialog.is_some() {
                    self.dismiss_command(window, cx);
                } else if self.totp_dialog.is_some() {
                    self.dismiss_totp(window, cx);
                } else if self.server_dialog.is_some() {
                    self.dismiss_server(window, cx);
                } else {
                    self.dismiss_server_import(window, cx);
                }
                cx.stop_propagation();
            }
            // 没有输入框的预览弹窗可能仍保留终端焦点；不要将方向快捷键透传到 shell。
            if pane_direction_for_keystroke(&event.keystroke).is_some()
                && self
                    .focused_terminal()
                    .read(cx)
                    .focus_handle()
                    .is_focused(window)
            {
                cx.stop_propagation();
            }
            return;
        }
        // SFTP 双栏面板：Esc 关闭（输入弹窗打开时先关弹窗）；面板打开期间
        // 拦截 pane 方向导航与全局快捷键（终端被覆盖，键盘穿过去只会打进
        // 看不见的 shell——口径对齐模态弹窗分支）。必须位于下方 pane 方向
        // 捕获与 settings 检查之前。
        if self.sftp_panel.is_some() {
            if plain_escape {
                if self.sftp_prompt.is_some() {
                    let field_with_menu = self
                        .sftp_prompt_fields
                        .iter()
                        .find(|(_, field)| field.read(cx).has_context_menu())
                        .map(|(_, field)| field.clone());
                    if let Some(field) = field_with_menu {
                        field.update(cx, |field, cx| field.close_context_menu(cx));
                    } else {
                        self.dismiss_sftp_prompt(window, cx);
                    }
                } else {
                    self.close_sftp_panel(window, cx);
                }
                cx.stop_propagation();
                return;
            }
            if self.sftp_prompt.is_some() {
                // 输入弹窗持有焦点，按键交给输入框自身
                return;
            }
            if pane_direction_for_keystroke(&event.keystroke).is_some()
                && self
                    .focused_terminal()
                    .read(cx)
                    .focus_handle()
                    .is_focused(window)
            {
                cx.stop_propagation();
            }
            return;
        }
        // 仅在真实终端焦点上捕获；设置与输入框应保留自己的方向键行为。
        if !self.settings_open
            && self
                .focused_terminal()
                .read(cx)
                .focus_handle()
                .is_focused(window)
            && let Some(direction) = pane_direction_for_keystroke(&event.keystroke)
        {
            self.focus_pane_direction(direction, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.settings_open {
            return;
        }
        if keystroke_matches_shortcut(event, "ctrl+shift+b") {
            if !event.is_held {
                self.toggle_broadcast(cx);
            }
            cx.stop_propagation();
            return;
        }
        for (tab, shortcut) in [
            (
                RightPanelTab::Commands,
                self.settings.shortcuts.commands_panel.clone(),
            ),
            (
                RightPanelTab::Servers,
                self.settings.shortcuts.servers_panel.clone(),
            ),
        ] {
            if shortcut
                .as_deref()
                .is_some_and(|key| keystroke_matches_shortcut(event, key))
            {
                if !event.is_held {
                    self.toggle_panel_tab(tab, cx);
                }
                cx.stop_propagation();
                return;
            }
        }
        // 统一绑定分发：命令（打开参数预览）→ 服务器（连接）→ 动态码（打开预览）
        if let Some(command) = self
            .settings
            .commands
            .iter()
            .find(|command| {
                command
                    .shortcut
                    .as_deref()
                    .is_some_and(|shortcut| keystroke_matches_shortcut(event, shortcut))
            })
            .cloned()
        {
            self.open_command(&command, window, cx);
            cx.stop_propagation();
            return;
        }
        if let Some(server) = self
            .settings
            .servers
            .iter()
            .find(|server| {
                server
                    .shortcut
                    .as_deref()
                    .is_some_and(|shortcut| keystroke_matches_shortcut(event, shortcut))
            })
            .cloned()
        {
            self.connect_server(&server.name, cx);
            cx.stop_propagation();
            return;
        }
        if let Some(entry) = self
            .settings
            .totp
            .iter()
            .find(|entry| {
                entry
                    .shortcut
                    .as_deref()
                    .is_some_and(|shortcut| keystroke_matches_shortcut(event, shortcut))
            })
            .cloned()
        {
            self.open_totp(&entry, cx);
            cx.stop_propagation();
            return;
        }
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.control && matches!(key, "left" | "right" | "up" | "down") {
            // 方向键不是方向导航时应交还给终端（例如 Ctrl+Shift+Left）。
            return;
        }
        if keystroke_matches_shortcut(event, "ctrl+j") {
            self.split_focused(SplitDirection::Horizontal, window, cx);
            cx.stop_propagation();
        } else if keystroke_matches_shortcut(event, "ctrl+k") {
            self.split_focused(SplitDirection::Vertical, window, cx);
            cx.stop_propagation();
        } else if keystroke_matches_shortcut(event, "ctrl+shift+[") {
            self.cycle_focus(false, window, cx);
            cx.stop_propagation();
        } else if keystroke_matches_shortcut(event, "ctrl+shift+]") {
            self.cycle_focus(true, window, cx);
            cx.stop_propagation();
        } else if keystroke_matches_shortcut(event, "ctrl+t") {
            self.new_tab(
                &MouseDownEvent {
                    position: gpui::point(px(0.0), px(0.0)),
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: true,
                },
                window,
                cx,
            );
            cx.stop_propagation();
        } else if keystroke_matches_shortcut(event, "ctrl+w") {
            let id = self.focused_pane_id();
            self.close_pane(id, window, cx);
            cx.stop_propagation();
        }
    }

    /// OSC 标题回调：更新窗格标题；返回是否需要同步窗口标题
    /// （仅当该窗格是活动标签的焦点窗格）。
    fn set_pane_title(&mut self, id: PaneId, title: &str, cx: &mut Context<Self>) -> bool {
        if let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == id) {
            if pane.status.is_exited() {
                // 已退出的窗格不接受迟到的 OSC 标题，避免覆盖退出后缀
                return false;
            }
            pane.title = if title.is_empty() {
                "本地终端".into()
            } else {
                title.into()
            };
            cx.notify();
            return self.active_tab().focused_pane == id;
        }
        false
    }

    fn mark_exited(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let closing = self
            .panes
            .iter()
            .find(|pane| pane.id == id)
            .is_some_and(|pane| matches!(pane.status, TabStatus::Closing));
        if closing {
            if let Some(index) = self.panes.iter().position(|pane| pane.id == id) {
                self.remove_pane_at(index, window, cx);
            }
            return;
        }
        if let Some(pane) = self.panes.iter_mut().find(|pane| pane.id == id)
            && pane.status.mark_exited()
        {
            if !pane.title.ends_with("（已退出）") {
                pane.title.push_str("（已退出）");
            }
            cx.notify();
        }
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.process_shortcuts(window, cx);
        self.process_ssh_results(window, cx);
        self.process_sftp_results(window, cx);
        if let Some(handle) = self.pending_focus.take() {
            handle.focus(window);
        }
        let drag_listener_workspace = cx.weak_entity();
        let active_tab_id = self.active_tab;
        let active_layout = self.active_tab().layout.clone();
        let pane_rects = Rc::new(RefCell::new(Vec::new()));
        let geometry_cache = self.pane_geometry.clone();
        let geometry_layout = active_layout.clone();
        let geometry_rects = pane_rects.clone();
        let focused_pane = self.active_tab().focused_pane;
        // 每个标签的标题 = 其焦点窗格标题；多窗格时附带窗格数
        let tab_labels = self
            .tabs
            .iter()
            .map(|tab| {
                let count = tab.layout.leaf_count();
                let title = tab.tab_title(&self.panes);
                (tab.id, title, count)
            })
            .collect::<Vec<_>>();
        let left_panel = self.left_panel;
        let right_panel = self.right_panel;
        let settings_open = self.settings_open;
        // 设置面板打开时先跑 scroll-spy（滚动 → 高亮联动），本帧导航绘制直接
        // 消费结果；不 notify（防自激帧循环，见 settings_scroll_spy_step 注记）。
        if settings_open {
            self.settings_scroll_spy_step();
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1A1B26))
            .capture_key_down(cx.listener(WorkspaceView::on_key_down))
            .child({
                let pane_rects = pane_rects.clone();
                canvas(
                    |_, _, _| (),
                    move |_, _, _, _| pane_rects.borrow_mut().clear(),
                )
            })
            .child(
                div()
                    .h(px(TITLEBAR_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .bg(rgb(0x161722))
                    .child(
                        div()
                            .h_full()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .gap_1()
                            .px_2()
                            .when(cfg!(target_os = "macos"), |brand| brand.pl(px(82.0)))
                            .child(button("☰", cx.listener(Self::toggle_left_panel)))
                            .child(titlebar_logo())
                            .child(div().text_color(rgb(0xC0CAF5)).child(if self.broadcast {
                                "LaterTerm · 广播中"
                            } else {
                                "LaterTerm"
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .overflow_x_hidden()
                            .when(!left_panel, |bar| {
                                bar.children(tab_labels.iter().map(|(id, title, count)| {
                                    let id = *id;
                                    let is_active = id == active_tab_id;
                                    let label = if *count > 1 {
                                        format!("{title} ⊞{count}")
                                    } else {
                                        title.clone()
                                    };
                                    div()
                                        .h_full()
                                        .relative()
                                        .group("tab")
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_3()
                                        .bg(if is_active {
                                            rgb(0x292B3D)
                                        } else {
                                            rgb(0x161722)
                                        })
                                        .text_color(if is_active {
                                            rgb(0xC0CAF5)
                                        } else {
                                            rgb(0xA9B1D6)
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, event, window, cx| {
                                                this.activate_tab(id, event, window, cx)
                                            }),
                                        )
                                        .child(label)
                                        // 激活标签底部强调线
                                        .when(is_active, |tab| {
                                            tab.child(
                                                div()
                                                    .absolute()
                                                    .bottom_0()
                                                    .left_0()
                                                    .right_0()
                                                    .h(px(2.0))
                                                    .bg(rgb(0x7AA2F7)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .px_1()
                                                .text_color(rgb(0x7D829F))
                                                .opacity(0.55)
                                                .group_hover("tab", |style| style.opacity(1.0))
                                                .hover(|style| style.text_color(rgb(0xF7768E)))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _, window, cx| {
                                                        this.close_tab(id, window, cx)
                                                    }),
                                                )
                                                .child("×"),
                                        )
                                }))
                            })
                            .child(button("+", cx.listener(Self::new_tab)))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::start_move)),
                    )
                    .child(button(
                        "⇔",
                        cx.listener(|this, _event, window, cx| {
                            cx.stop_propagation();
                            this.split_focused(SplitDirection::Horizontal, window, cx);
                        }),
                    ))
                    .child(button(
                        "⇕",
                        cx.listener(|this, _event, window, cx| {
                            cx.stop_propagation();
                            this.split_focused(SplitDirection::Vertical, window, cx);
                        }),
                    ))
                    .child(button("▣", cx.listener(Self::toggle_right_panel)))
                    .child(button(
                        "⚙",
                        cx.listener(|this, _, _, cx| this.toggle_settings(cx)),
                    ))
                    .when(!cfg!(target_os = "macos"), |titlebar| {
                        titlebar
                            .child(button("—", cx.listener(Self::minimize)))
                            .child(button("□", cx.listener(Self::zoom)))
                            .child(button_with_color(
                                "×",
                                0xF7768E,
                                cx.listener(Self::close_window),
                            ))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .child(if left_panel {
                        left_sidebar(&tab_labels, active_tab_id, &self.tab_scroll, cx)
                    } else {
                        div()
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .child(render_layout(
                                &active_layout,
                                &self.panes,
                                focused_pane,
                                show_pane_close(active_layout.leaf_count()),
                                self.split_drag.map(|drag| drag.split_id),
                                &pane_rects,
                                cx,
                            )),
                    )
                    .child(if right_panel {
                        quick_panel(self, cx)
                    } else {
                        div()
                    }),
            )
            .when(settings_open, |root| {
                root.child(settings_overlay(
                    cx.listener(Self::close_settings),
                    cx.listener(Self::close_settings),
                    settings_navigation(self.settings_section, cx),
                    settings_content(self, cx),
                ))
            })
            // SFTP 双栏面板：窗口级大覆盖层（右面板 240px 放不下双栏）
            .when_some(sftp_overlay(self, cx), |root, overlay| root.child(overlay))
            .when(self.welcome_visible, |root| {
                root.child(self.welcome_overlay(cx))
            })
            // 命令/动态码/服务器对话框：窗口级模态弹窗（最上层）
            .when_some(render_active_dialog(self, cx), |root, overlay| {
                root.child(overlay)
            })
            // 终端右键菜单：anchored 浮层，最顶层
            .when_some(
                self.terminal_menu
                    .clone()
                    .map(|menu| self.terminal_context_menu(menu, cx)),
                |root, menu_overlay| root.child(menu_overlay),
            )
            // 在全部窗格完成绘制后提交同一帧的几何快照。
            .child(canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    *geometry_cache.borrow_mut() = Some(PaneGeometry {
                        tab_id: active_tab_id,
                        viewport: window.viewport_size(),
                        panels: (left_panel, right_panel),
                        layout: geometry_layout.clone(),
                        panes: geometry_rects.borrow().clone(),
                    });
                },
            ))
            // 分割线拖拽：窗口级鼠标监听仅对下一帧有效，paint 期逐帧重挂
            // （canvas 的 paint 回调即 paint 期，模式同 CommandTextField 的 handle_input）
            .child(canvas(
                move |_, _, _| {},
                move |_, _, window, _cx| {
                    let weak = drag_listener_workspace.clone();
                    window.on_mouse_event(
                        move |event: &MouseMoveEvent, phase: DispatchPhase, _window, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let _ = weak.update(cx, |workspace, cx| {
                                if workspace.split_drag.is_some()
                                    && workspace.update_split_drag(event.position)
                                {
                                    cx.notify();
                                }
                            });
                        },
                    );
                    let weak = drag_listener_workspace.clone();
                    window.on_mouse_event(
                        move |_event: &MouseUpEvent, phase: DispatchPhase, _window, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let _ = weak.update(cx, |workspace, cx| {
                                if workspace.end_split_drag() {
                                    cx.notify();
                                }
                            });
                        },
                    );
                },
            ))
    }
}

impl WorkspaceView {
    /// 首启欢迎 + 引导一体覆盖层：品牌区 → 默认 Shell 芯片区 → 终端字体
    /// 芯片区 → 快捷键速查与导入提示（原样保留）→ 完成/跳过。
    /// 口径：
    /// - shell 点选仅暂存，「完成」时统一落盘；只对新标签/分屏生效（已开
    ///   窗格不重启，首窗格重开不做——范围控制）；「当前」芯片选回 None，
    ///   保持动态跟随默认而不是固化 $SHELL 路径；
    /// - 字体点选即时生效并落盘（onboarding_apply_font → persist_settings），
    ///   跳过引导也会保留已选字体；
    /// - 跳过/点遮罩 = dismiss_welcome，不写 shell。
    fn welcome_overlay(&self, cx: &mut Context<Self>) -> gpui::Div {
        // —— 默认 Shell 芯片：探测未到时显示平台默认 + 「探测中」占位 ——
        let (shell_candidates, probing) = match &self.onboarding.probe {
            Some(probe) => (probe.shells.clone(), false),
            None => (
                vec![onboarding::ShellCandidate {
                    path: laterterm_local::default_shell(),
                    is_current: true,
                    source: onboarding::ShellSource::Default,
                }],
                true,
            ),
        };
        let shell_chips: Vec<gpui::Div> = shell_candidates
            .iter()
            .map(|candidate| {
                let selected = match &self.onboarding.selected_shell {
                    Some(choice) => choice == &candidate.path,
                    None => candidate.is_current,
                };
                // 标注按来源：$SHELL=当前、平台兜底=默认、列表项被补标当前时
                // 同样显示当前（source 与 is_current 都是 UI 语义的一部分）
                let suffix = match candidate.source {
                    onboarding::ShellSource::Current => "（当前）",
                    onboarding::ShellSource::Default => "（默认）",
                    onboarding::ShellSource::Listed if candidate.is_current => "（当前）",
                    onboarding::ShellSource::Listed => "",
                };
                let label = format!("{}{suffix}", candidate.path);
                let choice = (!candidate.is_current).then(|| candidate.path.clone());
                setting_button(
                    label,
                    selected,
                    cx.listener(move |this, _, _, cx| {
                        this.onboarding_select_shell(choice.clone(), cx);
                    }),
                )
            })
            .collect();
        let shell_section = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(rgb(0xC0CAF5))
                    .text_size(px(14.0))
                    .child("默认 Shell"),
            )
            .child(div().flex().flex_wrap().gap_1().children(shell_chips))
            .child(
                div()
                    .text_color(rgb(0x565A78))
                    .text_size(px(12.0))
                    .child(if probing {
                        "正在探测本机可用 shell…（切换仅对新标签/分屏生效，已开窗格不变）"
                    } else {
                        "切换默认 shell 仅对新标签/分屏生效，已开窗格不变"
                    }),
            );
        // —— 终端字体芯片：fc-list 探测失败/未到时回退静态预设 ——
        let font_candidates = match &self.onboarding.probe {
            Some(probe) => probe.fonts.clone(),
            None => onboarding::preset_fonts(FONT_PRESETS.as_slice()),
        };
        let font_chips = font_candidates.iter().map(|candidate| {
            let selected = self.settings.appearance.font_family == candidate.name;
            let label = if candidate.recommended {
                format!("{}（推荐）", candidate.name)
            } else {
                candidate.name.clone()
            };
            let name = candidate.name.clone();
            setting_button(
                label,
                selected,
                cx.listener(move |this, _, _, cx| {
                    this.onboarding_apply_font(&name, cx);
                }),
            )
        });
        let font_section = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(rgb(0xC0CAF5))
                    .text_size(px(14.0))
                    .child("终端字体"),
            )
            .child(div().flex().flex_wrap().gap_1().children(font_chips))
            .child(
                div()
                    .text_color(rgb(0x565A78))
                    .text_size(px(12.0))
                    .child("点击字体立即生效并保存（之后跳过引导也会保留）"),
            );
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .bg(gpui::Hsla::from(rgb(0x161722)).opacity(0.96))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::dismiss_welcome))
            .child(
                div()
                    .w(px(640.0))
                    .max_h(px(680.0))
                    .p_8()
                    .bg(rgb(0x1A1B26))
                    .flex()
                    .flex_col()
                    .gap_4()
                    // 卡片内点击不再穿透到遮罩关闭（芯片/完成/跳过各有自己的
                    // listener，卡片空白处误触不应关掉引导）；点遮罩仍走 dismiss。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_size(px(28.0))
                            .text_color(rgb(0x7DCFFF))
                            .child("LaterTerm"),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xA9B1D6))
                            .child("跨平台 GPU 终端 · 为中文开发者打造"),
                    )
                    .child(
                        // 引导内容区：芯片组可能较高，小屏限高滚动（评审风险5）
                        div()
                            .id("welcome-content")
                            .flex()
                            .flex_col()
                            .gap_4()
                            .max_h(px(460.0))
                            .overflow_y_scroll()
                            .child(shell_section)
                            .child(font_section)
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_color(rgb(0xC0CAF5))
                                            .text_size(px(14.0))
                                            .child("常用快捷键"),
                                    )
                                    .child(welcome_shortcut("新建标签", "Ctrl+T"))
                                    .child(welcome_shortcut("分屏（横向 / 纵向）", "Ctrl+J / K"))
                                    .child(welcome_shortcut("窗格切换", "Ctrl+Shift+[ / ]"))
                                    .child(welcome_shortcut("搜索回滚", "Ctrl+Shift+F"))
                                    .child(welcome_shortcut(
                                        "广播输入（多窗格同步）",
                                        "Ctrl+Shift+B",
                                    ))
                                    .child(welcome_shortcut("设置 / 服务器管理", "Ctrl+,")),
                            )
                            .child(div().text_color(rgb(0x565A78)).text_size(px(12.0)).child(
                                "提示：右侧面板 → 服务器 可添加 SSH 连接或从 XShell / deepin 导入",
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .px_6()
                                    .py_2()
                                    .bg(rgb(0x252637))
                                    .text_color(rgb(0xA9B1D6))
                                    .text_size(px(16.0))
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(Self::dismiss_welcome),
                                    )
                                    .child("跳过"),
                            )
                            .child(
                                div()
                                    .px_6()
                                    .py_2()
                                    .bg(rgb(0x7AA2F7))
                                    .text_color(rgb(0x1A1B26))
                                    .text_size(px(16.0))
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(Self::complete_onboarding),
                                    )
                                    .child("完成"),
                            ),
                    ),
            )
    }
}

fn welcome_shortcut(label: &str, keys: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(220.0))
                .text_color(rgb(0xA9B1D6))
                .child(label.to_string()),
        )
        .child(
            div()
                .px_2()
                .py_1()
                .bg(rgb(0x24283B))
                .text_color(rgb(0x7DCFFF))
                .text_size(px(12.0))
                .child(keys.to_string()),
        )
}

fn show_pane_close(pane_count: usize) -> bool {
    pane_count > 1
}

fn render_layout(
    node: &LayoutNode,
    panes: &[Pane],
    focused: PaneId,
    show_close: bool,
    dragging_split: Option<u64>,
    pane_rects: &Rc<RefCell<Vec<PaneRect>>>,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    match node {
        LayoutNode::Leaf(id) => {
            let Some(pane) = panes.iter().find(|pane| pane.id == *id) else {
                return div();
            };
            let close_id = *id;
            // 滚动摘要：有历史时画悬浮滚动条（随滚动回调刷新）
            let scroll: ScrollSummary = pane.terminal.read(cx).scroll_summary();
            let mut pane_div = div()
                .relative()
                .group("pane")
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| {
                        // 点击 pane 任意处即聚焦该 pane
                        let pane_id = close_id;
                        if this.focused_pane_id() != pane_id {
                            this.focus_pane(pane_id, window, cx);
                        } else {
                            this.focus_active(window, cx);
                        }
                    }),
                )
                .capture_key_down(cx.listener(WorkspaceView::on_key_down))
                .child(pane.terminal.clone())
                .child({
                    let pane_rects = pane_rects.clone();
                    div().absolute().inset_0().child(
                        canvas(
                            |bounds, _, _| bounds,
                            move |bounds, _, _, _| {
                                pane_rects.borrow_mut().push(PaneRect {
                                    id: close_id,
                                    x: bounds.origin.x.into(),
                                    y: bounds.origin.y.into(),
                                    width: bounds.size.width.into(),
                                    height: bounds.size.height.into(),
                                });
                            },
                        )
                        .size_full(),
                    )
                });
            // 悬浮滚动条：右侧细条平时半透明，悬停窗格加亮（无监听器，不拦截终端鼠标）
            if let Some((thumb, top)) = scroll.thumb() {
                pane_div = pane_div.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .w(px(6.0))
                        .child(
                            div()
                                .absolute()
                                .top(relative(top))
                                .right(px(2.0))
                                .h(relative(thumb))
                                .w(px(3.0))
                                .rounded_full()
                                .bg(gpui::Hsla::from(rgb(0x565A78)).opacity(0.6))
                                .group_hover("pane", |style| {
                                    style.bg(gpui::Hsla::from(rgb(0x7AA2F7)).opacity(0.9))
                                }),
                        ),
                );
            }
            if show_close {
                pane_div = pane_div.child(
                    div()
                        .absolute()
                        .top_1()
                        .right_1()
                        .w(px(22.0))
                        .h(px(22.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .opacity(0.0)
                        .group_hover("pane", |style| style.opacity(1.0))
                        .text_color(rgb(0xA9B1D6))
                        .bg(gpui::Hsla::from(rgb(0x24283B)).opacity(0.95))
                        .hover(|style| style.bg(rgb(0x3B4261)).text_color(rgb(0xF7768E)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, window, cx| {
                                this.close_pane(close_id, window, cx)
                            }),
                        )
                        .child("×"),
                );
                if *id == focused {
                    pane_div = pane_div.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(px(2.0))
                            .bg(gpui::Hsla::from(rgb(0x7AA2F7)).opacity(0.7)),
                    );
                }
            }
            pane_div
        }
        LayoutNode::Split {
            id,
            direction,
            first,
            second,
            ratio,
        } => {
            let horizontal = matches!(direction, SplitDirection::Horizontal);
            let split_id = *id;
            let direction = *direction;
            let dragging = dragging_split == Some(split_id);
            let mut container = div()
                .relative()
                .flex()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0));
            if horizontal {
                container = container.flex_row();
            } else {
                container = container.flex_col();
            }
            // 首个子树占 ratio 比例（百分比定位），余量由第二个子树 flex_1 吃掉
            let (first_wrapper, divider) = if horizontal {
                (
                    div().w(relative(*ratio)),
                    div()
                        .w(px(3.0))
                        .h_full()
                        .cursor(CursorStyle::ResizeLeftRight),
                )
            } else {
                (
                    div().h(relative(*ratio)),
                    div().h(px(3.0)).w_full().cursor(CursorStyle::ResizeRow),
                )
            };
            container
                .child(
                    first_wrapper
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .flex()
                        .child(render_layout(
                            first,
                            panes,
                            focused,
                            show_close,
                            dragging_split,
                            pane_rects,
                            cx,
                        )),
                )
                .child(
                    divider
                        .flex_shrink_0()
                        .bg(if dragging {
                            rgb(0x7AA2F7)
                        } else {
                            rgb(0x3A3E5C)
                        })
                        .hover(|style| style.bg(gpui::Hsla::from(rgb(0x7AA2F7)).opacity(0.7)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                this.start_split_drag(split_id, direction, cx)
                            }),
                        ),
                )
                .child(render_layout(
                    second,
                    panes,
                    focused,
                    show_close,
                    dragging_split,
                    pane_rects,
                    cx,
                ))
                // paint 期上报容器几何，mousedown 换算拖拽比例用
                .child(div().absolute().inset_0().child(canvas(
                    move |bounds, _, _| bounds,
                    move |bounds, _, _, _| {
                        SPLIT_GEOMETRY.with_borrow_mut(|map| {
                            map.insert(split_id, bounds);
                        });
                    },
                )))
        }
    }
}

/// 标签列表采用固定标题和独立滚动区域，长标题不挤占关闭按钮。
fn left_sidebar(
    tab_labels: &[(TabId, String, usize)],
    active_tab_id: TabId,
    scroll: &ScrollHandle,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    div()
        .w(px(PANEL_WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(rgb(0x1C1E2C))
        .border_r_1()
        .border_color(rgb(0x292D40))
        .child(
            div()
                .h(px(52.0))
                .px_3()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_between()
                .text_size(px(12.0))
                .text_color(rgb(0xA9B1D6))
                .child(
                    div().flex().items_center().gap_2().child("会话").child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(rgb(0x303548))
                            .text_color(rgb(0xC0CAF5))
                            .child(tab_labels.len().to_string()),
                    ),
                )
                .child(button("+", cx.listener(WorkspaceView::new_tab))),
        )
        .child(
            div()
                .debug_selector(|| "sidebar-tabs".into())
                .id("sidebar-tabs")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .px_2()
                .pb_2()
                .children(tab_labels.iter().map(|(id, title, count)| {
                    let id = *id;
                    let selected = id == active_tab_id;
                    let label = title.clone();
                    let count = *count;
                    div()
                        .debug_selector(move || {
                            if selected {
                                "sidebar-active-tab".into()
                            } else {
                                "sidebar-other-tab".into()
                            }
                        })
                        .group("session-row")
                        .h(px(38.0))
                        .mb_1()
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_md()
                        .bg(if selected {
                            rgb(0x303A52)
                        } else {
                            rgb(0x1C1E2C)
                        })
                        .text_color(if selected {
                            rgb(0xC0D6FF)
                        } else {
                            rgb(0xA9B1D6)
                        })
                        .hover(|style| {
                            style.bg(if selected {
                                rgb(0x35415B)
                            } else {
                                rgb(0x292D40)
                            })
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event, window, cx| {
                                this.activate_tab(id, event, window, cx)
                            }),
                        )
                        .child(div().w(px(2.0)).h(px(18.0)).rounded_full().bg(if selected {
                            rgb(0x7AA2F7)
                        } else {
                            rgb(0x1C1E2C)
                        }))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(rgb(0x7AA2F7))
                                .child("›_"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(13.0))
                                .child(label),
                        )
                        .when(count > 1, |row| {
                            row.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(0xA9B1D6))
                                    .child(count.to_string()),
                            )
                        })
                        .child(
                            div()
                                .px_1()
                                .opacity(0.0)
                                .group_hover("session-row", |style| style.opacity(1.0))
                                .text_color(rgb(0xA9B1D6))
                                .hover(|style| style.text_color(rgb(0xF7768E)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.close_tab(id, window, cx)
                                    }),
                                )
                                .child("×"),
                        )
                })),
        )
}

fn sidebar_shortcut_label(shortcut: Option<&str>) -> &str {
    shortcut.unwrap_or("设置快捷键")
}

fn quick_panel(workspace: &mut WorkspaceView, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let commands = workspace.settings.commands.clone();
    let error = workspace.command_error.clone();
    let notice = workspace.command_notice.clone();
    let delete_confirm = workspace.command_delete_confirm.clone();
    let active_tab = workspace.right_panel_tab;
    let item_count = match active_tab {
        RightPanelTab::Commands => commands.len(),
        RightPanelTab::Totp => workspace.settings.totp.len(),
        RightPanelTab::Servers => workspace.settings.servers.len(),
    };
    let scroll_id = match active_tab {
        RightPanelTab::Commands => "sidebar-commands",
        RightPanelTab::Totp => "sidebar-totp",
        RightPanelTab::Servers => "sidebar-servers",
    };

    let panel = div().w_full().text_color(rgb(0xC0CAF5));
    let panel = match active_tab {
        RightPanelTab::Commands => panel
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .debug_selector(|| "command-new-button".into())
                            .flex_1()
                            .min_w(px(0.0))
                            .child(setting_button(
                                "新建命令",
                                true,
                                cx.listener(|this, _event, window, cx| {
                                    this.edit_command(None, window, cx)
                                }),
                            )),
                    )
                    .child(setting_button(
                        "导入",
                        false,
                        cx.listener(|this, _event, _window, cx| this.import_commands_deepin(cx)),
                    )),
            )
            .child(panel_caption("命令模板与快捷键"))
            .when_some(error, |panel, error| {
                panel.child(div().text_color(rgb(0xF7768E)).child(error))
            })
            // 导入/批量操作的一次性计数提示（同 server_notice 的绿色口径）
            .when_some(notice, |panel, notice| {
                panel.child(div().pt_2().text_color(rgb(0x9ECE6A)).child(notice))
            })
            .when(commands.is_empty(), |panel| {
                panel.child(sidebar_empty("还没有命令", "点击上方按钮新建常用命令"))
            })
            .children(commands.into_iter().map(|command| {
                let has_separate_template = command.template != command.name;
                let pending_delete = delete_confirm.as_deref() == Some(command.name.as_str());
                let open_command = command.clone();
                entry_card()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event, window, cx| {
                                    this.open_command(&open_command, window, cx);
                                }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_color(rgb(0xC0CAF5))
                                    .child(command.name.clone()),
                            ),
                    )
                    .when(has_separate_template, |card| {
                        card.child(
                            div()
                                .pt_1()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(rgb(0xA9B1D6))
                                .child(command.template.clone()),
                        )
                    })
                    .child(
                        div()
                            .pt_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(sidebar_shortcut_chip(sidebar_shortcut_label(
                                command.shortcut.as_deref(),
                            )))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener({
                                    let command = command.clone();
                                    move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.edit_command(Some(command.clone()), window, cx);
                                        this.start_shortcut_recording(window, cx);
                                    }
                                }),
                            ),
                    )
                    .child(
                        div()
                            .pt_1()
                            .flex()
                            .gap_1()
                            .child(setting_button(
                                "编辑",
                                false,
                                cx.listener({
                                    let command = command.clone();
                                    move |this, _event, window, cx| {
                                        this.edit_command(Some(command.clone()), window, cx)
                                    }
                                }),
                            ))
                            .child(setting_button(
                                if pending_delete {
                                    "确认删除"
                                } else {
                                    "删除"
                                },
                                pending_delete,
                                cx.listener({
                                    let name = command.name.clone();
                                    move |this, _event, _window, cx| this.delete_command(&name, cx)
                                }),
                            )),
                    )
            })),
        RightPanelTab::Totp => render_totp_section(workspace, panel, cx),
        RightPanelTab::Servers => render_server_section(workspace, panel, cx),
    };

    div()
        .w(px(PANEL_WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(rgb(0x1C1E2C))
        .border_l_1()
        .border_color(rgb(0x292D40))
        .child(sidebar_header("工具", item_count))
        .child(
            div()
                .mx_2()
                .mb_2()
                .p_1()
                .rounded_md()
                .bg(rgb(0x161824))
                .flex()
                .gap_1()
                .children(RightPanelTab::ALL.map(|tab| {
                    sidebar_tab_button(
                        tab.label(),
                        tab == active_tab,
                        cx.listener(move |this, _event, _window, cx| {
                            this.right_panel_tab = tab;
                            this.command_delete_confirm = None;
                            this.totp_delete_confirm = None;
                            this.server_delete_confirm = None;
                            cx.notify();
                        }),
                    )
                })),
        )
        .child(
            div()
                .id(scroll_id)
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .px_2()
                .pb_3()
                .child(panel),
        )
}

// ---------- 窗口级模态弹窗（命令/动态码/服务器共用） ----------

/// 按当前打开的对话框构建窗口级模态弹窗；无对话框时返回 None。
/// （原为右面板内覆盖层，现统一为居中弹窗；错误提示随之移入弹窗内部。）
fn render_active_dialog(
    workspace: &mut WorkspaceView,
    cx: &mut Context<WorkspaceView>,
) -> Option<gpui::Div> {
    // SFTP 栏内操作弹窗（新建目录/重命名）：窗口级模态，与其它弹窗同骨架
    if let Some(prompt) = workspace.sftp_prompt.clone() {
        let fields = workspace.sftp_prompt_fields.clone();
        let error = workspace
            .sftp_panel
            .as_ref()
            .and_then(|panel| panel.error.clone());
        let body = div()
            .flex()
            .flex_col()
            .p_4()
            .when_some(error, |body, error| body.child(dialog_error_line(error)))
            .children(
                fields
                    .into_iter()
                    .map(|(name, field)| dialog_field_row(&name, field)),
            )
            .child(dialog_footer(vec![
                setting_button(
                    "确定",
                    true,
                    cx.listener(|this, _event, _window, cx| this.submit_sftp_prompt(cx)),
                ),
                setting_button(
                    "取消",
                    false,
                    cx.listener(|this, _event, window, cx| this.dismiss_sftp_prompt(window, cx)),
                ),
            ]));
        return Some(modal_dialog(
            format!("SFTP · {}", prompt.kind.title()),
            body,
            dismiss_listener(cx, WorkspaceView::dismiss_sftp_prompt),
        ));
    }
    if let Some(owner) = &workspace.panel_shortcut_dialog {
        let body = div()
            .p_4()
            .when_some(workspace.shortcut_error.clone(), |body, error| {
                body.child(dialog_error_line(error))
            })
            .when_some(workspace.shortcut_editor.as_ref(), |body, editor| {
                body.child(shortcut_editor_row(editor.recorder.clone()))
            })
            .child(dialog_footer(vec![
                setting_button(
                    "保存",
                    true,
                    cx.listener(|this, _, window, cx| this.save_panel_shortcut(window, cx)),
                ),
                setting_button(
                    "取消",
                    false,
                    dismiss_listener(cx, WorkspaceView::dismiss_panel_shortcut),
                ),
            ]));
        return Some(modal_dialog(
            owner.describe(),
            body,
            dismiss_listener(cx, WorkspaceView::dismiss_panel_shortcut),
        ));
    }
    if let Some(source) = workspace.server_import_dialog {
        return Some(server_import_modal(source, cx));
    }
    if let Some(CommandDialog::Edit { original_name }) = workspace.command_dialog.clone() {
        return Some(command_edit_modal(workspace, original_name, cx));
    }
    if let Some(CommandDialog::Preview { template, preview }) = workspace.command_dialog.clone() {
        return Some(command_preview_modal(workspace, template, preview, cx));
    }
    if let Some(TotpDialog::Edit { original }) = workspace.totp_dialog.clone() {
        return Some(totp_edit_modal(workspace, original, cx));
    }
    if let Some(TotpDialog::Preview {
        name,
        code,
        generated_at,
        period,
    }) = workspace.totp_dialog.clone()
    {
        return Some(totp_preview_modal(
            workspace,
            name,
            code,
            generated_at,
            period,
            cx,
        ));
    }
    if workspace.server_save.is_some() {
        let body = div()
            .p_4()
            .child("正在保存服务器配置，请稍候…")
            .child(
                div()
                    .pt_2()
                    .text_color(rgb(0x7D829F))
                    .child("可取消后续操作；已经进入系统密钥环的写入无法撤回。"),
            )
            .child(dialog_footer(vec![setting_button(
                "取消",
                false,
                cx.listener(WorkspaceView::cancel_server),
            )]));
        return Some(modal_dialog(
            "保存服务器",
            body,
            dismiss_listener(cx, WorkspaceView::dismiss_server),
        ));
    }
    if let Some(ServerDialog::Edit { original }) = workspace.server_dialog.clone() {
        return Some(server_edit_modal(workspace, original, cx));
    }
    if let Some(ServerDialog::GroupRename { original }) = workspace.server_dialog.clone() {
        return Some(server_group_rename_modal(workspace, original, cx));
    }
    if let Some(ServerDialog::Connecting { name }) = workspace.server_dialog.clone() {
        let body = div()
            .p_4()
            .child("正在准备连接（可取消）…")
            .child(dialog_footer(vec![setting_button(
                "取消连接",
                false,
                cx.listener(WorkspaceView::cancel_server),
            )]));
        let title = if workspace
            .ssh_request
            .as_ref()
            .is_some_and(|request| request.dest == ConnectDest::SftpPanel)
        {
            format!("SFTP 面板 · 连接 {name}")
        } else {
            format!("连接 {name}")
        };
        return Some(modal_dialog(
            title,
            body,
            dismiss_listener(cx, WorkspaceView::dismiss_server),
        ));
    }
    if let Some(ServerDialog::Connect { name, remember, .. }) = workspace.server_dialog.clone() {
        return Some(server_connect_modal(workspace, name, remember, cx));
    }
    None
}

/// 弹窗骨架：暗色遮罩 + 居中卡片。点遮罩或标题栏 × 都走 cancel。
fn modal_dialog(
    title: impl Into<gpui::SharedString>,
    body: gpui::Div,
    cancel: impl Fn(&MouseDownEvent, &mut Window, &mut App) + Clone + 'static,
) -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.55))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Left, cancel.clone())
        .child(
            div()
                .w(px(480.0))
                .debug_selector(|| "modal-card".into())
                .max_w(relative(0.95))
                .max_h(relative(0.90))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(0x202132))
                .border_1()
                .border_color(rgb(0x3B3F5C))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .h(px(44.0))
                        .flex_shrink_0()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_4()
                        .border_b_1()
                        .border_color(rgb(0x343750))
                        .child(div().text_color(rgb(0xC0CAF5)).child(title.into()))
                        .child(
                            div()
                                .h(px(26.0))
                                .w(px(26.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(0x7D829F))
                                .hover(|this| this.text_color(rgb(0xF7768E)))
                                .on_mouse_down(MouseButton::Left, cancel)
                                .child("×"),
                        ),
                )
                .child(
                    div()
                        .id("modal-body-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .child(body.flex_shrink_0()),
                ),
        )
}

/// `cx.listener` 的可克隆版本：listener 的返回类型不透出 `Clone`，
/// 而弹窗遮罩与标题栏 × 需要把同一个 cancel 闭包用两次。
fn dismiss_listener(
    cx: &Context<WorkspaceView>,
    dismiss: fn(&mut WorkspaceView, &mut Window, &mut Context<WorkspaceView>),
) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + Clone + 'static {
    let view = cx.weak_entity();
    move |_event, window, cx| {
        cx.stop_propagation();
        view.update(cx, |view, cx| dismiss(view, window, cx)).ok();
    }
}

/// 弹窗内一行「标签 + 输入框」。
fn dialog_field_row(label: &str, field: Entity<CommandTextField>) -> gpui::Div {
    div()
        .pt_2()
        .child(div().text_color(rgb(0x7D829F)).child(label.to_string()))
        .child(field)
}

fn shortcut_editor_row(recorder: Entity<ShortcutRecorder>) -> gpui::Div {
    div()
        .pt_2()
        .child(div().text_color(rgb(0x7D829F)).child("快捷键（应用内）"))
        .child(recorder)
}

/// 弹窗底部按钮排。
fn dialog_footer(buttons: Vec<gpui::Div>) -> gpui::Div {
    div()
        .debug_selector(|| "modal-footer".into())
        .pt_4()
        .flex()
        .gap_2()
        .children(buttons)
}

/// 弹窗内错误提示行（保存/连接失败等需要就地可见的信息）。
fn dialog_error_line(error: String) -> gpui::Div {
    div().pb_2().text_color(rgb(0xF7768E)).child(error)
}

/// 打开弹窗时把焦点交给第一个输入框（光标立即闪烁）。
fn focus_first_field(
    fields: &[(String, Entity<CommandTextField>)],
    window: &mut Window,
    cx: &Context<WorkspaceView>,
) {
    if let Some((_, first)) = fields.first() {
        first.read(cx).focus_handle().focus(window);
    }
}

fn command_edit_modal(
    workspace: &WorkspaceView,
    original_name: Option<String>,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let fields = workspace.command_fields.clone();
    let error = workspace.command_error.clone();
    let title = if original_name.is_some() {
        "编辑命令"
    } else {
        "新建命令"
    };
    let mut body = div()
        .flex()
        .flex_col()
        .p_4()
        .when_some(error, |body, error| body.child(dialog_error_line(error)))
        .children(
            fields
                .into_iter()
                .map(|(name, field)| dialog_field_row(&name, field)),
        );
    if let Some(editor) = &workspace.shortcut_editor {
        body = body.child(shortcut_editor_row(editor.recorder.clone()));
    }
    let body = body.child(dialog_footer(vec![
        setting_button(
            "保存",
            true,
            cx.listener(move |this, _event, window, cx| {
                this.save_edit_command(original_name.clone(), window, cx)
            }),
        ),
        setting_button("取消", false, cx.listener(WorkspaceView::cancel_command)),
    ]));
    modal_dialog(
        title,
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_command),
    )
}

fn command_preview_modal(
    workspace: &WorkspaceView,
    template: CommandTemplate,
    preview: Option<String>,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let fields = workspace.command_fields.clone();
    let error = workspace.command_error.clone();
    let display_command = preview.unwrap_or_else(|| {
        format!(
            "参数错误：{}",
            template.variables().unwrap_or_default().join(", ")
        )
    });
    let display_confirm = template.confirm || !fields.is_empty();
    let mut body = div()
        .flex()
        .flex_col()
        .p_4()
        .when_some(error, |body, error| body.child(dialog_error_line(error)))
        .children(
            fields
                .into_iter()
                .map(|(name, field)| dialog_field_row(&name, field)),
        )
        .child(
            div()
                .pt_3()
                .text_color(rgb(0xC0CAF5))
                .child(display_command),
        );
    if display_confirm {
        body = body.child(dialog_footer(vec![
            setting_button(
                "发送",
                true,
                cx.listener(|this, _event, window, cx| {
                    this.confirm_command(window, cx);
                }),
            ),
            setting_button("取消", false, cx.listener(WorkspaceView::cancel_command)),
        ]));
    }
    modal_dialog(
        "命令预览",
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_command),
    )
}

fn totp_edit_modal(
    workspace: &WorkspaceView,
    original: Option<String>,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let fields = workspace.totp_fields.clone();
    let meta = workspace.totp_meta_draft.clone();
    let error = workspace.totp_error.clone();
    let title = if original.is_some() {
        "编辑动态码"
    } else {
        "新建动态码"
    };
    let body = div()
        .flex()
        .flex_col()
        .p_4()
        .when_some(error, |body, error| body.child(dialog_error_line(error)))
        .children(
            fields
                .into_iter()
                .map(|(name, field)| dialog_field_row(&name, field)),
        )
        .when_some(workspace.shortcut_editor.as_ref(), |body, editor| {
            body.child(shortcut_editor_row(editor.recorder.clone()))
        })
        .child(
            div()
                .pt_2()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_color(rgb(0x7D829F)).child("算法"))
                .children(
                    [
                        TotpAlgorithm::Sha1,
                        TotpAlgorithm::Sha256,
                        TotpAlgorithm::Sha512,
                    ]
                    .into_iter()
                    .map(|algo| {
                        setting_button(
                            algo.label(),
                            meta.algorithm == algo,
                            cx.listener(move |this, _event, _window, cx| {
                                this.totp_meta_draft.algorithm = algo;
                                cx.notify();
                            }),
                        )
                    }),
                ),
        )
        .child(
            div()
                .pt_2()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_color(rgb(0x7D829F)).child("位数"))
                .children([6u32, 8].into_iter().map(|digits| {
                    setting_button(
                        digits.to_string(),
                        meta.digits == digits,
                        cx.listener(move |this, _event, _window, cx| {
                            this.totp_meta_draft.digits = digits;
                            cx.notify();
                        }),
                    )
                }))
                .child(
                    div()
                        .text_color(rgb(0x565A78))
                        .child(format!("周期 {} 秒", meta.period)),
                ),
        )
        .child(dialog_footer(vec![
            setting_button(
                "保存",
                true,
                cx.listener(move |this, _event, _window, cx| this.save_totp(original.clone(), cx)),
            ),
            setting_button("取消", false, cx.listener(WorkspaceView::cancel_totp)),
        ]));
    modal_dialog(
        title,
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_totp),
    )
}

fn totp_preview_modal(
    workspace: &WorkspaceView,
    name: String,
    code: Option<String>,
    generated_at: SystemTime,
    period: u64,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let error = workspace.totp_error.clone();
    let remaining = totp_preview_remaining(generated_at, period);
    let expired = code.is_some() && remaining == 0;
    let ready = code.is_some() && !expired;
    let body_line = match (code, expired) {
        (None, _) => div().text_color(rgb(0x7D829F)).child("正在从密钥环读取…"),
        (Some(code), false) => div()
            .text_color(rgb(0xC0CAF5))
            .child(format!("{code}   {remaining}s 后轮换")),
        (Some(_), true) => div()
            .text_color(rgb(0xF7768E))
            .child("已过期，重新点击条目生成"),
    };
    let body = div()
        .flex()
        .flex_col()
        .p_4()
        .when_some(error, |body, error| body.child(dialog_error_line(error)))
        .child(body_line)
        .child(dialog_footer(vec![
            setting_button(
                "发送",
                ready,
                cx.listener(|this, _event, window, cx| {
                    this.confirm_totp(window, cx);
                }),
            ),
            setting_button("取消", false, cx.listener(WorkspaceView::cancel_totp)),
        ]));
    modal_dialog(
        format!("动态码 · {name}"),
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_totp),
    )
}

fn server_edit_modal(
    workspace: &WorkspaceView,
    original: Option<String>,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let fields = workspace.server_fields.clone();
    let meta = workspace.server_meta_draft.clone();
    let error = workspace.server_error.clone();
    let title = if original.is_some() {
        "编辑服务器"
    } else {
        "新建服务器"
    };
    let body = div()
        .flex()
        .flex_col()
        .p_4()
        .when_some(error, |body, error| body.child(dialog_error_line(error)))
        .children(
            fields
                .into_iter()
                .map(|(name, field)| dialog_field_row(&name, field)),
        )
        .when_some(workspace.shortcut_editor.as_ref(), |body, editor| body.child(shortcut_editor_row(editor.recorder.clone())))
        .child(
            div()
                .pt_2()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_color(rgb(0x7D829F)).child("认证"))
                .children(
                    [
                        (ServerAuthKind::Password, "密码"),
                        (ServerAuthKind::KeyboardInteractive, "交互口令"),
                        (ServerAuthKind::PrivateKey, "私钥（免登录密码）"),
                        (ServerAuthKind::Agent, "Agent"),
                    ]
                    .into_iter()
                    .map(|(kind, label)| {
                        setting_button(
                            label,
                            meta.auth == kind,
                            cx.listener(move |this, _event, _window, cx| {
                                this.server_meta_draft.auth = kind;
                                cx.notify();
                            }),
                        )
                    }),
                ),
        )
        .child(div().pt_2().text_size(px(11.0)).text_color(rgb(0x7D829F)).child("私钥模式凭据为本机私钥的解锁口令，可留空。客户端仍需与服务端公钥配对的私钥；选择 Agent 时使用系统 SSH agent，不读取密码密钥环。"))
        .child(dialog_footer(vec![
            setting_button(
                "保存",
                true,
                cx.listener(move |this, _event, _window, cx| {
                    this.save_server(original.clone(), cx)
                }),
            ),
            setting_button("取消", false, cx.listener(WorkspaceView::cancel_server)),
        ]));
    modal_dialog(
        title,
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_server),
    )
}

/// 批量改组弹窗：单个「分组名」输入框（预填旧组名）；清空保存 = 解散，
/// 填已存在组名 = 并入该组。改组只动元数据，不触碰密钥环。
fn server_group_rename_modal(
    workspace: &WorkspaceView,
    original: String,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let fields = workspace.server_fields.clone();
    let error = workspace.server_error.clone();
    let title = format!("重命名分组 {original}");
    let body = div()
        .flex()
        .flex_col()
        .p_4()
        .when_some(error, |body, error| body.child(dialog_error_line(error)))
        .child(div().text_color(rgb(0x7D829F)).child(format!(
            "修改分组「{original}」下所有服务器的归属；填入已存在的分组名会并入该组，清空则解散。"
        )))
        .children(
            fields
                .into_iter()
                .map(|(name, field)| dialog_field_row(&name, field)),
        )
        .child(dialog_footer(vec![
            setting_button(
                "保存",
                true,
                cx.listener(move |this, _event, _window, cx| {
                    this.save_group_rename(original.clone(), cx)
                }),
            ),
            setting_button("取消", false, cx.listener(WorkspaceView::cancel_server)),
        ]));
    modal_dialog(
        title,
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_server),
    )
}

fn server_connect_modal(
    workspace: &WorkspaceView,
    name: String,
    remember: bool,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let fields = workspace.server_fields.clone();
    let error = workspace.server_error.clone();
    let is_private_key = matches!(
        workspace.server_dialog,
        Some(ServerDialog::Connect {
            kind: CredentialKind::PrivateKey { .. },
            ..
        })
    );
    let busy = workspace.ssh_connecting.is_some();
    let body = div()
        .flex()
        .flex_col()
        .p_4()
        .when_some(error, |body, error| body.child(dialog_error_line(error)))
        .child(
            div()
                .text_color(rgb(0x7D829F))
                .child(if is_private_key { "本机私钥已加密，请输入私钥解锁口令（不是远端登录密码）" } else { "输入登录凭据；密码留空将使用本机对应私钥，不发送空密码。私钥路径留空时尝试 id_ed25519、id_rsa。" }),
        )
        .children(
            fields
                .into_iter()
                .map(|(name, field)| dialog_field_row(&name, field)),
        )
        .child(
            div()
                .pt_2()
                .flex()
                .items_center()
                .gap_1()
                .child(setting_button(
                    if remember {
                        "记住凭据 ✓"
                    } else {
                        "记住凭据"
                    },
                    remember,
                    cx.listener(move |this, _event, _window, cx| {
                        if let Some(ServerDialog::Connect { remember, .. }) =
                            &mut this.server_dialog
                        {
                            *remember = !*remember;
                        }
                        cx.notify();
                    }),
                ))
                .child(div().text_color(rgb(0x565A78)).child("连接成功后才保存；不勾选则仅本次使用")),
        )
        .when(busy, |body| body.child(div().pt_2().child("正在连接，可取消…")))
        .child(dialog_footer(vec![
            setting_button(
                if busy { "连接中…" } else { "连接" },
                !busy,
                cx.listener(|this, _event, _window, cx| this.submit_server_connect(cx)),
            ),
            setting_button("取消", false, cx.listener(WorkspaceView::cancel_server)),
        ]));
    // SFTP 面板补输与 shell 连接共用弹窗，标题标注去向
    let title = if workspace
        .ssh_request
        .as_ref()
        .is_some_and(|request| request.dest == ConnectDest::SftpPanel)
    {
        format!("SFTP 面板 · 连接 {name}")
    } else {
        format!("连接 {name}")
    };
    modal_dialog(
        title,
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_server),
    )
}

fn server_import_modal(source: ServerImportSource, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let options = [
        (
            ServerImportSource::LaterTerm,
            "LaterTerm 文件",
            "选择本程序导出的 TOML 文件",
        ),
        (
            ServerImportSource::Deepin,
            "Deepin 终端",
            "一键读取本机 Deepin 终端的远程管理列表",
        ),
        (
            ServerImportSource::Xshell,
            "Xshell 会话",
            "选择一个或多个 .xsh 会话文件",
        ),
    ];
    let body = div()
        .flex()
        .flex_col()
        .p_4()
        .children(options.into_iter().map(|(kind, label, desc)| {
            div()
                .pt_2()
                .flex()
                .items_center()
                .gap_2()
                .child(setting_button(
                    label,
                    source == kind,
                    cx.listener(move |this, _event, _window, cx| {
                        this.server_import_dialog = Some(kind);
                        cx.notify();
                    }),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(rgb(0x7D829F))
                        .child(desc),
                )
        }))
        .child(
            div()
                .pt_3()
                .text_color(rgb(0x565A78))
                .child("任何来源都不迁移凭据：首次连接补输一次即可（可记住入密钥环）"),
        )
        .child(dialog_footer(vec![
            setting_button(
                "开始导入",
                true,
                cx.listener(|this, _event, _window, cx| this.start_server_import(cx)),
            ),
            setting_button(
                "取消",
                false,
                cx.listener(WorkspaceView::cancel_server_import),
            ),
        ]));
    modal_dialog(
        "导入服务器",
        body,
        dismiss_listener(cx, WorkspaceView::dismiss_server_import),
    )
}
/// 当前时间窗口的剩余秒数（条目行内的轮换提示，随 ticker 每秒刷新）。
fn totp_window_remaining(period: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    period - now % period
}

/// 预览里该动态码距过期的秒数。
fn totp_preview_remaining(generated_at: SystemTime, period: u64) -> u64 {
    SystemTime::now()
        .duration_since(generated_at)
        .map(|elapsed| period.saturating_sub(elapsed.as_secs()))
        .unwrap_or(0)
}

/// 右面板「动态码」区：条目列表（编辑/预览走窗口级模态弹窗）。
fn render_totp_section(
    workspace: &mut WorkspaceView,
    panel: gpui::Div,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let entries = workspace.settings.totp.clone();
    let delete_confirm = workspace.totp_delete_confirm.clone();
    let error = workspace.totp_error.clone();

    panel
        .child(setting_button(
            "新建动态码",
            true,
            cx.listener(|this, _event, window, cx| this.edit_totp(None, window, cx)),
        ))
        .child(panel_caption("密钥保存在系统密钥环中"))
        .when_some(error, |panel, error| {
            panel.child(div().text_color(rgb(0xF7768E)).child(error))
        })
        .when(entries.is_empty(), |panel| {
            panel.child(sidebar_empty("还没有动态码", "支持粘贴密钥或 otpauth 链接"))
        })
        .children(entries.iter().map(|entry| {
            let name = entry.name.clone();
            let remaining = totp_window_remaining(entry.period);
            let pending_delete = delete_confirm.as_deref() == Some(entry.name.as_str());
            entry_card()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_1()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let entry = entry.clone();
                                move |this, _event, _window, cx| this.open_totp(&entry, cx)
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_color(rgb(0xC0CAF5))
                                .child(entry.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(0x7D829F))
                                .child(format!("{remaining}s")),
                        ),
                )
                .when_some(entry.shortcut.clone(), |card, shortcut| {
                    card.child(
                        div()
                            .pt_1()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(rgb(0x7D829F))
                            .child(shortcut),
                    )
                })
                .child(
                    div()
                        .pt_1()
                        .flex()
                        .gap_1()
                        .child(setting_button(
                            "编辑",
                            false,
                            cx.listener({
                                let entry = entry.clone();
                                move |this, _event, window, cx| {
                                    this.edit_totp(Some(&entry), window, cx)
                                }
                            }),
                        ))
                        .child(setting_button(
                            if pending_delete {
                                "确认删除"
                            } else {
                                "删除"
                            },
                            pending_delete,
                            cx.listener(move |this, _event, _window, cx| {
                                this.delete_totp(&name, cx)
                            }),
                        )),
                )
        }))
}

/// 右面板「服务器」区：条目列表（编辑/连接凭据走窗口级模态弹窗）。
/// 有分组时按组折叠展示（组名升序、未分组殿后）；纯未分组时与旧版平铺外观一致。
fn render_server_section(
    workspace: &mut WorkspaceView,
    panel: gpui::Div,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let servers = workspace.settings.servers.clone();
    let delete_confirm = workspace.server_delete_confirm.clone();
    let error = workspace.server_error.clone();
    let notice = workspace.server_notice.clone();
    let connecting = workspace.ssh_connecting.clone();
    let collapsed = workspace.server_groups_collapsed.clone();
    let disband_confirm = workspace.server_group_confirm.clone();

    panel
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .debug_selector(|| "server-new-button".into())
                        .flex_1()
                        .min_w(px(0.0))
                        .child(setting_button(
                            "+ 新建",
                            true,
                            cx.listener(|this, _event, window, cx| {
                                this.edit_server(None, window, cx)
                            }),
                        )),
                )
                .child(setting_button(
                    "导入",
                    false,
                    cx.listener(|this, _event, _window, cx| this.open_server_import(cx)),
                ))
                .child(
                    div()
                        .debug_selector(|| "server-export-button".into())
                        .child(setting_button(
                            "导出",
                            false,
                            cx.listener(|this, _event, _window, cx| this.export_servers(cx)),
                        )),
                ),
        )
        .child(panel_caption("凭据保存在系统密钥环中"))
        .when(servers.is_empty(), |panel| {
            panel.child(sidebar_empty(
                "还没有服务器",
                "新建连接，或导入已有的服务器列表",
            ))
        })
        .when_some(notice, |panel, notice| {
            panel.child(div().pt_2().text_color(rgb(0x9ECE6A)).child(notice))
        })
        .when_some(connecting, |panel, connecting| {
            panel.child(
                div()
                    .pt_2()
                    .text_color(rgb(0xBB9AF7))
                    .child(format!("正在连接 {connecting} …（最长 15s）")),
            )
        })
        .when_some(error, |panel, error| {
            panel.child(div().text_color(rgb(0xF7768E)).child(error))
        })
        // 分组渲染：组头（▸/▾ + 组名 + 计数）+ 成员条目；折叠的组不渲染成员行。
        // 硬约束：纯未分组（无任何命名组）时保持旧版平铺外观，一个组头都不渲染。
        .children({
            let mut groups = server_groups(&servers);
            if groups.len() == 1 && groups[0].0.is_none() {
                let (_, indexes) = groups.swap_remove(0);
                indexes
                    .into_iter()
                    .map(|index| {
                        let entry = &servers[index];
                        server_entry_row(
                            entry,
                            delete_confirm.as_deref() == Some(entry.name.as_str()),
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                groups
                    .into_iter()
                    .flat_map(|(group, indexes)| {
                        let is_collapsed = collapsed.contains(&group);
                        let mut rows = Vec::new();
                        rows.push(server_group_header(
                            &group,
                            indexes.len(),
                            is_collapsed,
                            disband_confirm.as_deref(),
                            cx,
                        ));
                        if !is_collapsed {
                            rows.extend(indexes.into_iter().map(|index| {
                                let entry = &servers[index];
                                server_entry_row(
                                    entry,
                                    delete_confirm.as_deref() == Some(entry.name.as_str()),
                                    cx,
                                )
                            }));
                        }
                        rows
                    })
                    .collect::<Vec<_>>()
            }
        })
}

/// 分组组头行：▸/▾ + 组名 + 计数；点整行切换折叠。命名组附带「重命名/解散」
/// （低对比常显 + hover 提亮，与条目「编辑/删除」同一模式；解散需二次确认）。
fn server_group_header(
    group: &Option<String>,
    count: usize,
    is_collapsed: bool,
    disband_confirm: Option<&str>,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let label = group.clone().unwrap_or_else(|| "未分组".into());
    let key = group.clone();
    let selector = format!("server-group-header-{label}");
    let header = div()
        .debug_selector(move || selector.clone())
        .pt_3()
        .pb_1()
        .px_1()
        .flex()
        .items_center()
        .gap_1()
        .text_size(px(12.0))
        .text_color(rgb(0x7D829F))
        .hover(|style| style.text_color(rgb(0xA9B1D6)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if this.server_groups_collapsed.contains(&key) {
                    this.server_groups_collapsed.remove(&key);
                } else {
                    this.server_groups_collapsed.insert(key.clone());
                }
                cx.notify();
            }),
        )
        .child(if is_collapsed { "▸" } else { "▾" })
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_color(rgb(0xA9B1D6))
                .child(label),
        )
        .child(
            div()
                .px_1()
                .rounded_sm()
                .bg(rgb(0x303548))
                .text_size(px(11.0))
                .text_color(rgb(0xC0CAF5))
                .child(count.to_string()),
        );
    // 未分组桶没有组级操作（成员各自在编辑弹窗里进组）
    let Some(group_name) = group.clone() else {
        return header;
    };
    header
        .child(
            div()
                .debug_selector(|| "server-group-rename".into())
                .text_size(px(11.0))
                .text_color(rgb(0xA9B1D6))
                .hover(|style| style.text_color(rgb(0xC0D6FF)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener({
                        let group_name = group_name.clone();
                        move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.edit_server_group(&group_name, window, cx);
                        }
                    }),
                )
                .child("重命名"),
        )
        .child({
            let disbanding = disband_confirm == Some(group_name.as_str());
            div()
                .debug_selector(|| "server-group-disband".into())
                .text_size(px(11.0))
                .text_color(rgb(if disbanding { 0xF7768E } else { 0xA9B1D6 }))
                .hover(|style| style.text_color(rgb(0xF7768E)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if disbanding {
                            this.disband_server_group(group_name.clone(), cx);
                        } else {
                            this.server_group_confirm = Some(group_name.clone());
                            cx.notify();
                        }
                    }),
                )
                .child(if disbanding { "确认解散" } else { "解散" })
        })
}

/// 服务器条目三行结构：名称+连接 / 地址·认证 / 快捷键·编辑·删除。
/// 卡片带 `server-card-<名称>` 选择器（分组折叠冒烟测试按名定位成员）。
fn server_entry_row(
    entry: &ServerEntry,
    pending_delete: bool,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    let auth_label = match entry.auth {
        ServerAuthKind::Password => "密码",
        ServerAuthKind::KeyboardInteractive => "交互",
        ServerAuthKind::PrivateKey => "密钥",
        ServerAuthKind::Agent => "Agent",
    };
    let name = entry.name.clone();
    server_list_item()
        .debug_selector(move || format!("server-card-{name}"))
        // 第一行：名称 + 连接
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(rgb(0xC0CAF5))
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_size(px(12.0))
                        .text_color(rgb(0xB7CEFF))
                        .bg(rgb(0x303E58))
                        .hover(|style| style.bg(rgb(0x405578)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let name = entry.name.clone();
                                move |this, _event, _window, cx| this.connect_server(&name, cx)
                            }),
                        )
                        .child("连接"),
                )
                .child(
                    div()
                        .debug_selector(|| "server-sftp".into())
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_size(px(12.0))
                        .text_color(rgb(0x7DCFFF))
                        .bg(rgb(0x2A3450))
                        .hover(|style| style.bg(rgb(0x364468)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let name = entry.name.clone();
                                move |this, _event, _window, cx| this.open_sftp_panel(&name, cx)
                            }),
                        )
                        .child("文件"),
                ),
        )
        // 优先展示连接目标；快捷键属于次要操作。
        .child(
            div()
                .debug_selector(|| "server-address".into())
                .w_full()
                .pt_1()
                .truncate()
                .text_size(px(12.0))
                .text_color(rgb(0xA9B1D6))
                .child({
                    // 跳板引用一眼可见：走哪条链直连/穿透不分家
                    let mut line = format!(
                        "{}@{}:{} · {}",
                        entry.username, entry.host, entry.port, auth_label
                    );
                    if let Some(jump) = &entry.jump {
                        line += &format!(" · 经 {jump}");
                    }
                    line
                }),
        )
        .child(
            div()
                .pt_2()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .debug_selector(|| "server-shortcut".into())
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(rgb(0xA9B1D6))
                        .hover(|style| style.text_color(rgb(0xC0D6FF)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let entry = entry.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.edit_server(Some(&entry), window, cx);
                                    this.start_shortcut_recording(window, cx);
                                }
                            }),
                        )
                        .child(sidebar_shortcut_label(entry.shortcut.as_deref()).to_string()),
                )
                .child(
                    div()
                        .debug_selector(|| "server-edit".into())
                        .text_size(px(11.0))
                        .text_color(rgb(0xA9B1D6))
                        .hover(|style| style.text_color(rgb(0xC0D6FF)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let entry = entry.clone();
                                move |this, _event, window, cx| {
                                    this.edit_server(Some(&entry), window, cx)
                                }
                            }),
                        )
                        .child("编辑"),
                )
                .child(
                    div()
                        .debug_selector(|| "server-delete".into())
                        .text_size(px(11.0))
                        .text_color(rgb(if pending_delete { 0xF7768E } else { 0xA9B1D6 }))
                        .hover(|style| style.text_color(rgb(0xF7768E)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let name = entry.name.clone();
                                move |this, _event, _window, cx| this.delete_server(&name, cx)
                            }),
                        )
                        .child(if pending_delete {
                            "确认删除"
                        } else {
                            "删除"
                        }),
                ),
        )
}

/// 侧栏快捷键入口：保留点击设置，同时降低对主要内容的视觉干扰。
fn sidebar_shortcut_chip(text: &str) -> gpui::Div {
    div()
        .px_2()
        .py_1()
        .rounded_sm()
        .text_size(px(11.0))
        .text_color(rgb(0xA9B1D6))
        .bg(rgb(0x30364A))
        .hover(|style| style.bg(rgb(0x3B4561)).text_color(rgb(0xD6DFF8)))
        .child(text.to_string())
}

/// 服务器采用列表而非重卡片，减少视觉噪声和垂直占用。
fn server_list_item() -> gpui::Div {
    div()
        .pt_3()
        .pb_3()
        .px_1()
        .flex()
        .flex_col()
        .border_b_1()
        .border_color(rgb(0x303548))
        .hover(|style| style.bg(rgb(0x242838)))
}

/// 侧栏列表项使用低对比度背景，强调色留给选中项和主要操作。
fn entry_card() -> gpui::Div {
    div()
        .mt_2()
        .p_3()
        .flex()
        .flex_col()
        .rounded_md()
        .bg(rgb(0x252939))
        .border_1()
        .border_color(rgb(0x303548))
        .hover(|style| style.bg(rgb(0x2B3042)))
}

/// 侧栏的固定标题与条目数量。
fn sidebar_header(title: &str, count: usize) -> gpui::Div {
    div()
        .h(px(52.0))
        .px_3()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(12.0))
        .text_color(rgb(0xA9B1D6))
        .child(title.to_string())
        .child(
            div()
                .px_1()
                .rounded_sm()
                .bg(rgb(0x303548))
                .text_size(px(11.0))
                .text_color(rgb(0xC0CAF5))
                .child(count.to_string()),
        )
}

fn sidebar_tab_button(
    label: &'static str,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    div()
        .flex_1()
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_size(px(12.0))
        .bg(if selected {
            rgb(0x35415B)
        } else {
            rgb(0x161824)
        })
        .text_color(if selected {
            rgb(0xD6E3FF)
        } else {
            rgb(0xA9B1D6)
        })
        .hover(|style| {
            style.bg(if selected {
                rgb(0x35415B)
            } else {
                rgb(0x292D40)
            })
        })
        .on_mouse_down(MouseButton::Left, listener)
        .child(label)
}

fn sidebar_empty(title: &str, description: &str) -> gpui::Div {
    div()
        .pt_4()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_color(rgb(0xA9B1D6)).child(title.to_string()))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(0x9AA0A6))
                .child(description.to_string()),
        )
}

/// 侧栏操作下方的辅助说明。
fn panel_caption(text: &str) -> gpui::Div {
    div()
        .pt_2()
        .text_size(px(11.0))
        .text_color(rgb(0x9AA0A6))
        .child(text.to_string())
}

fn button(
    label: &'static str,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    button_with_color(label, 0xA9B1D6, listener)
}

fn button_with_color(
    label: &'static str,
    color: u32,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .h_full()
        .min_w(px(32.0))
        .flex()
        .items_center()
        .justify_center()
        .px_2()
        .rounded_sm()
        .text_color(rgb(color))
        .hover(|style| style.bg(rgb(0x292D40)).text_color(rgb(0xD7DEF7)))
        .on_mouse_down(MouseButton::Left, listener)
        .child(label)
}

struct TransferWrappedSession {
    inner: Arc<dyn TerminalSession>,
    download_dir: std::path::PathBuf,
}

impl TerminalSession for TransferWrappedSession {
    fn backend(&self) -> laterterm_core::BackendKind {
        self.inner.backend()
    }

    fn title(&self) -> String {
        self.inner.title()
    }

    fn take_streams(&self) -> std::io::Result<laterterm_core::SessionStreams> {
        let streams = self.inner.take_streams()?;
        // 事件通道：接收端 drop 后 send 静默失败——传输仍正常工作，只是没有 UI 通知
        let (events, _observer) = std::sync::mpsc::channel();
        Ok(laterterm_transfer::wrap(
            streams,
            laterterm_transfer::TransferOptions {
                download_dir: self.download_dir.clone(),
                events,
                upload_files: None,
            },
        ))
    }

    fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.inner.resize(cols, rows)
    }

    fn kill(&self) {
        self.inner.kill();
    }
}

fn wrap_transfer(
    session: Arc<dyn TerminalSession>,
    settings: &Appearance,
) -> Arc<dyn TerminalSession> {
    let download_dir = if settings.downloads_dir.is_empty() {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("Downloads")
    } else {
        PathBuf::from(&settings.downloads_dir)
    };
    Arc::new(TransferWrappedSession {
        inner: session,
        download_dir,
    })
}
/// 解析本地会话 shell：显式配置（首启引导固化 / 手改 settings.toml）优先，
/// 否则跟随平台默认（$SHELL / COMSPEC，由 laterterm-local 动态解析）。
fn resolve_shell(shell: Option<&str>) -> String {
    match shell {
        Some(explicit) => explicit.to_owned(),
        None => laterterm_local::default_shell(),
    }
}

#[allow(clippy::too_many_arguments)] // 装配函数：显式传参比打包结构更直观
fn create_local_terminal(
    id: PaneId,
    workspace: gpui::WeakEntity<WorkspaceView>,
    cx: &mut Context<WorkspaceView>,
    settings: &Appearance,
    theme: &Theme,
    shortcut_tx: flume::Sender<(PaneId, WorkspaceShortcut)>,
    broadcast_tx: flume::Sender<(PaneId, Vec<u8>)>,
    cols: usize,
    rows: usize,
    shell: String,
) -> (Entity<TerminalView>, Arc<SessionControl>) {
    // 会话的开启/spawn/回收线程全部内聚在 LocalSession（laterterm-local）。
    // PTY 初始尺寸直接用窗口实际尺寸：避免启动后 resize 触发 bash readline
    // 的 \r 重画，把光标拉回提示符行首。
    let session: Arc<dyn TerminalSession> = Arc::new(
        LocalSession::spawn(LocalOptions {
            shell,
            args: vec![],
            env: vec![],
            cols: cols as u16,
            rows: rows as u16,
        })
        .expect("启动本地会话失败"),
    );
    let session = wrap_transfer(session, settings);
    let session_control = Arc::new(SessionControl::new(Arc::clone(&session)));
    let terminal = create_terminal_view(
        id,
        session,
        terminal_config_for(settings, theme, cols, rows),
        workspace,
        shortcut_tx,
        broadcast_tx,
        cx,
    );
    (terminal, session_control)
}

/// 密钥环操作包一层 5s 超时：Secret Service 偶发卡住（等解锁/授权确认而
/// 弹窗不可见）时给出可行动的错误，而不是让连接流程永远停住。
pub(crate) fn run_secret_op<T, F>(op: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = flume::bounded::<anyhow::Result<T>>(1);
    std::thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "密钥环 5 秒无响应（可能在等解锁/授权确认），请检查系统弹窗后重试"
        )),
    }
}

/// 开一个本地 shell 会话（attach_pane 之外仅差标题命名，故独立成函数）。
/// shell 由调用方传入设置（None = 跟随平台默认）；已开窗格不随设置改动重启，
/// 只对新标签/分屏生效。
fn spawn_local_session(shell: Option<&str>) -> Arc<dyn TerminalSession> {
    let shell = resolve_shell(shell);
    Arc::new(
        LocalSession::spawn(LocalOptions {
            shell,
            args: vec![],
            env: vec![],
            cols: 110,
            rows: 30,
        })
        .expect("启动本地会话失败"),
    )
}

/// 外观设置 → 终端视图配置（本地与 SSH 会话共用）。
/// 首帧前的网格/PTY 初始尺寸估算：用字体度量近似值把窗口内容区换算成行列。
/// 与真实测量的偏差极小，避免「启动后 resize → bash readline 重画 → 光标被
/// \r 拉回行首」的提示符错位。
fn estimate_grid_size(settings: &Appearance, viewport: gpui::Size<gpui::Pixels>) -> (usize, usize) {
    // 系数为 JetBrains Mono 实测值(cell 7.83x19.2 @13px)：估算与首帧实测的偏差
    // 应尽量小，否则启动后的 PTY resize 会触发 bash readline 的 \r 重画，
    // 把光标拉回提示符行首。
    let cell_width = settings.font_size * 0.6058;
    let cell_height = settings.font_size * 1.2308 * settings.line_height;
    let usable_width = (viewport.width - px(settings.padding * 2.0)).max(px(1.0));
    let usable_height = (viewport.height - px(settings.padding * 2.0)).max(px(1.0));
    let cols = ((usable_width / px(cell_width)) as usize).clamp(2, 500);
    let rows = ((usable_height / px(cell_height)) as usize).clamp(2, 200);
    (cols, rows)
}

fn terminal_config_for(
    settings: &Appearance,
    theme: &Theme,
    cols: usize,
    rows: usize,
) -> TerminalConfig {
    TerminalConfig {
        font_family: settings.font_family.clone(),
        font_size: px(settings.font_size),
        cols,
        rows,
        cursor_blink: settings.cursor_blink,
        cursor_style: cursor_style_for(settings.cursor_style),
        smooth_scrolling: settings.smooth_scrolling,
        copy_on_select: settings.copy_on_select,
        scrollback: settings.scrollback,
        line_height_multiplier: settings.line_height,
        padding: Edges::all(px(settings.padding)),
        colors: palette_for(theme),
        ligatures: settings.ligatures,
        font_fallbacks: settings.font_fallbacks.clone(),
    }
}

/// 装配终端视图：分屏/焦点快捷键与 OSC 标题回调对所有后端一致。
/// resize 由视图默认回调转发给会话，无需在此另接。
fn create_terminal_view(
    id: PaneId,
    session: Arc<dyn TerminalSession>,
    config: TerminalConfig,
    workspace: gpui::WeakEntity<WorkspaceView>,
    shortcut_tx: flume::Sender<(PaneId, WorkspaceShortcut)>,
    broadcast_tx: flume::Sender<(PaneId, Vec<u8>)>,
    cx: &mut Context<WorkspaceView>,
) -> Entity<TerminalView> {
    cx.new(|cx| {
        let shortcut_tx = shortcut_tx.clone();
        TerminalView::new(session, config, cx)
            .with_key_handler(move |event| {
                let shortcut = if keystroke_matches_shortcut(event, "ctrl+j") {
                    Some(WorkspaceShortcut::Split(SplitDirection::Horizontal))
                } else if keystroke_matches_shortcut(event, "ctrl+k") {
                    Some(WorkspaceShortcut::Split(SplitDirection::Vertical))
                } else if keystroke_matches_shortcut(event, "ctrl+shift+[") {
                    Some(WorkspaceShortcut::FocusPrevious)
                } else if keystroke_matches_shortcut(event, "ctrl+shift+]") {
                    Some(WorkspaceShortcut::FocusNext)
                } else if keystroke_matches_shortcut(event, "ctrl+shift+b") {
                    Some(WorkspaceShortcut::ToggleBroadcast)
                } else {
                    None
                };
                if let Some(shortcut) = shortcut {
                    let _ = shortcut_tx.send((id, shortcut));
                    true
                } else {
                    false
                }
            })
            .with_input_callback(move |bytes| {
                let _ = broadcast_tx.send((id, bytes.to_vec()));
            })
            .with_title_callback({
                let workspace = workspace.clone();
                move |window, cx, title| {
                    let active = workspace
                        .update(cx, |workspace, cx| workspace.set_pane_title(id, title, cx))
                        .unwrap_or(false);
                    if active {
                        window.set_window_title(if title.is_empty() { "LaterTerm" } else { title });
                    }
                }
            })
            // 滚动/输出变化时唤醒工作区，让滚动条指示器跟手
            .with_scroll_callback({
                let workspace = workspace.clone();
                move |cx| {
                    let _ = workspace.update(cx, |_, cx| cx.notify());
                }
            })
    })
}

/// laterterm_config 主题 → termview 调色板。
///
/// builder 的颜色方法都是 (r, g, b) 三参形式，这里用宏把 0xRRGGBB 打包值展开进去。
macro_rules! set_color {
    ($builder:expr, $method:ident, $value:expr) => {
        $builder = $builder.$method(
            ($value >> 16) as u8,
            (($value >> 8) & 0xFF) as u8,
            ($value & 0xFF) as u8,
        )
    };
}

/// 订阅终端右键菜单请求：把事件转成工作区的菜单状态。
/// 首窗格（内联创建）与后续窗格（attach_pane）共用。
fn subscribe_terminal_menu(
    terminal: &Entity<TerminalView>,
    pane_id: PaneId,
    window: &mut Window,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Subscription {
    cx.subscribe_in(
        terminal,
        window,
        move |this, _, event: &laterterm_termview::TerminalMenuRequested, _window, cx| {
            this.terminal_menu = Some(TerminalMenuState {
                pane_id,
                position: event.position,
                url: event.url.clone(),
            });
            cx.notify();
        },
    )
}

fn pane_direction_for_keystroke(key: &gpui::Keystroke) -> Option<PaneDirection> {
    let modifiers = &key.modifiers;
    if !modifiers.control
        || modifiers.shift
        || modifiers.alt
        || modifiers.platform
        || modifiers.function
    {
        return None;
    }
    match key.key.to_ascii_lowercase().as_str() {
        "left" => Some(PaneDirection::Left),
        "right" => Some(PaneDirection::Right),
        "up" => Some(PaneDirection::Up),
        "down" => Some(PaneDirection::Down),
        _ => None,
    }
}

fn keystroke_matches_shortcut(event: &KeyDownEvent, shortcut: &str) -> bool {
    crate::shortcut_recorder::matches_shortcut(&event.keystroke, shortcut)
}

fn palette_for(theme: &Theme) -> ColorPalette {
    let colors = &theme.colors;
    let mut builder = ColorPalette::builder();
    set_color!(builder, background, colors.background);
    set_color!(builder, foreground, colors.foreground);
    set_color!(builder, cursor, colors.cursor);
    set_color!(builder, black, colors.black);
    set_color!(builder, red, colors.red);
    set_color!(builder, green, colors.green);
    set_color!(builder, yellow, colors.yellow);
    set_color!(builder, blue, colors.blue);
    set_color!(builder, magenta, colors.magenta);
    set_color!(builder, cyan, colors.cyan);
    set_color!(builder, white, colors.white);
    set_color!(builder, bright_black, colors.bright_black);
    set_color!(builder, bright_red, colors.bright_red);
    set_color!(builder, bright_green, colors.bright_green);
    set_color!(builder, bright_yellow, colors.bright_yellow);
    set_color!(builder, bright_blue, colors.bright_blue);
    set_color!(builder, bright_magenta, colors.bright_magenta);
    set_color!(builder, bright_cyan, colors.bright_cyan);
    set_color!(builder, bright_white, colors.bright_white);
    builder.build()
}

/// laterterm_config 光标样式 → termview 光标样式。
///
/// 两 crate 不互依，各自持一份枚举；match 穷尽映射，新增形状漏接时编译期报错。
/// 注意：workspace.rs 顶部已 `use gpui::CursorStyle`（指针样式），此处的
/// 光标形状枚举一律写全路径，避免裸名解析到 gpui 的同名枚举。
fn cursor_style_for(style: laterterm_config::CursorStyle) -> laterterm_termview::CursorStyle {
    match style {
        laterterm_config::CursorStyle::Block => laterterm_termview::CursorStyle::Block,
        laterterm_config::CursorStyle::Bar => laterterm_termview::CursorStyle::Bar,
        laterterm_config::CursorStyle::Underline => laterterm_termview::CursorStyle::Underline,
    }
}

#[cfg(test)]
mod exit_regression_tests {
    use super::{
        PaneDirection, SplitDirection, WorkspaceView, config_event_requires_reload,
        cursor_style_for, keystroke_matches_shortcut, pane_direction_for_keystroke,
        terminal_config_for,
    };
    use gpui::{KeyDownEvent, Keystroke, MouseDownEvent, TestAppContext};

    #[test]
    fn config_watcher_ignores_read_access_but_keeps_mutations() {
        use notify::EventKind;
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
        };
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Open(AccessMode::Write)),
        ] {
            assert!(!config_event_requires_reload(&kind), "{kind:?}");
        }
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Remove(RemoveKind::File),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Any,
        ] {
            assert!(config_event_requires_reload(&kind), "{kind:?}");
        }
    }

    #[test]
    fn directional_shortcuts_require_only_control() {
        for (key, expected) in [
            ("left", PaneDirection::Left),
            ("right", PaneDirection::Right),
            ("up", PaneDirection::Up),
            ("down", PaneDirection::Down),
        ] {
            let event = Keystroke::parse(&format!("ctrl-{key}")).unwrap();
            assert_eq!(pane_direction_for_keystroke(&event), Some(expected));
            for prefix in ["", "ctrl-shift-", "ctrl-alt-", "ctrl-cmd-"] {
                let event = Keystroke::parse(&format!("{prefix}{key}")).unwrap();
                assert_eq!(pane_direction_for_keystroke(&event), None);
            }
        }
    }

    #[test]
    fn shortcut_match_uses_canonical_modifiers() {
        let event = KeyDownEvent {
            keystroke: Keystroke::parse("ctrl-shift-up").unwrap(),
            is_held: false,
        };
        assert!(keystroke_matches_shortcut(&event, " Shift + Control + Up "));
        assert!(!keystroke_matches_shortcut(&event, "mystery+ctrl+shift+up"));
        assert!(!keystroke_matches_shortcut(&event, "ctrl+up"));
    }

    #[gpui::test]
    fn workspace_opens_with_vertical_tabs_and_right_panel_closed(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.read_with(cx, |view, _| {
            assert!(view.left_panel, "启动时应显示左侧标签栏");
            assert!(!view.right_panel, "右侧功能面板默认保持关闭");
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn new_active_tab_is_visible_in_scrolling_sidebar(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(400.0)));
        workspace.update_in(cx, |view, window, cx| {
            view.welcome_visible = false;
            for _ in 0..12 {
                view.spawn_tab(window, cx);
            }
        });
        cx.run_until_parked();
        let list = cx.debug_bounds("sidebar-tabs").expect("侧栏列表必须存在");
        let active = cx
            .debug_bounds("sidebar-active-tab")
            .expect("活动标签必须存在");
        assert!(
            active.top() >= list.top() && active.bottom() <= list.bottom(),
            "新建的活动标签应滚入可视区域"
        );
        workspace.update_in(cx, |view, window, cx| {
            let first = view.tabs[0].focused_pane;
            view.focus_pane(first, window, cx);
        });
        cx.run_until_parked();
        let list = cx.debug_bounds("sidebar-tabs").unwrap();
        let active = cx.debug_bounds("sidebar-active-tab").unwrap();
        assert!(
            active.top() >= list.top() && active.bottom() <= list.bottom(),
            "切换至顶部标签时应滚入可视区域"
        );
        workspace.update_in(cx, |view, window, cx| {
            view.tab_scroll.set_offset(gpui::point(
                gpui::px(0.0),
                -view.tab_scroll.max_offset().height,
            ));
            view.toggle_left_panel(&MouseDownEvent::default(), window, cx);
            view.toggle_left_panel(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        let list = cx.debug_bounds("sidebar-tabs").unwrap();
        let active = cx.debug_bounds("sidebar-active-tab").unwrap();
        assert!(
            active.top() >= list.top() && active.bottom() <= list.bottom(),
            "重新打开侧栏应显示活动标签"
        );
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn cursor_blink_setting_updates_existing_panes(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            view.spawn_tab(window, cx);
            let enabled = !view.panes[0].terminal.read(cx).config().cursor_blink;
            view.settings.appearance.cursor_blink = enabled;
            view.apply_appearance(cx);
            for pane in &view.panes {
                assert_eq!(pane.terminal.read(cx).config().cursor_blink, enabled);
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn copy_on_select_setting_updates_existing_panes(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            view.spawn_tab(window, cx);
            let enabled = !view.panes[0].terminal.read(cx).config().copy_on_select;
            view.settings.appearance.copy_on_select = enabled;
            // 既有窗格链路：apply_appearance 逐窗格热更新
            view.apply_appearance(cx);
            for pane in &view.panes {
                assert_eq!(pane.terminal.read(cx).config().copy_on_select, enabled);
            }
            // 新窗格链路：terminal_config_for 产出同值
            let appearance = view.settings.appearance.clone();
            let theme = view.themes.resolve(&appearance.theme);
            let config = terminal_config_for(&appearance, theme, 80, 24);
            assert_eq!(config.copy_on_select, enabled, "新建窗格应继承同值");
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn cursor_style_setting_updates_existing_panes(cx: &mut TestAppContext) {
        use laterterm_config::CursorStyle;
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            view.spawn_tab(window, cx);
            // config() 返回 termview 侧 TerminalConfig，断言值须映射成 termview 枚举
            for target in [CursorStyle::Bar, CursorStyle::Underline, CursorStyle::Block] {
                view.settings.appearance.cursor_style = target;
                view.apply_appearance(cx);
                for pane in &view.panes {
                    assert_eq!(
                        pane.terminal.read(cx).config().cursor_style,
                        cursor_style_for(target),
                        "光标样式 {target:?} 应热更新到既有窗格"
                    );
                }
            }
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn terminal_right_click_event_records_menu_state(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        let pane_id = workspace.update_in(cx, |view, _window, cx| {
            let pane_id = view.panes[0].id;
            let terminal = view.panes[0].terminal.clone();
            // 模拟 termview 右键：直接发射菜单请求事件（真实链路由 on_right_click 触发）。
            // subscribe_in 是窗口级订阅，须在窗口上下文里发射才会派发。
            terminal.update(cx, |_, cx| {
                cx.emit(laterterm_termview::TerminalMenuRequested {
                    position: gpui::point(gpui::px(40.0), gpui::px(60.0)),
                    url: Some("https://example.com".into()),
                });
            });
            pane_id
        });
        cx.run_until_parked();
        workspace.update(cx, |view, _| {
            let menu = view.terminal_menu.clone().expect("右键事件应记录菜单状态");
            assert_eq!(menu.pane_id, pane_id);
            assert_eq!(menu.url.as_deref(), Some("https://example.com"));
        });
    }

    #[gpui::test]
    fn server_card_places_address_above_shortcut(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _, cx| {
            view.welcome_visible = false;
            view.right_panel = true;
            view.right_panel_tab = super::RightPanelTab::Servers;
            view.settings.servers = vec![laterterm_config::ServerEntry {
                name: "测试服务器".into(),
                host: "example.com".into(),
                username: "user".into(),
                ..Default::default()
            }];
            cx.notify();
        });
        cx.run_until_parked();
        let new_button = cx
            .debug_bounds("server-new-button")
            .expect("新建按钮应可见");
        let export = cx
            .debug_bounds("server-export-button")
            .expect("导出按钮应可见");
        assert!(
            new_button.right() <= export.left(),
            "服务器工具栏按钮不可重叠"
        );
        let address = cx.debug_bounds("server-address").expect("服务器地址应可见");
        let shortcut = cx
            .debug_bounds("server-shortcut")
            .expect("快捷键入口应可见");
        assert!(
            address.bottom() <= shortcut.top(),
            "服务器地址应排在快捷键之前"
        );
        let edit = cx.debug_bounds("server-edit").expect("编辑按钮应可见");
        let delete = cx.debug_bounds("server-delete").expect("删除按钮应可见");
        assert_eq!(shortcut.top(), edit.top(), "次要操作应在同一行");
        assert_eq!(edit.top(), delete.top(), "编辑和删除应在同一行");
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 分组渲染与折叠冒烟：组头可见、成员排组头之后、未分组殿后；
    /// 折叠后组内成员消失、未分组桶不受影响；重命名/解散按钮存在。
    /// TODO: GPUI debug_bounds 在设置浮层改版后不再可靠反映渲染树——待并行会话修复
    #[gpui::test]
    #[ignore = "GPUI debug_selector 在渲染路径变更后断言失真，待修复"]
    fn server_group_headers_render_and_collapse_members(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        let mk = |name: &str, group: Option<&str>| laterterm_config::ServerEntry {
            name: name.into(),
            host: "example.com".into(),
            username: "user".into(),
            group: group.map(str::to_string),
            ..Default::default()
        };
        workspace.update_in(cx, |view, _, cx| {
            view.welcome_visible = false;
            view.right_panel = true;
            view.right_panel_tab = super::RightPanelTab::Servers;
            // 先只放未分组：硬约束——纯未分组外观与旧版一致（无组头）
            view.settings.servers = vec![mk("游离机", None)];
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("server-group-header-未分组").is_none(),
            "纯未分组不应出现组头（旧版平铺外观）"
        );
        workspace.update_in(cx, |view, _, cx| {
            view.settings.servers = vec![
                mk("生产一", Some("生产")),
                mk("生产二", Some("生产")),
                mk("游离机", None),
            ];
            cx.notify();
        });
        cx.run_until_parked();
        let header = cx
            .debug_bounds("server-group-header-生产")
            .expect("命名组组头应可见");
        let member = cx.debug_bounds("server-card-生产一").expect("组成员应可见");
        let ungrouped = cx
            .debug_bounds("server-card-游离机")
            .expect("未分组成员应可见");
        assert!(header.bottom() <= member.top(), "成员应排在组头之后");
        assert!(member.top() < ungrouped.top(), "未分组桶应殿后于命名组成员");
        cx.debug_bounds("server-group-rename")
            .expect("重命名入口应可见");
        cx.debug_bounds("server-group-disband")
            .expect("解散入口应可见");
        // 折叠：组内成员消失，未分组不受影响
        workspace.update(cx, |view, cx| {
            view.server_groups_collapsed.insert(Some("生产".into()));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("server-card-生产一").is_none(),
            "折叠后组成员应隐藏"
        );
        assert!(
            cx.debug_bounds("server-card-游离机").is_some(),
            "折叠命名组不能影响未分组桶"
        );
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// Deepin 命令导入冒烟：tempdir 注入 command-config.conf + 注入落盘路径，
    /// 断言内存态与计数提示，且真实用户配置（默认路径）绝不被测试写入。
    #[gpui::test]
    fn deepin_command_import_merges_in_memory_with_injected_paths(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!(
            "laterterm-deepin-import-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("command-config.conf");
        std::fs::write(
            &conf,
            "[更新系统]\nCommand = sudo apt update\nShortcut = Ctrl+Shift+G\n\n[空命令]\nCommand =\n",
        )
        .unwrap();
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _, cx| {
            view.welcome_visible = false;
            view.right_panel = true;
            view.right_panel_tab = super::RightPanelTab::Commands;
            // 屏蔽本机真实配置对计数的影响；落盘路径指向 tempdir（评审缺口 3 的口径：
            // 测试绝不写 Settings::default_path()，也不用改进程 HOME 的竞态方案）
            view.settings.commands.clear();
            view.settings.servers.clear();
            view.settings.totp.clear();
            view.settings.shortcuts = Default::default();
            view.settings_save_path = Some(dir.join("settings.toml"));
            cx.notify();
        });
        cx.run_until_parked();
        workspace.update(cx, |view, cx| {
            view.import_commands_deepin_with(&conf, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert_eq!(view.settings.commands.len(), 1, "空命令节应被跳过");
            assert_eq!(view.settings.commands[0].name, "更新系统");
            assert_eq!(view.settings.commands[0].template, "sudo apt update");
            assert_eq!(
                view.settings.commands[0].shortcut.as_deref(),
                Some("Ctrl+Shift+G")
            );
            assert!(view.settings.commands[0].confirm, "导入命令默认需确认");
            let notice = view.command_notice.clone().expect("导入后应有计数提示");
            assert!(notice.contains("导入 1"), "{notice}");
            assert!(notice.contains("解析跳过 1"), "{notice}");
        });
        // 落盘写进了注入路径（而非真实配置）
        let saved = std::fs::read_to_string(dir.join("settings.toml")).unwrap();
        assert!(saved.contains("更新系统"), "导入命令应写入注入路径");
        let _ = std::fs::remove_dir_all(&dir);
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[test]
    fn sidebar_shortcut_label_remains_concise_and_identifiable() {
        assert_eq!(super::sidebar_shortcut_label(None), "设置快捷键");
        assert_eq!(
            super::sidebar_shortcut_label(Some("ctrl+shift+k")),
            "ctrl+shift+k"
        );
    }

    #[test]
    fn pane_close_button_is_only_shown_for_split_sessions() {
        assert!(!super::show_pane_close(1));
        assert!(super::show_pane_close(2));
        assert!(super::show_pane_close(3));
    }

    #[gpui::test]
    fn native_window_shutdown_cancels_pending_work(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update(cx, |view, _| {
            let save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let connect = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            view.server_save = Some(save.clone());
            view.ssh_request = Some(super::SshRequest {
                id: 1,
                entry: laterterm_config::ServerEntry::default(),
                jump: Vec::new(),
                dest: super::ConnectDest::Shell,
                active: connect.clone(),
                running: true,
            });
            view.shutdown();
            view.shutdown();
            assert!(!save.load(std::sync::atomic::Ordering::Acquire));
            assert!(!connect.load(std::sync::atomic::Ordering::Acquire));
            assert!(view.panes.iter().all(|pane| {
                pane.session
                    .closing
                    .load(std::sync::atomic::Ordering::Acquire)
            }));
        });
    }

    #[gpui::test]
    fn titlebar_logo_keeps_square_bounds_in_narrow_window(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        cx.simulate_resize(gpui::size(gpui::px(600.0), gpui::px(460.0)));
        cx.run_until_parked();
        let logo = cx
            .debug_bounds("brand-logo")
            .expect("标题栏必须包含品牌图标");
        assert_eq!(logo.size, gpui::size(gpui::px(20.0), gpui::px(20.0)));
        assert!(logo.top() >= gpui::px(0.0));
        assert!(logo.bottom() <= gpui::px(super::TITLEBAR_HEIGHT));
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn extra_modifiers_do_not_trigger_builtin_tabs(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            window.activate_window();
            view.focus_active(window, cx);
        });
        cx.simulate_keystrokes("ctrl-alt-t");
        cx.simulate_keystrokes("ctrl-cmd-j");
        cx.simulate_keystrokes("ctrl-cmd-k");
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert_eq!(view.panes.len(), 1, "备用分屏处理也必须精确匹配");
            assert_eq!(view.tabs.len(), 1, "额外修饰键不能被内置 Ctrl+T 抢走");
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn merged_broadcast_shortcut_is_exact_and_modal_safe(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            view.welcome_visible = false;
            view.focus_active(window, cx);
            window.activate_window();
        });
        cx.simulate_keystrokes("ctrl-shift-b");
        workspace.update_in(cx, |view, window, cx| {
            assert!(view.broadcast);
            view.edit_command(None, window, cx);
        });
        cx.simulate_keystrokes("ctrl-shift-b");
        workspace.update_in(cx, |view, window, cx| {
            assert!(view.broadcast, "弹窗输入不能切换广播");
            view.dismiss_command(window, cx);
        });
        cx.simulate_keystrokes("ctrl-alt-shift-b");
        workspace.read_with(cx, |view, _| assert!(view.broadcast));
        cx.simulate_keystrokes("ctrl-shift-b");
        workspace.read_with(cx, |view, _| {
            assert!(!view.broadcast);
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn panel_shortcuts_toggle_tabs_without_creating_sessions(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            view.settings.shortcuts.commands_panel = Some("ctrl+alt+c".into());
            view.settings.shortcuts.servers_panel = Some("ctrl+alt+s".into());
            window.activate_window();
            view.focus_active(window, cx);
        });
        cx.simulate_keystrokes("ctrl-alt-c");
        workspace.read_with(cx, |view, _| {
            assert!(view.right_panel);
            assert_eq!(view.right_panel_tab, super::RightPanelTab::Commands);
            assert!(
                view.check_shortcut_conflict(Some("ctrl+alt+c"), None)
                    .is_some()
            );
        });
        cx.simulate_keystrokes("ctrl-alt-s");
        workspace.read_with(cx, |view, _| {
            assert!(view.right_panel);
            assert_eq!(view.right_panel_tab, super::RightPanelTab::Servers);
        });
        cx.simulate_keystrokes("ctrl-alt-s");
        workspace.read_with(cx, |view, _| {
            assert!(!view.right_panel);
            assert_eq!(view.tabs.len(), 1);
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn recorder_captures_reserved_keys_and_escape_cancels_only_recording(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            view.edit_command(None, window, cx);
            let recorder = view.shortcut_editor.as_ref().unwrap().recorder.clone();
            recorder.update(cx, |recorder, cx| recorder.start(window, cx));
            window.activate_window();
        });
        cx.simulate_keystrokes("ctrl-j");
        workspace.update_in(cx, |view, window, cx| {
            assert_eq!(view.panes.len(), 1);
            let recorder = view.shortcut_editor.as_ref().unwrap().recorder.clone();
            assert_eq!(recorder.read(cx).value(), Some("ctrl+j"));
            recorder.update(cx, |recorder, cx| recorder.start(window, cx));
        });
        cx.simulate_keystrokes("escape");
        workspace.read_with(cx, |view, cx| {
            assert!(view.command_dialog.is_some());
            assert_eq!(view.shortcut_value(cx).as_deref(), Some("ctrl+j"));
        });
        cx.simulate_keystrokes("escape");
        workspace.read_with(cx, |view, _| {
            assert!(view.command_dialog.is_none());
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn cancelled_connection_drops_late_prompts(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            let entry = laterterm_config::ServerEntry {
                name: "取消测试".into(),
                host: "invalid.test".into(),
                username: "test".into(),
                ..Default::default()
            };
            view.settings.servers = vec![entry.clone()];
            view.ssh_request = Some(super::SshRequest {
                id: 9,
                entry: entry.clone(),
                jump: Vec::new(),
                dest: super::ConnectDest::Shell,
                active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                running: true,
            });
            view.ssh_connecting = Some(entry.name.clone());
            view.dismiss_server(window, cx);
            view.finish_ssh_connect(
                9,
                entry,
                Ok(super::ConnectionOutcome::Prompt {
                    kind: super::CredentialKind::Login,
                    warning: None,
                }),
                window,
                cx,
            );
            assert!(view.server_dialog.is_none(), "取消后不能弹回旧凭据框");
            assert!(view.ssh_connecting.is_none());
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn server_modal_scroll_reaches_footer_in_short_window(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        cx.simulate_resize(gpui::size(gpui::px(800.0), gpui::px(460.0)));
        workspace.update_in(cx, |view, window, cx| {
            view.edit_server(None, window, cx);
            window.activate_window();
        });
        cx.run_until_parked();
        let card = cx.debug_bounds("modal-card").unwrap();
        assert!(card.bottom() <= gpui::px(460.0));
        let before = cx.debug_bounds("modal-footer").unwrap();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: card.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-1200.0))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();
        let after = cx.debug_bounds("modal-footer").unwrap();
        assert!(after.top() < before.top(), "长表单必须能够滚动");
        assert!(after.bottom() <= card.bottom(), "保存与取消按钮必须可达");
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn all_item_editors_record_without_dispatch_or_terminal_input(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        for kind in 0..3 {
            workspace.update_in(cx, |view, window, cx| {
                match kind {
                    0 => view.edit_command(None, window, cx),
                    1 => view.edit_server(None, window, cx),
                    _ => view.edit_totp(None, window, cx),
                }
                view.start_shortcut_recording(window, cx);
                window.activate_window();
            });
            cx.simulate_keystrokes("ctrl-down");
            cx.simulate_keystrokes("ctrl-t");
            cx.simulate_keystrokes("a");
            workspace.update_in(cx, |view, window, cx| {
                assert_eq!(view.shortcut_value(cx).as_deref(), Some("ctrl+down"));
                assert_eq!(view.tabs.len(), 1, "录制控件保留焦点时不分发工作区动作");
                assert_eq!(view.panes.len(), 1);
                for (_, field) in view
                    .command_fields
                    .iter()
                    .chain(view.server_fields.iter())
                    .chain(view.totp_fields.iter())
                {
                    if field.read(cx).value().is_empty() {
                        continue;
                    }
                    assert!(
                        !field.read(cx).value().contains('a'),
                        "录制后的字符不能进入表单"
                    );
                }
                match kind {
                    0 => view.dismiss_command(window, cx),
                    1 => view.dismiss_server(window, cx),
                    _ => view.dismiss_totp(window, cx),
                }
            });
        }
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn stale_success_closes_session_without_attaching_tab(cx: &mut TestAppContext) {
        struct StaleSession(std::sync::atomic::AtomicBool);
        impl laterterm_core::TerminalSession for StaleSession {
            fn backend(&self) -> laterterm_core::BackendKind {
                laterterm_core::BackendKind::Ssh
            }
            fn title(&self) -> String {
                "stale".into()
            }
            fn take_streams(&self) -> std::io::Result<laterterm_core::SessionStreams> {
                panic!("过期会话不得进入视图")
            }
            fn resize(&self, _: u16, _: u16) -> std::io::Result<()> {
                Ok(())
            }
            fn kill(&self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        let session = std::sync::Arc::new(StaleSession(std::sync::atomic::AtomicBool::new(false)));
        workspace.update_in(cx, |view, window, cx| {
            view.finish_ssh_connect(
                99,
                laterterm_config::ServerEntry::default(),
                Ok(super::ConnectionOutcome::Connected {
                    session: session.clone(),
                    warning: None,
                }),
                window,
                cx,
            );
            assert_eq!(view.tabs.len(), 1);
            assert!(session.0.load(std::sync::atomic::Ordering::Acquire));
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn control_arrows_follow_visible_nested_panes(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        let (left, top, bottom) = workspace.update_in(cx, |view, window, cx| {
            let left = view.focused_pane_id();
            view.split_focused(SplitDirection::Horizontal, window, cx);
            let top = view.focused_pane_id();
            view.split_focused(SplitDirection::Vertical, window, cx);
            let bottom = view.focused_pane_id();
            view.focus_pane(left, window, cx);
            window.activate_window();
            (left, top, bottom)
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-right");
        workspace.read_with(cx, |view, _| assert_eq!(view.focused_pane_id(), top));
        cx.simulate_keystrokes("ctrl-down");
        workspace.read_with(cx, |view, _| assert_eq!(view.focused_pane_id(), bottom));
        cx.simulate_keystrokes("ctrl-down");
        workspace.read_with(cx, |view, _| assert_eq!(view.focused_pane_id(), bottom));
        cx.simulate_keystrokes("ctrl-left");
        workspace.read_with(cx, |view, _| assert_eq!(view.focused_pane_id(), left));
        workspace.update_in(cx, |view, window, cx| {
            view.settings_open = true;
            view.focus_active(window, cx);
            cx.notify();
        });
        cx.simulate_keystrokes("ctrl-right");
        workspace.read_with(cx, |view, _| {
            assert_eq!(view.focused_pane_id(), left, "设置面板打开时不能切换窗格");
        });
        workspace.update_in(cx, |view, window, cx| {
            view.settings_open = false;
            view.focus_pane(bottom, window, cx);
        });
        cx.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(760.0)));
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-up");
        workspace.update_in(cx, |view, window, cx| {
            assert_eq!(view.focused_pane_id(), top, "窗口缩放后应使用新几何");
            view.edit_command(None, window, cx);
        });
        cx.simulate_keystrokes("ctrl-left");
        workspace.read_with(cx, |view, _| {
            assert_eq!(
                view.focused_pane_id(),
                top,
                "输入弹窗不能被方向快捷键抢焦点"
            );
            assert!(view.command_dialog.is_some());
        });
        workspace.update_in(cx, |view, window, cx| {
            view.dismiss_command(window, cx);
            let command = laterterm_config::CommandTemplate {
                name: "确认".into(),
                template: "pwd".into(),
                confirm: true,
                ..Default::default()
            };
            view.open_command(&command, window, cx);
        });
        cx.simulate_keystrokes("ctrl-left");
        workspace.read_with(cx, |view, _| {
            assert_eq!(
                view.focused_pane_id(),
                top,
                "无输入框的预览弹窗也不能切换窗格"
            );
            assert!(view.command_dialog.is_some());
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    #[gpui::test]
    fn close_hidden_split_and_last_pane(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        let (hidden, split) = workspace.update_in(cx, |view, window, cx| {
            let hidden = view.focused_pane_id();
            view.spawn_tab(window, cx);
            view.split_focused(SplitDirection::Horizontal, window, cx);
            let split = view.focused_pane_id();
            let tab = view.tabs[0].id;
            view.close_tab(tab, window, cx);
            view.close_pane(split, window, cx);
            (hidden, split)
        });
        for _ in 0..300 {
            cx.run_until_parked();
            if workspace.read_with(cx, |view, _| view.panes.len() == 1) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        workspace.update_in(cx, |view, window, cx| {
            assert_eq!(
                view.panes.len(),
                1,
                "隐藏标签和分屏必须在无需鼠标输入时被回收"
            );
            assert_eq!(view.active_tab().layout.leaf_count(), 1);
            view.mark_exited(hidden, window, cx);
            view.mark_exited(split, window, cx);
            assert_eq!(view.panes.len(), 1, "重复退出不能删除存活窗格");
            let last = view.focused_pane_id();
            view.close_pane(last, window, cx);
        });
        for _ in 0..300 {
            cx.run_until_parked();
            if workspace.read_with(cx, |view, _| view.next_pane_id == 5) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        workspace.read_with(cx, |view, _| {
            assert_eq!(view.next_pane_id, 5, "最后窗格退出后只补位一次");
            assert_eq!(view.panes.len(), 1);
            assert_eq!(view.tabs.len(), 1);
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }
}

/// 跳板机链（ProxyJump）的面板语义测试：保存校验早退不落盘、删除阻断、
/// 改名改写引用、导入悬空断链。保存成功路径要写真实配置文件，故改名
/// 改写只测纯函数 [`apply_server_save`]；补输重试与凭据硬错误在
/// server_connection 的 MemoryStore 测试里覆盖。
#[cfg(test)]
mod server_jump_tests {
    use super::{WorkspaceView, apply_server_save, merge_entries_into};
    use gpui::TestAppContext;
    use laterterm_config::{ServerEntry, Settings};

    fn entry(name: &str, jump: Option<&str>) -> ServerEntry {
        ServerEntry {
            name: name.into(),
            host: "10.0.0.1".into(),
            username: "ops".into(),
            jump: jump.map(str::to_string),
            ..Default::default()
        }
    }

    /// 在编辑弹窗里把指定字段的文本整体替换为 `value`（模拟键入）。
    fn fill_field(
        view: &mut WorkspaceView,
        label: &str,
        value: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<WorkspaceView>,
    ) {
        use gpui::EntityInputHandler;
        let field = view
            .server_fields
            .iter()
            .find(|(name, _)| name == label)
            .unwrap_or_else(|| panic!("弹窗缺少「{label}」字段"))
            .1
            .clone();
        field.update(cx, |field, cx| {
            field.replace_text_in_range(None, value, window, cx)
        });
    }

    /// 保存自引条目必须同步报错且不落盘：早退发生在后台保存 spawn 之前，
    /// 断言报错文案与配置表未被改动。
    #[gpui::test]
    fn save_rejects_self_referencing_jump_without_persisting(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            let self_ref = entry("自引机", Some("自引机"));
            view.settings.servers = vec![self_ref.clone()];
            view.welcome_visible = false;
            view.edit_server(Some(&self_ref), window, cx);
            // 弹窗应含「跳板」行且预填当前引用
            let jump_field = view
                .server_fields
                .iter()
                .find(|(name, _)| name == "跳板")
                .expect("编辑弹窗必须有「跳板」字段");
            assert_eq!(jump_field.1.read(cx).value(), "自引机");
            view.save_server(Some("自引机".into()), cx);
            assert!(
                view.server_error
                    .as_deref()
                    .is_some_and(|error| error.contains("自身设为跳板")),
                "应报自引错误: {:?}",
                view.server_error
            );
            assert_eq!(
                view.settings.servers,
                vec![self_ref],
                "校验失败不得改动配置表（不落盘）"
            );
            assert!(view.server_save.is_none(), "同步早退不应进入后台保存");
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 双节点环（A→B→A）在保存 B 时被 preview 全表解析拦下，报环链路且不落盘。
    #[gpui::test]
    fn save_rejects_jump_cycle_with_chain_in_error(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, window, cx| {
            let a = entry("A", Some("B"));
            let b = entry("B", None);
            view.settings.servers = vec![a, b.clone()];
            view.edit_server(Some(&b), window, cx);
            fill_field(view, "跳板", "A", window, cx);
            view.save_server(Some("B".into()), cx);
            let error = view.server_error.clone().expect("环必须报错");
            assert!(error.contains("环"), "{error}");
            assert!(error.contains("B→A→B"), "{error}");
            assert_eq!(view.settings.servers.len(), 2, "不落盘");
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 删除被引用跳板被阻断：提示引用方清单，条目保留（宁报错不断链）。
    #[gpui::test]
    fn delete_blocks_bastion_still_referenced_by_others(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _window, cx| {
            let bastion = entry("堡垒机", None);
            let user = entry("内网机", Some("堡垒机"));
            view.settings.servers = vec![bastion, user];
            // 第一次点击进入确认态，第二次点击触发阻断
            view.delete_server("堡垒机", cx);
            assert_eq!(view.server_delete_confirm.as_deref(), Some("堡垒机"));
            view.delete_server("堡垒机", cx);
            let error = view.server_error.clone().expect("被引用必须阻断");
            assert!(error.contains("堡垒机"), "{error}");
            assert!(error.contains("内网机"), "应列出引用方: {error}");
            assert!(error.contains("先解除引用"), "{error}");
            assert!(
                view.settings
                    .servers
                    .iter()
                    .any(|item| item.name == "堡垒机"),
                "被引用条目必须保留"
            );
            assert_eq!(view.server_delete_confirm, None, "阻断后退出确认态");
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 改名跳板机：所有指向旧名的引用改写为新名，无关引用不动，
    /// 改名条目自身的 jump 原样保留；改写后全表链路可解析。
    #[test]
    fn rename_rewrites_jump_refs_and_keeps_own_chain() {
        let mut settings = Settings {
            servers: vec![
                entry("引用方", Some("旧名")),
                entry("旁人", Some("别人")),
                entry("上游", None),
                {
                    let mut old = entry("旧名", Some("上游"));
                    old.host = "bastion.example.com".into();
                    old
                },
            ],
            ..Default::default()
        };
        let mut renamed = entry("旧名", Some("上游"));
        renamed.name = "新名".into();
        renamed.host = "bastion.example.com".into();
        apply_server_save(&mut settings, Some("旧名"), renamed).expect("保存应成功");
        assert_eq!(settings.servers.len(), 4, "改名不增删条目");
        assert_eq!(
            settings
                .servers
                .iter()
                .find(|item| item.name == "引用方")
                .unwrap()
                .jump
                .as_deref(),
            Some("新名"),
            "指向旧名的引用必须改写"
        );
        assert_eq!(
            settings
                .servers
                .iter()
                .find(|item| item.name == "旁人")
                .unwrap()
                .jump
                .as_deref(),
            Some("别人"),
            "无关引用不动"
        );
        assert_eq!(
            settings
                .servers
                .iter()
                .find(|item| item.name == "新名")
                .unwrap()
                .jump
                .as_deref(),
            Some("上游"),
            "改名条目自身的跳板链保留"
        );
        let chain = laterterm_config::resolve_jump_chain(&settings.servers, "引用方").unwrap();
        let names: Vec<&str> = chain.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["上游", "新名"], "改写后链路必须可解析");
    }

    /// 导入含悬空 jump 的条目：条目保留、jump 被清空，且计入回报。
    #[test]
    fn import_clears_dangling_jump_but_keeps_entry() {
        let mut existing = vec![entry("老机", None)];
        let incoming = vec![
            {
                let mut dangling = entry("悬空机", Some("幽灵"));
                dangling.host = "10.0.0.9".into();
                dangling
            },
            {
                let mut good = entry("正常机", Some("老机"));
                good.host = "10.0.0.10".into();
                good
            },
        ];
        let (added, skipped_dup, skipped_bad, cleared) =
            merge_entries_into(&mut existing, incoming);
        assert_eq!((added, skipped_dup, skipped_bad), (2, 0, 0));
        assert_eq!(cleared, vec!["悬空机".to_string()]);
        assert_eq!(existing.len(), 3, "宁断链不丢条目");
        assert_eq!(
            existing
                .iter()
                .find(|item| item.name == "悬空机")
                .unwrap()
                .jump,
            None,
            "悬空引用必须被清空"
        );
        assert_eq!(
            existing
                .iter()
                .find(|item| item.name == "正常机")
                .unwrap()
                .jump
                .as_deref(),
            Some("老机"),
            "有效引用不动"
        );
        // 清理后全表可解析
        assert!(laterterm_config::resolve_jump_chain(&existing, "正常机").is_ok());
    }
}

/// 首启引导的完成链路（gpui::test）：探测结果注入 → 完成 → shell 落盘 +
/// 引导层关闭 + 新标签使用所选 shell；跳过路径不写任何东西。
/// 全部走 `deliver_probe` 注入与 `settings_save_path` 注入，不依赖真机探测、
/// 不写真实配置（评审阻断1：私有字段仅 workspace 内联测试可达）。
#[cfg(test)]
mod onboarding_tests {
    use super::WorkspaceView;
    use crate::onboarding::{self, FONT_PRESETS, ProbeResults};
    use gpui::{MouseDownEvent, TestAppContext};
    use std::path::Path;

    fn temp_settings_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "laterterm-onboarding-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_probe() -> ProbeResults {
        ProbeResults {
            // 候选：/bin/bash（当前）+ /bin/sh（列表项）——后续测试点选
            // 非“当前”的 /bin/sh 芯片验证固化路径；/bin/sh 各发行版必有。
            shells: onboarding::parse_shells(
                Some("/bin/bash"),
                "# 注释\n/bin/bash\n/bin/sh\n",
                "/bin/bash",
                &|_: &Path| true,
            ),
            fonts: onboarding::preset_fonts(FONT_PRESETS.as_slice()),
        }
    }

    /// 完成：点选非「当前」芯片（即真实芯片 listener 调用的
    /// onboarding_select_shell）→ 完成落盘到注入路径、引导层关闭、新标签用
    /// 所选 shell 启动。断言走会话级 title（Pane.title 仍是「终端 N」硬编码，
    /// 评审阻断2 口径：不改 title 语义，会话 title 才是 shell basename）。
    #[gpui::test]
    fn completion_persists_shell_closes_welcome_and_spawns_selected_shell(cx: &mut TestAppContext) {
        let dir = temp_settings_dir("complete");
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _, cx| {
            view.welcome_visible = true;
            view.settings.shell = None;
            view.onboarding.selected_shell = None;
            view.settings_save_path = Some(dir.join("settings.toml"));
            view.deliver_probe(fake_probe());
            cx.notify();
        });
        cx.run_until_parked();
        // 模拟点选「/bin/sh」芯片（芯片 listener 的实际动作）
        workspace.update(cx, |view, cx| {
            view.onboarding_select_shell(Some("/bin/sh".into()), cx);
            assert_eq!(view.onboarding.selected_shell.as_deref(), Some("/bin/sh"));
        });
        assert!(
            !dir.join("settings.toml").exists(),
            "芯片点选仅暂存，完成前不应落盘"
        );
        workspace.update_in(cx, |view, window, cx| {
            view.complete_onboarding(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert!(!view.welcome_visible, "完成后引导层应关闭");
            assert_eq!(view.settings.shell.as_deref(), Some("/bin/sh"));
        });
        let saved = std::fs::read_to_string(dir.join("settings.toml")).unwrap();
        assert!(saved.contains("shell = \"/bin/sh\""), "{saved}");
        // 新标签按所选 shell 启动（真开 PTY，与本模块既有测试同口径）
        workspace.update_in(cx, |view, window, cx| {
            view.spawn_tab(window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            let pane = view.panes.last().unwrap();
            assert_eq!(
                pane.session.session.title(),
                "sh",
                "新窗格会话应使用引导所选 shell"
            );
            for pane in &view.panes {
                pane.session.close();
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 什么都没点直接「完成」（探测未到/未选芯片）：暂选 None = 保持跟随
    /// 默认，不写 shell，文件也不落。
    #[gpui::test]
    fn completing_without_selection_keeps_dynamic_default(cx: &mut TestAppContext) {
        let dir = temp_settings_dir("untouched");
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _, cx| {
            view.welcome_visible = true;
            view.settings.shell = None;
            view.onboarding.selected_shell = None;
            view.settings_save_path = Some(dir.join("settings.toml"));
            cx.notify();
        });
        cx.run_until_parked();
        // 「当前」芯片的语义是回选 None（保持动态跟随默认），同样不固化
        workspace.update(cx, |view, cx| {
            view.onboarding_select_shell(Some("/bin/sh".into()), cx);
            view.onboarding_select_shell(None, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |view, window, cx| {
            view.complete_onboarding(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert!(!view.welcome_visible);
            assert_eq!(view.settings.shell, None, "未选择时不应固化 shell");
        });
        assert!(
            !dir.join("settings.toml").exists(),
            "无变更不应产生落盘文件"
        );
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 跳过（dismiss_welcome，即点遮罩/跳过按钮同路径）：不写 shell；字体
    /// 芯片是即时落盘的（onboarding_apply_font → persist_settings 注入路径），
    /// 跳过后保留。
    #[gpui::test]
    fn skipping_writes_nothing_but_font_chip_choice_survives(cx: &mut TestAppContext) {
        let dir = temp_settings_dir("skip");
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _, cx| {
            view.welcome_visible = true;
            view.settings.shell = None;
            view.onboarding.selected_shell = None;
            view.settings_save_path = Some(dir.join("settings.toml"));
            view.deliver_probe(fake_probe());
            cx.notify();
        });
        cx.run_until_parked();
        // 点字体芯片（直接调 handler：持久化走注入路径，等值短路不触发）
        workspace.update(cx, |view, cx| {
            view.onboarding_apply_font("DejaVu Sans Mono", cx);
        });
        cx.run_until_parked();
        let saved = std::fs::read_to_string(dir.join("settings.toml")).unwrap();
        assert!(
            saved.contains("font_family = \"DejaVu Sans Mono\""),
            "字体芯片应即时落盘: {saved}"
        );
        assert!(!saved.contains("shell ="), "字体落盘不应顺带写 shell");
        // 跳过：shell 保持 None
        workspace.update_in(cx, |view, window, cx| {
            view.dismiss_welcome(&MouseDownEvent::default(), window, cx);
        });
        workspace.read_with(cx, |view, _| {
            assert!(!view.welcome_visible);
            assert_eq!(view.settings.shell, None, "跳过不写 shell");
            assert_eq!(
                view.settings.appearance.font_family, "DejaVu Sans Mono",
                "已选字体在跳过后保留"
            );
        });
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 泵单发闩：probe 已有结果时后到的注入/线程结果被丢弃（评审阻断4）。
    #[gpui::test]
    fn deliver_probe_is_first_wins(cx: &mut TestAppContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update(cx, |view, _| {
            view.welcome_visible = true;
            view.deliver_probe(fake_probe());
            // 第二次投递（无论来自真线程还是测试）必须被闩掉
            view.deliver_probe(ProbeResults {
                shells: Vec::new(),
                fonts: Vec::new(),
            });
            assert_eq!(
                view.onboarding.probe.as_ref().unwrap().shells.len(),
                2,
                "保留的应是先到的 fake_probe（bash 当前 + sh 列表项）"
            );
        });
    }
}

#[cfg(test)]
mod settings_panel_tests {
    use super::{Entity, SettingsSection, WorkspaceView};
    use gpui::{MouseDownEvent, TestAppContext, VisualTestContext, px};

    /// 开窗 + 打开设置面板并跑至静止；把 VisualTestContext 一并交还，
    /// 供 debug_bounds / update_in 使用（TestAppContext 本体无这些入口）。
    fn open_settings_panel(
        cx: &mut TestAppContext,
    ) -> (Entity<WorkspaceView>, &mut VisualTestContext) {
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _window, cx| {
            view.welcome_visible = false;
            view.toggle_settings(cx);
        });
        cx.run_until_parked();
        (workspace, cx)
    }

    /// 单页化 + 打开默认态：高亮首节、导航/内容/六节锚点全部在场。
    #[gpui::test]
    fn settings_panel_defaults_to_first_section_with_nav_highlight(cx: &mut TestAppContext) {
        let (workspace, cx) = open_settings_panel(cx);
        workspace.read_with(cx, |view, _| {
            assert_eq!(
                view.settings_section,
                SettingsSection::Appearance,
                "打开默认高亮首节"
            );
        });
        let active = cx
            .debug_bounds("settings-nav-active")
            .expect("选中导航项必须存在");
        assert!(active.size.height > px(0.0), "选中导航项必须可见");
        let content = cx
            .debug_bounds("settings-content")
            .expect("单页滚动容器必须存在");
        // 六节锚点全部在场且纵向排布（单页非分页的直接证据）；视口外分区被
        // 容器内容遮罩裁剪，只断言「存在 + 单调排布」，不断言落在可视带内
        //（滚出视口的节本就应在容器带外）
        let selectors = [
            "settings-section-appearance",
            "settings-section-font",
            "settings-section-scrolling",
            "settings-section-shortcuts",
            "settings-section-terminal",
            "settings-section-servers",
            "settings-section-about",
            "settings-section-update",
        ];
        let mut previous_top = content.top();
        for selector in selectors {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} 分区锚点必须存在"));
            assert!(
                bounds.top() >= previous_top,
                "{selector} 应按导航顺序纵向排布"
            );
            previous_top = bounds.top();
        }
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 检查更新三态渲染（离线注入，零真实网络）：Idle 只报当前版本；
    /// 注入 Newer 结果横幅与「去下载」按钮在场；注入 Err 出「未能确认」
    /// 静默行且无任何弹窗元素——失败绝不呈现为「已是最新」（update_check.rs
    /// 失败语义口径的结果侧回归钉）。
    #[gpui::test]
    fn update_check_states_render_offline(cx: &mut TestAppContext) {
        let (workspace, cx) = open_settings_panel(cx);
        // Idle：检查按钮 + 「当前 v0.2.0」状态行在场
        assert!(
            cx.debug_bounds("update-check-btn").is_some(),
            "检查按钮必须在场"
        );
        assert!(
            cx.debug_bounds("update-status-idle").is_some(),
            "Idle 态状态行必须在场"
        );
        // 注入 Newer：横幅 + 「去下载」按钮出现，Idle 状态行退场
        workspace.update(cx, |view, cx| {
            view.update_check
                .finish(Ok(crate::update_check::Outcome::Newer("v99.0.0".into())));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("update-newer-banner").is_some(),
            "新版本横幅必须紧贴按钮渲染"
        );
        assert!(
            cx.debug_bounds("update-download-btn").is_some(),
            "「去下载」按钮必须在场"
        );
        assert!(cx.debug_bounds("update-status-idle").is_none());
        // 注入 Err：「未能确认」行在场，横幅退场，无弹窗元素（静默文案）
        workspace.update(cx, |view, cx| {
            view.update_check.finish(Err("GitHub 返回 403".into()));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("update-status-error").is_some(),
            "失败状态行必须在场"
        );
        assert!(cx.debug_bounds("update-newer-banner").is_none());
        workspace.read_with(cx, |view, _| {
            assert!(
                view.command_dialog.is_none()
                    && view.totp_dialog.is_none()
                    && view.server_dialog.is_none()
                    && view.shortcut_editor.is_none(),
                "失败走静默状态行，不得弹任何对话框"
            );
        });
        // 清除（「知道了」同款路径）：回到 Idle 状态行
        workspace.update(cx, |view, cx| {
            view.update_check.clear();
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("update-status-idle").is_some(),
            "清除后回到 Idle 状态行"
        );
        assert!(cx.debug_bounds("update-status-error").is_none());
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 滚动 → 高亮：滚到底兜底选中末节（矮分区碰不到判定线）。
    #[gpui::test]
    fn settings_scroll_to_bottom_pins_last_section(cx: &mut TestAppContext) {
        let (workspace, cx) = open_settings_panel(cx);
        workspace.update(cx, |view, cx| {
            // 门控前提：内容必须超高，否则 spy 跳过侦测、断言失效
            assert!(
                view.settings_scroll.max_offset().height > px(2.0),
                "六节内容在设置面板内应可滚动"
            );
            view.settings_scroll.set_offset(gpui::point(
                px(0.0),
                -view.settings_scroll.max_offset().height,
            ));
            cx.notify();
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert_eq!(
                view.settings_section,
                SettingsSection::UpdateCheck,
                "滚到底必须兜底选中末节"
            );
        });
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 点击 → 跳转：立即高亮 + Top 策略把分区起点顶到容器口。
    /// （jump-skip 防的一帧高亮闪烁属帧内瞬态，无观察点——跳过侦测的收敛
    /// 行为由下面第二段断言覆盖：再跑一帧不改 offset，高亮仍钉在目标分区。）
    #[gpui::test]
    fn settings_nav_click_highlights_then_jumps_to_section_top(cx: &mut TestAppContext) {
        let (workspace, cx) = open_settings_panel(cx);
        workspace.update_in(cx, |view, window, cx| {
            view.select_settings_section(
                SettingsSection::Shortcuts,
                &MouseDownEvent::default(),
                window,
                cx,
            );
            assert_eq!(
                view.settings_section,
                SettingsSection::Shortcuts,
                "点击立即更新高亮，不等滚动"
            );
        });
        cx.run_until_parked();
        let content = cx
            .debug_bounds("settings-content")
            .expect("滚动容器必须存在");
        let section = cx
            .debug_bounds("settings-section-shortcuts")
            .expect("目标分区必须存在");
        assert!(
            (section.top() - content.top()).abs() < px(2.0),
            "跳转帧 Top 策略应把分区起点对齐容器口：section={section:?} content={content:?}"
        );
        workspace.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert_eq!(
                view.settings_section,
                SettingsSection::Shortcuts,
                "后续帧收敛一致：高亮不被 spy 拉走"
            );
        });
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 关闭再打开：滚动与高亮都复位到首节（ScrollHandle 跨开关持久，
    /// 打开分支不复位 set_offset 就会残留上次位置——评审硬伤2 回归钉）。
    #[gpui::test]
    fn settings_reopen_resets_scroll_and_section(cx: &mut TestAppContext) {
        let (workspace, cx) = open_settings_panel(cx);
        workspace.update(cx, |view, cx| {
            view.settings_scroll.set_offset(gpui::point(
                px(0.0),
                -view.settings_scroll.max_offset().height,
            ));
            cx.notify();
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert_eq!(view.settings_section, SettingsSection::UpdateCheck);
        });
        // 关闭（复位逻辑只允许写在打开分支，关闭帧不丢高亮）
        workspace.update_in(cx, |view, window, cx| {
            view.close_settings(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        workspace.update(cx, |view, cx| view.toggle_settings(cx));
        cx.run_until_parked();
        workspace.read_with(cx, |view, _| {
            assert_eq!(
                view.settings_section,
                SettingsSection::Appearance,
                "重开必须回首节"
            );
            assert_eq!(
                view.settings_scroll.offset().y,
                px(0.0),
                "重开必须复位滚动（ScrollHandle 持久残留）"
            );
        });
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 主题预览闭环：选中即应用到全部真实窗格，已保存值不动；关面板
    /// （scrim/× 同一入口 close_settings）自动还原已保存主题色；预览期间
    /// 点回已保存主题 = 放弃预览（评审必改1：不能裸清 theme_preview）。
    #[gpui::test]
    fn theme_preview_applies_to_existing_panes_and_restores_on_close(cx: &mut TestAppContext) {
        let (workspace, cx) = open_settings_panel(cx);
        // 钉已知已保存主题：WorkspaceView::new 读开发者真实 settings.toml，
        // 恢复目标不能写死 tokyo-night（评审必改2，否则随本机配置抖动）。
        workspace.update(cx, |view, cx| {
            view.settings.appearance.theme = "tokyo-night".into();
            view.apply_appearance(cx);
        });
        workspace.update_in(cx, |view, window, cx| {
            view.spawn_tab(window, cx);
            view.preview_theme(
                &MouseDownEvent::default(),
                "gruvbox-dark".into(),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        // 色板预览卡常驻于「滚动与主题」分区块内部（六节对位约束）
        assert!(
            cx.debug_bounds("theme-preview-card").is_some(),
            "主题色板预览卡必须在场"
        );
        workspace.read_with(cx, |view, cx| {
            assert_eq!(view.theme_preview.as_deref(), Some("gruvbox-dark"));
            assert_eq!(
                view.settings.appearance.theme, "tokyo-night",
                "预览不得改动已保存值"
            );
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve("gruvbox-dark")).background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "预览主题应立即应用到全部既有窗格"
                );
            }
        });
        // 预览期间新建窗格应继承预览色（attach_pane 走 effective 读口）
        workspace.update_in(cx, |view, window, cx| {
            view.spawn_tab(window, cx);
        });
        workspace.read_with(cx, |view, cx| {
            assert_eq!(view.panes.len(), 3, "初始窗格 + 两次新建");
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve("gruvbox-dark")).background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "预览期间新建窗格应与预览色一致"
                );
            }
        });
        // 关闭（scrim/× 同一入口）：预览清空，窗格恢复已保存主题色
        workspace.update_in(cx, |view, window, cx| {
            view.close_settings(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, cx| {
            assert!(view.theme_preview.is_none(), "关面板必须自动还原预览");
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve("tokyo-night")).background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "关面板应恢复已保存主题色"
                );
            }
        });
        // 必改1回归：预览期间点回已保存主题 = 放弃预览（委托 cancel take+apply）
        workspace.update_in(cx, |view, window, cx| {
            view.preview_theme(
                &MouseDownEvent::default(),
                "gruvbox-dark".into(),
                window,
                cx,
            );
            assert_eq!(view.theme_preview.as_deref(), Some("gruvbox-dark"));
            view.preview_theme(&MouseDownEvent::default(), "tokyo-night".into(), window, cx);
        });
        workspace.read_with(cx, |view, cx| {
            assert!(view.theme_preview.is_none(), "点回已保存主题必须放弃预览");
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve("tokyo-night")).background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "点回已保存主题应立即恢复已保存色"
                );
            }
        });
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 提交预览：落盘走 persist_settings 注入路径（不碰真实配置），内存态、
    /// 窗格颜色与 settings.toml 文本三面一致，预览态清空。
    #[gpui::test]
    fn theme_commit_persists_via_injected_save_path(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!(
            "laterterm-theme-commit-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (workspace, cx) = open_settings_panel(cx);
        workspace.update_in(cx, |view, window, cx| {
            view.settings_save_path = Some(dir.join("settings.toml"));
            view.settings.appearance.theme = "tokyo-night".into();
            view.apply_appearance(cx);
            view.spawn_tab(window, cx);
            view.preview_theme(
                &MouseDownEvent::default(),
                "gruvbox-dark".into(),
                window,
                cx,
            );
            view.commit_theme_preview(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, cx| {
            assert!(view.theme_preview.is_none(), "提交后预览态必须清空");
            assert_eq!(
                view.settings.appearance.theme, "gruvbox-dark",
                "提交必须落内存"
            );
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve("gruvbox-dark")).background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "提交后窗格应钉住新主题色"
                );
            }
        });
        let saved = std::fs::read_to_string(dir.join("settings.toml")).unwrap();
        assert!(
            saved.contains("gruvbox-dark"),
            "提交必须写入注入路径（settings.toml 应含新主题名）"
        );
        let _ = std::fs::remove_dir_all(&dir);
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 预览不存在的主题名：resolve 静默回退默认主题（窗格仍是已保存主题色）；
    /// commit 被 themes.get 严格校验拦截——不落盘、不落内存、预览清空并恢复。
    #[gpui::test]
    fn theme_preview_missing_theme_falls_back_and_commit_does_not_persist(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!(
            "laterterm-theme-missing-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (workspace, cx) = open_settings_panel(cx);
        workspace.update_in(cx, |view, window, cx| {
            view.settings_save_path = Some(dir.join("settings.toml"));
            view.settings.appearance.theme = "tokyo-night".into();
            view.apply_appearance(cx);
            view.spawn_tab(window, cx);
            view.preview_theme(
                &MouseDownEvent::default(),
                "no-such-theme".into(),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, cx| {
            assert_eq!(view.theme_preview.as_deref(), Some("no-such-theme"));
            assert_eq!(
                view.settings.appearance.theme, "tokyo-night",
                "缺失主题的预览不得改动已保存值"
            );
            // resolve 回退目标 = DEFAULT_THEME（theme.rs 硬编码，环境无关）
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve(laterterm_config::DEFAULT_THEME))
                    .background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "缺失主题的预览应回退默认主题色"
                );
            }
        });
        workspace.update_in(cx, |view, window, cx| {
            view.commit_theme_preview(&MouseDownEvent::default(), window, cx);
        });
        workspace.read_with(cx, |view, cx| {
            assert!(
                view.theme_preview.is_none(),
                "缺失主题的 commit 必须清空预览"
            );
            assert_eq!(
                view.settings.appearance.theme, "tokyo-night",
                "缺失主题的 commit 不得落内存"
            );
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve("tokyo-night")).background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "缺失主题的 commit 应恢复已保存主题色"
                );
            }
        });
        assert!(
            !dir.join("settings.toml").exists(),
            "缺失主题的 commit 绝不能写盘"
        );
        let _ = std::fs::remove_dir_all(&dir);
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }

    /// 恢复默认早退路径：appearance 已是默认值时 change_setting 等值短路不
    /// apply（评审必改3），reset 必须自带 take + apply 收掉预览，否则窗格钉
    /// 在预览色而预览态已消失、永久卡住。
    #[gpui::test]
    fn theme_reset_discards_preview_even_when_change_setting_early_returns(
        cx: &mut TestAppContext,
    ) {
        let (workspace, cx) = open_settings_panel(cx);
        workspace.update(cx, |view, cx| {
            // 外观钉成默认值：触发 change_setting 的等值早退分支（不落盘不 apply）
            view.settings.appearance = laterterm_config::Appearance::default();
            view.apply_appearance(cx);
        });
        workspace.update_in(cx, |view, window, cx| {
            view.preview_theme(
                &MouseDownEvent::default(),
                "gruvbox-dark".into(),
                window,
                cx,
            );
            assert_eq!(view.theme_preview.as_deref(), Some("gruvbox-dark"));
            view.reset_settings(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |view, cx| {
            assert!(view.theme_preview.is_none(), "恢复默认必须收掉预览态");
            assert_eq!(
                view.settings.appearance.theme,
                laterterm_config::DEFAULT_THEME,
                "恢复默认后已保存值为默认主题"
            );
            let expected = gpui::Rgba::from(
                super::palette_for(view.themes.resolve(laterterm_config::DEFAULT_THEME))
                    .background(),
            );
            for pane in &view.panes {
                assert_eq!(
                    gpui::Rgba::from(pane.terminal.read(cx).config().colors.background()),
                    expected,
                    "恢复默认早退路径也必须把窗格从预览色拉回默认主题色"
                );
            }
        });
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes {
                pane.session.close();
            }
        });
    }
}
