use crate::branding::titlebar_logo;
use crate::command_text_field::{CommandTextChanged, CommandTextField, FieldSubmit};
use crate::pane_navigation::{Direction as PaneDirection, PaneRect, neighbor};
use crate::server_connection::{self, ConnectionOutcome, CredentialKind};
use crate::shortcut_recorder::{ShortcutChanged, ShortcutRecorder};
use gpui::{
    App, AppContext, AsyncApp, Bounds, Context, CursorStyle, DispatchPhase, Edges, Entity,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ScrollHandle,
    MouseMoveEvent, MouseUpEvent, ParentElement, PathPromptOptions, Render,
    StatefulInteractiveElement, Styled, WeakEntity, Window, canvas, div, prelude::FluentBuilder,
    px, relative, rgb,
};
use laterterm_config::{
    Appearance, CommandTemplate, ServerAuthKind, ServerEntry, ServersFile, Settings, Theme,
    ThemeRegistry, TotpAlgorithm, TotpConfig, TotpEntry, app_config_dir, deepin_default_path,
    generate_code, load_deepin_servers, load_xshell_sessions, normalize_shortcut,
    parse_secret_input,
};
use laterterm_core::TerminalSession;
use laterterm_local::{LocalOptions, LocalSession};
use laterterm_secrets::{SecretStore, default_store, totp_account};
use laterterm_termview::view::ScrollSummary;
use laterterm_termview::{ColorPalette, TerminalConfig, TerminalView};
use notify::Watcher;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TITLEBAR_HEIGHT: f32 = 34.0;
const PANEL_WIDTH: f32 = 240.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsPage {
    Appearance,
    Shortcuts,
    Terminal,
    Servers,
}

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

/// 服务器侧面板对话框。Connect 用于缺凭据时补输（可选择记住入密钥环）。
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
}

struct SshRequest {
    id: u64,
    entry: ServerEntry,
    active: Arc<AtomicBool>,
    running: bool,
}

type PendingSshResult = (u64, ServerEntry, anyhow::Result<ConnectionOutcome>);

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

struct Pane {
    id: PaneId,
    title: String,
    terminal: Entity<TerminalView>,
    session: Arc<SessionControl>,
    status: TabStatus,
}

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
    left_panel: bool,
    tab_scroll: ScrollHandle,
    right_panel: bool,
    settings_open: bool,
    broadcast: bool,
    welcome_visible: bool,
    broadcast_tx: flume::Sender<(PaneId, Vec<u8>)>,
    broadcast_rx: flume::Receiver<(PaneId, Vec<u8>)>,
    settings_page: SettingsPage,
    shortcut_editor: Option<ShortcutEditor>,
    panel_shortcut_dialog: Option<BindingOwner>,
    shortcut_error: Option<String>,
    settings: Settings,
    themes: ThemeRegistry,
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
    /// 导入来源选择弹窗（Some(已选来源) = 打开中）。
    server_import_dialog: Option<ServerImportSource>,
    /// 正在连接中的服务器名（None 表示空闲；期间禁止并发连接）。
    ssh_connecting: Option<String>,
    ssh_request: Option<SshRequest>,
    next_ssh_request_id: u64,
    /// 后台结果按请求标识校验，过期成功会话立即关闭。
    pending_ssh_results: Vec<PendingSshResult>,
    /// 待生效的焦点请求：异步上下文拿不到 window，先记下 handle，render 期统一 focus。
    pending_focus: Option<FocusHandle>,
    secret_store: Arc<dyn SecretStore>,
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
        );
        cx.subscribe_in(
            &terminal,
            window,
            move |this, _, _: &laterterm_termview::view::TerminalExited, window, cx| {
                this.mark_exited(first_pane, window, cx);
            },
        )
        .detach();
        let view = Self {
            settings,
            themes,
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
            server_import_dialog: None,
            ssh_connecting: None,
            ssh_request: None,
            next_ssh_request_id: 1,
            pending_ssh_results: Vec::new(),
            pending_focus: None,
            next_split_id: 1,
            pane_geometry: Rc::new(RefCell::new(None)),
            split_drag: None,
            secret_store: default_store(),
            settings_page: SettingsPage::Appearance,
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
            broadcast_tx,
            broadcast_rx,
        };
        Self::watch_config_files(cx);
        view.start_shortcut_pump(cx);
        view.start_broadcast_pump(cx);
        view.start_totp_ticker(cx);
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
        let theme = self.themes.resolve(&self.settings.appearance.theme).clone();
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
        let session = spawn_local_session();
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
        let session = spawn_local_session();
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
    fn close_tab(
        &mut self,
        id: TabId,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    fn toggle_settings(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_open = !self.settings_open;
        cx.notify();
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

    /// 把当前设置（外观五项 + 主题）应用到所有窗格，不重建终端。
    fn apply_appearance(&mut self, cx: &mut Context<Self>) {
        let appearance = self.settings.appearance.clone();
        let palette = palette_for(self.themes.resolve(&appearance.theme));
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
                config.font_fallbacks = appearance.font_fallbacks.clone();
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
            self.settings = settings;
            self.themes = themes;
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

    fn select_settings_page(
        &mut self,
        page: SettingsPage,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_page = page;
        cx.notify();
    }

    fn close_settings(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_open = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn save_command(&mut self, command: CommandTemplate, cx: &mut Context<Self>) {
        let previous = self.settings.clone();
        if let Err(error) = self.settings.upsert_command(command) {
            self.command_error = Some(error.to_string());
            return;
        }
        if let Some(path) = Settings::default_path()
            && let Err(error) = self.settings.save_to(&path)
        {
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
        if let Some(path) = Settings::default_path()
            && let Err(error) = self.settings.save_to(&path)
        {
            self.settings = previous;
            self.command_error = Some(format!("删除命令失败: {error}"));
        } else {
            self.command_delete_confirm = None;
            self.command_error = None;
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
                    } else if let Some(path) = Settings::default_path()
                        && let Err(error) = workspace.settings.save_to(&path)
                    {
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
                    if let Some(path) = Settings::default_path()
                        && let Err(error) = workspace.settings.save_to(&path)
                    {
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
        let rows: [(&str, String, &str); 8] = [
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
                        if let Some(previous) = &original
                            && previous != &meta.name
                        {
                            workspace.settings.remove_server(previous);
                        }
                        if let Err(error) = workspace.settings.upsert_server(meta) {
                            workspace.settings = previous_settings;
                            workspace.server_error = Some(error.to_string());
                        } else if let Some(path) = Settings::default_path()
                            && let Err(error) = workspace.settings.save_to(&path)
                        {
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
                    if let Some(path) = Settings::default_path()
                        && let Err(error) = workspace.settings.save_to(&path)
                    {
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

    /// 来源二：Deepin 终端远程管理列表，一键读取默认路径（无需选文件）。
    fn import_servers_deepin(&mut self, cx: &mut Context<Self>) {
        let Some(path) = deepin_default_path() else {
            self.server_error = Some("无法定位 HOME 目录，Deepin 导入失败".into());
            cx.notify();
            return;
        };
        match load_deepin_servers(&path) {
            Ok(entries) => {
                if let Some(parent) = path.parent()
                    && let Ok(text) = std::fs::read_to_string(parent.join("command-config.conf"))
                    && let Ok(commands) = laterterm_deepin::parse_commands(&text)
                {
                    for command in crate::import::deepin_commands_batch(&commands) {
                        if !self
                            .settings
                            .commands
                            .iter()
                            .any(|entry| entry.name == command.name)
                        {
                            self.settings.commands.push(command);
                        }
                    }
                    self.settings.save();
                }
                self.merge_server_entries(entries, &path.display().to_string(), cx);
            }
            Err(error) => {
                self.server_error = Some(format!("Deepin 导入失败: {error:#}"));
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
                imported.extend(entries.into_iter().map(ServerEntry::from));
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
    fn merge_server_entries(
        &mut self,
        entries: Vec<ServerEntry>,
        source_desc: &str,
        cx: &mut Context<Self>,
    ) {
        let previous = self.settings.clone();
        let mut added = 0usize;
        let mut skipped_dup = 0usize;
        let mut skipped_bad = 0usize;
        for entry in entries {
            let Some(entry) = entry.sanitized() else {
                skipped_bad += 1;
                continue;
            };
            if self
                .settings
                .servers
                .iter()
                .any(|item| item.name == entry.name)
            {
                skipped_dup += 1;
                continue;
            }
            self.settings.servers.push(entry);
            added += 1;
        }
        if added > 0
            && let Some(config_path) = Settings::default_path()
            && let Err(error) = self.settings.save_to(&config_path)
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
        if added == 0 {
            if skipped_dup == 0 && skipped_bad == 0 {
                self.server_error = Some(format!("没有可导入的服务器条目（{source_desc}）"));
            } else {
                self.server_error = Some(notice);
            }
        } else {
            self.server_notice = Some(notice);
        }
        cx.notify();
    }

    /// 点连接立即占用请求槽，准备私钥和密钥环操作都在后台进行。
    fn connect_server(&mut self, name: &str, cx: &mut Context<Self>) {
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
        self.ssh_request = Some(SshRequest {
            id,
            entry: entry.clone(),
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
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let result = server_connection::connect(&entry, supplied, remember, store, active);
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
                    && let Some((_, _, Ok(ConnectionOutcome::Connected { session, .. }))) = result
                {
                    session.kill();
                }
            }
        })
        .detach();
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
                    name: entry.name,
                    kind,
                    request_id,
                    remember: false,
                });
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

    /// 需要 window 的结果在 render 期消费。
    fn process_ssh_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (request_id, entry, result) in std::mem::take(&mut self.pending_ssh_results) {
            self.finish_ssh_connect(request_id, entry, result, window, cx);
        }
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
        let result = self.settings.shortcuts.validate().and_then(|()| {
            let path =
                Settings::default_path().ok_or_else(|| anyhow::anyhow!("无法确定配置目录"))?;
            self.settings.save_to(&path)
        });
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
                                                    cx.listener(move |this, event, window, cx| {
                                                        this.close_tab(id, event, window, cx)
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
                    .child(button("⚙", cx.listener(Self::toggle_settings)))
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
                    settings_navigation(self.settings_page, cx),
                    settings_content(self.settings_page, self, cx),
                ))
            })
            .when(self.welcome_visible, |root| {
                root.child(welcome_overlay(
                    cx.listener(Self::dismiss_welcome),
                    cx.listener(Self::dismiss_welcome),
                ))
            })
            // 命令/动态码/服务器对话框：窗口级模态弹窗（最上层）
            .when_some(render_active_dialog(self, cx), |root, overlay| {
                root.child(overlay)
            })
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

/// 首启欢迎覆盖层：快捷键速查 + 开始按钮。
fn welcome_overlay(
    scrim_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    button_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
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
        .on_mouse_down(MouseButton::Left, scrim_listener)
        .child(
            div()
                .w(px(520.0))
                .p_8()
                .bg(rgb(0x1A1B26))
                .flex()
                .flex_col()
                .gap_4()
                .on_mouse_down(MouseButton::Left, |_, _, _| {})
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
                    div()
                        .pt_4()
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
                        .child(welcome_shortcut("广播输入（多窗格同步）", "Ctrl+Shift+B"))
                        .child(welcome_shortcut("设置 / 服务器管理", "Ctrl+,")),
                )
                .child(
                    div()
                        .pt_4()
                        .text_color(rgb(0x565A78))
                        .text_size(px(12.0))
                        .child("提示：右侧面板 → 服务器 可添加 SSH 连接或从 XShell / deepin 导入"),
                )
                .child(
                    div().pt_4().child(
                        div()
                            .px_6()
                            .py_2()
                            .bg(rgb(0x7AA2F7))
                            .text_color(rgb(0x1A1B26))
                            .text_size(px(16.0))
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, button_listener)
                            .child("开始使用"),
                    ),
                ),
        )
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
                        div().absolute().top_0().left_0().right_0().h(px(2.0))
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
                .child(div().flex().items_center().gap_2()
                    .child("会话")
                    .child(div().px_1().rounded_sm().bg(rgb(0x303548)).text_color(rgb(0xC0CAF5)).child(tab_labels.len().to_string())))
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
                        .debug_selector(move || if selected { "sidebar-active-tab".into() } else { "sidebar-other-tab".into() })
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
                        .child(div().text_size(px(13.0)).text_color(rgb(0x7AA2F7)).child("›_"))
                        .child(div().flex_1().min_w(px(0.0)).truncate().text_size(px(13.0)).child(label))
                        .when(count > 1, |row| row.child(div().text_size(px(11.0)).text_color(rgb(0xA9B1D6)).child(count.to_string())))
                        .child(
                            div()
                                .px_1()
                                .opacity(0.0)
                                .group_hover("session-row", |style| style.opacity(1.0))
                                .text_color(rgb(0xA9B1D6))
                                .hover(|style| style.text_color(rgb(0xF7768E)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event, window, cx| {
                                        this.close_tab(id, event, window, cx)
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
            .child(setting_button(
                "新建命令",
                true,
                cx.listener(|this, _event, window, cx| this.edit_command(None, window, cx)),
            ))
            .child(panel_caption("命令模板与快捷键"))
            .when_some(error, |panel, error| {
                panel.child(div().text_color(rgb(0xF7768E)).child(error))
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
                            .child(sidebar_shortcut_chip(sidebar_shortcut_label(command.shortcut.as_deref())))
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
    if let Some(ServerDialog::Connecting { name }) = workspace.server_dialog.clone() {
        let body = div()
            .p_4()
            .child("正在准备连接（可取消）…")
            .child(dialog_footer(vec![setting_button(
                "取消连接",
                false,
                cx.listener(WorkspaceView::cancel_server),
            )]));
        return Some(modal_dialog(
            format!("连接 {name}"),
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
    modal_dialog(
        format!("连接 {name}"),
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

    panel
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().debug_selector(|| "server-new-button".into()).flex_1().min_w(px(0.0)).child(setting_button(
                    "+ 新建",
                    true,
                    cx.listener(|this, _event, window, cx| this.edit_server(None, window, cx)),
                )))
                .child(setting_button(
                    "导入",
                    false,
                    cx.listener(|this, _event, _window, cx| this.open_server_import(cx)),
                ))
                .child(div().debug_selector(|| "server-export-button".into()).child(setting_button(
                    "导出",
                    false,
                    cx.listener(|this, _event, _window, cx| this.export_servers(cx)),
                ))),
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
        // 两行式条目：220px 面板放不下「长文本 + 三个按钮」单行排布（会被压到逐字换行）
        .children(servers.iter().map(|entry| {
            let auth_label = match entry.auth {
                ServerAuthKind::Password => "密码",
                ServerAuthKind::KeyboardInteractive => "交互",
                ServerAuthKind::PrivateKey => "密钥",
                ServerAuthKind::Agent => "Agent",
            };
            let pending_delete = delete_confirm.as_deref() == Some(entry.name.as_str());
            server_list_item()
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
                                .on_mouse_down(MouseButton::Left, cx.listener({
                                    let name = entry.name.clone();
                                    move |this, _event, _window, cx| this.connect_server(&name, cx)
                                }))
                                .child("连接"),
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
                        .child(format!(
                            "{}@{}:{} · {}",
                            entry.username, entry.host, entry.port, auth_label
                        )),
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
                                .on_mouse_down(MouseButton::Left, cx.listener({
                                    let entry = entry.clone();
                                    move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.edit_server(Some(&entry), window, cx);
                                        this.start_shortcut_recording(window, cx);
                                    }
                                }))
                                .child(sidebar_shortcut_label(entry.shortcut.as_deref()).to_string()),
                        )
                        .child(
                            div()
                                .debug_selector(|| "server-edit".into())
                                .text_size(px(11.0))
                                .text_color(rgb(0xA9B1D6))
                                .hover(|style| style.text_color(rgb(0xC0D6FF)))
                                .on_mouse_down(MouseButton::Left, cx.listener({
                                    let entry = entry.clone();
                                    move |this, _event, window, cx| this.edit_server(Some(&entry), window, cx)
                                }))
                                .child("编辑"),
                        )
                        .child(
                            div()
                                .debug_selector(|| "server-delete".into())
                                .text_size(px(11.0))
                                .text_color(rgb(if pending_delete { 0xF7768E } else { 0xA9B1D6 }))
                                .hover(|style| style.text_color(rgb(0xF7768E)))
                                .on_mouse_down(MouseButton::Left, cx.listener({
                                    let name = entry.name.clone();
                                    move |this, _event, _window, cx| this.delete_server(&name, cx)
                                }))
                                .child(if pending_delete { "确认删除" } else { "删除" }),
                        ),
                )
        }))
}

fn settings_overlay(
    scrim_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    close_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    navigation: gpui::Div,
    content: gpui::Stateful<gpui::Div>,
) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .left_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.6))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Left, scrim_listener)
        .child(
            div()
                .w(px(640.0))
                .h(px(560.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded_md()
                .bg(rgb(0x1C1E2C))
                .border_1()
                .border_color(rgb(0x343A50))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .h(px(52.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_5()
                        .border_b_1()
                        .border_color(rgb(0x343750))
                        .child(div().text_color(rgb(0xC0CAF5)).child("设置"))
                        .child(
                            div()
                                .h(px(30.0))
                                .w(px(30.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(0xF7768E))
                                .on_mouse_down(MouseButton::Left, close_listener)
                                .child("×"),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .flex()
                        .child(navigation)
                        .child(content),
                )
                .child(
                    div()
                        .h(px(44.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_5()
                        .border_t_1()
                        .border_color(rgb(0x343750))
                        .text_color(rgb(0x7D829F))
                        .child("修改即时生效")
                        .child("LaterTerm"),
                ),
        )
}

fn settings_navigation(page: SettingsPage, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let entries = [
        (SettingsPage::Appearance, "外观"),
        (SettingsPage::Shortcuts, "快捷键"),
        (SettingsPage::Terminal, "终端"),
        (SettingsPage::Servers, "服务器"),
    ];
    div()
        .w(px(150.0))
        .h_full()
        .p_3()
        .bg(rgb(0x1B1C2A))
        .flex()
        .flex_col()
        .gap_1()
        .children(entries.into_iter().map(|(entry, label)| {
            let selected = entry == page;
            div()
                .w_full()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(if selected {
                    rgb(0x35415B)
                } else {
                    rgb(0x1B1C2A)
                })
                .text_color(if selected {
                    rgb(0xD6E3FF)
                } else {
                    rgb(0xA9B1D6)
                })
                .hover(|style| style.bg(if selected { rgb(0x35415B) } else { rgb(0x292D40) }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, window, cx| {
                        this.select_settings_page(entry, event, window, cx)
                    }),
                )
                .child(label)
        }))
}

fn settings_content(
    page: SettingsPage,
    workspace: &mut WorkspaceView,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Stateful<gpui::Div> {
    let title = match page {
        SettingsPage::Appearance => "外观",
        SettingsPage::Shortcuts => "快捷键",
        SettingsPage::Terminal => "终端",
        SettingsPage::Servers => "服务器",
    };
    let body = match page {
        SettingsPage::Appearance => settings_panel(workspace, cx),
        SettingsPage::Shortcuts => shortcuts_page(workspace, cx),
        SettingsPage::Terminal => div()
            .pt_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("本地会话"))
            .child(
                setting_card()
                    .child(info_row("Shell", "$SHELL / bash"))
                    .child(info_row("TERM", "xterm-256color"))
                    .child(info_row("COLORTERM", "truecolor"))
                    .child(info_row("滚动", "自然滚动，支持历史回看")),
            ),
        SettingsPage::Servers => div()
            .pt_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("连接"))
            .child(
                setting_card()
                    .child(info_row("本地 Shell", "已内置"))
                    .child(info_row("SSH", "远程模块接入后可用")),
            ),
    };
    div()
        .id(gpui::SharedString::from(format!(
            "settings-content-{page:?}"
        )))
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .h_full()
        .overflow_y_scroll()
        .p_5()
        .text_color(rgb(0xC0CAF5))
        .child(div().text_color(rgb(0xC0CAF5)).child(title))
        .child(body)
}

/// 设置→快捷键：全量绑定总览（内置 + 命令 + 服务器 + 动态码），冲突标红。
fn shortcuts_page(workspace: &WorkspaceView, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let bindings = workspace.all_shortcut_bindings();
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for binding in &bindings {
        *counts.entry(binding.normalized.as_str()).or_default() += 1;
    }
    let row = |binding: &ShortcutBinding| {
        let conflicted = counts
            .get(binding.normalized.as_str())
            .copied()
            .unwrap_or(1)
            > 1;
        setting_card().child(shortcut_row(
            &binding.label,
            &binding.normalized,
            conflicted,
        ))
    };
    let mut page = div().pt_4().flex().flex_col().gap_3();
    page = page.child(section_title("内置"));
    for binding in bindings
        .iter()
        .filter(|b| matches!(b.owner, BindingOwner::Builtin))
    {
        page = page.child(row(binding));
    }
    page = page.child(section_title("面板"));
    for (label, shortcut, owner) in [
        (
            "命令面板",
            workspace.settings.shortcuts.commands_panel.as_deref(),
            BindingOwner::CommandsPanel,
        ),
        (
            "服务器面板",
            workspace.settings.shortcuts.servers_panel.as_deref(),
            BindingOwner::ServersPanel,
        ),
    ] {
        let invalid = shortcut.is_some_and(|key| normalize_shortcut(key).is_err());
        let conflicted = shortcut
            .and_then(|key| normalize_shortcut(key).ok())
            .is_some_and(|key| counts.get(key.as_str()).copied().unwrap_or(0) > 1);
        page = page.child(
            setting_card()
                .child(shortcut_row(
                    label,
                    shortcut.unwrap_or("未设置"),
                    invalid || conflicted,
                ))
                .child(setting_button(
                    "编辑快捷键",
                    false,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.edit_panel_shortcut(owner.clone(), window, cx);
                    }),
                )),
        );
    }
    // 直接从配置渲染，保留未绑定和无效历史值的修复入口。
    page = page.child(section_title("命令"));
    for command in &workspace.settings.commands {
        let command = command.clone();
        let shortcut = command.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = command
            .shortcut
            .as_ref()
            .is_some_and(|key| normalize_shortcut(key).is_err());
        let conflicted = normalize_shortcut(shortcut)
            .ok()
            .is_some_and(|key| counts.get(key.as_str()).copied().unwrap_or(0) > 1);
        page = page.child(
            setting_card()
                .child(shortcut_row(&command.name, shortcut, invalid || conflicted))
                .child(setting_button(
                    "编辑快捷键",
                    false,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.edit_command(Some(command.clone()), window, cx);
                        this.start_shortcut_recording(window, cx);
                    }),
                )),
        );
    }
    page = page.child(section_title("服务器"));
    for entry in &workspace.settings.servers {
        let entry = entry.clone();
        let shortcut = entry.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = entry
            .shortcut
            .as_ref()
            .is_some_and(|key| normalize_shortcut(key).is_err());
        let conflicted = normalize_shortcut(shortcut)
            .ok()
            .is_some_and(|key| counts.get(key.as_str()).copied().unwrap_or(0) > 1);
        page = page.child(
            setting_card()
                .child(shortcut_row(&entry.name, shortcut, invalid || conflicted))
                .child(setting_button(
                    "编辑快捷键",
                    false,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.edit_server(Some(&entry), window, cx);
                        this.start_shortcut_recording(window, cx);
                    }),
                )),
        );
    }
    page = page.child(section_title("动态码"));
    for entry in &workspace.settings.totp {
        let entry = entry.clone();
        let shortcut = entry.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = entry
            .shortcut
            .as_ref()
            .is_some_and(|key| normalize_shortcut(key).is_err());
        let conflicted = normalize_shortcut(shortcut)
            .ok()
            .is_some_and(|key| counts.get(key.as_str()).copied().unwrap_or(0) > 1);
        page = page.child(
            setting_card()
                .child(shortcut_row(&entry.name, shortcut, invalid || conflicted))
                .child(setting_button(
                    "编辑快捷键",
                    false,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.edit_totp(Some(&entry), window, cx);
                        this.start_shortcut_recording(window, cx);
                    }),
                )),
        );
    }
    let has_conflict = counts.values().any(|count| *count > 1);
    let invalid = workspace
        .settings
        .commands
        .iter()
        .filter_map(|entry| {
            entry
                .shortcut
                .as_ref()
                .map(|key| (format!("命令 {}", entry.name), key))
        })
        .chain(workspace.settings.servers.iter().filter_map(|entry| {
            entry
                .shortcut
                .as_ref()
                .map(|key| (format!("服务器 {}", entry.name), key))
        }))
        .chain(workspace.settings.totp.iter().filter_map(|entry| {
            entry
                .shortcut
                .as_ref()
                .map(|key| (format!("动态码 {}", entry.name), key))
        }))
        .chain(
            workspace
                .settings
                .shortcuts
                .commands_panel
                .as_ref()
                .map(|key| ("命令面板".to_string(), key)),
        )
        .chain(
            workspace
                .settings
                .shortcuts
                .servers_panel
                .as_ref()
                .map(|key| ("服务器面板".to_string(), key)),
        )
        .filter(|(_, key)| normalize_shortcut(key).is_err())
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        page = page.child(section_title("无效快捷键"));
        for (label, key) in &invalid {
            page = page.child(setting_card().child(shortcut_row(label, key, true)));
        }
    }
    page.child(
        div()
            .pt_2()
            .text_color(if has_conflict || !invalid.is_empty() {
                rgb(0xF7768E)
            } else {
                rgb(0x565A78)
            })
            .child(if has_conflict || !invalid.is_empty() {
                "存在冲突或无效绑定：请在对应编辑弹窗里修改（方向切换优先）"
            } else {
                "绑定在命令 / 服务器 / 动态码的编辑弹窗里设置，保存时自动查重"
            }),
    )
}

fn settings_panel(workspace: &mut WorkspaceView, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let settings = workspace.settings.appearance.clone();
    div()
        .pt_4()
        .flex()
        .flex_col()
        .gap_3()
        .child(section_title("终端外观"))
        .child(
            setting_card()
                .child(setting_row(
                    "字号",
                    value_chip(&format!("{:.0} px", settings.font_size)),
                    stepper_controls(
                        cx,
                        |s: &mut Appearance| s.font_size = (s.font_size - 1.0).max(8.0),
                        |s: &mut Appearance| s.font_size = (s.font_size + 1.0).min(32.0),
                    ),
                ))
                .child(setting_row(
                    "行高",
                    value_chip(&format!("{:.1}", settings.line_height)),
                    stepper_controls(
                        cx,
                        |s: &mut Appearance| {
                            s.line_height = ((s.line_height - 0.1) * 10.0).round() / 10.0;
                            s.line_height = s.line_height.clamp(0.8, 2.0);
                        },
                        |s: &mut Appearance| {
                            s.line_height = ((s.line_height + 0.1) * 10.0).round() / 10.0;
                            s.line_height = s.line_height.clamp(0.8, 2.0);
                        },
                    ),
                ))
                .child(setting_row(
                    "内边距",
                    value_chip(&format!("{:.0} px", settings.padding)),
                    stepper_controls(
                        cx,
                        |s: &mut Appearance| s.padding = (s.padding - 1.0).max(0.0),
                        |s: &mut Appearance| s.padding = (s.padding + 1.0).min(24.0),
                    ),
                )),
        )
        .child(section_title("字体"))
        .child(
            setting_card()
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .py_2()
                        .child(
                            div()
                                .w(px(64.0))
                                .pt_1()
                                .text_color(rgb(0xA9B1D6))
                                .child("字体"),
                        )
                        .child(font_family_controls(cx, &settings.font_family)),
                )
                .child(setting_row(
                    "连字",
                    div(),
                    toggle_controls(
                        cx,
                        ("开", true),
                        ("关", false),
                        settings.ligatures,
                        |s: &mut Appearance, value| s.ligatures = value,
                    ),
                ))
                .child(setting_row(
                    "光标闪烁",
                    div(),
                    toggle_controls(
                        cx,
                        ("开", true),
                        ("关", false),
                        settings.cursor_blink,
                        |s: &mut Appearance, value| s.cursor_blink = value,
                    ),
                ))
                .child(setting_row(
                    "平滑滚动",
                    div(),
                    toggle_controls(
                        cx,
                        ("开", true),
                        ("关", false),
                        settings.smooth_scrolling,
                        |s: &mut Appearance, value| s.smooth_scrolling = value,
                    ),
                ))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .py_2()
                        .child(
                            div()
                                .w(px(64.0))
                                .pt_1()
                                .text_color(rgb(0xA9B1D6))
                                .child("备选"),
                        )
                        .child(font_fallback_controls(cx, &settings.font_fallbacks)),
                ),
        )
        .child(section_title("滚动与主题"))
        .child(
            setting_card()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_2()
                        .child(div().w(px(64.0)).text_color(rgb(0xA9B1D6)).child("回滚"))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(scrollback_controls(cx, &settings)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_2()
                        .child(div().w(px(64.0)).text_color(rgb(0xA9B1D6)).child("主题"))
                        .child(theme_controls(cx, &workspace.themes, &settings.theme)),
                ),
        )
        .child(setting_button(
            "恢复默认",
            false,
            cx.listener(WorkspaceView::reset_settings),
        ))
}

fn stepper_controls(
    cx: &mut Context<WorkspaceView>,
    decrease: impl Fn(&mut Appearance) + Clone + 'static,
    increase: impl Fn(&mut Appearance) + Clone + 'static,
) -> gpui::Div {
    div()
        .flex()
        .gap_1()
        .child(setting_button(
            "−",
            false,
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, decrease.clone());
            }),
        ))
        .child(setting_button(
            "+",
            false,
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, increase.clone());
            }),
        ))
}

fn scrollback_controls(cx: &mut Context<WorkspaceView>, settings: &Appearance) -> gpui::Div {
    div().flex().gap_1().children(
        [(10_000usize, "10k"), (50_000, "50k"), (100_000, "100k")]
            .into_iter()
            .map(|(value, label)| {
                let selected = settings.scrollback == value;
                setting_button(
                    label,
                    selected,
                    cx.listener(move |this, event, window, cx| {
                        this.change_setting(event, window, cx, move |s| s.scrollback = value);
                    }),
                )
            }),
    )
}

/// 常用等宽字体预设（fontconfig 解析不到的名称会自然回退系统默认）。
const FONT_PRESETS: [&str; 6] = [
    "JetBrainsMono Nerd Font",
    "CaskaydiaCove Nerd Font",
    "FiraCode Nerd Font",
    "Sarasa Mono SC",
    "Noto Sans Mono CJK SC",
    "monospace",
];

/// 常用 CJK 备选字体预设；空表表示交回系统默认回退。
const FALLBACK_PRESETS: [&str; 3] = ["Sarasa Mono SC", "Noto Sans Mono CJK SC", "无"];

fn font_family_controls(cx: &mut Context<WorkspaceView>, current: &str) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(FONT_PRESETS.into_iter().map(|name| {
            let selected = name == current;
            setting_button(
                name,
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.change_setting(event, window, cx, |s| s.font_family = name.to_string());
                }),
            )
        }))
}

fn font_fallback_controls(cx: &mut Context<WorkspaceView>, current: &[String]) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(FALLBACK_PRESETS.into_iter().map(|name| {
            let selected = if name == "无" {
                current.is_empty()
            } else {
                current.iter().any(|entry| entry == name)
            };
            setting_button(
                name,
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.change_setting(event, window, cx, |s| {
                        if name == "无" {
                            s.font_fallbacks.clear();
                        } else if let Some(position) =
                            s.font_fallbacks.iter().position(|entry| entry == name)
                        {
                            s.font_fallbacks.remove(position);
                        } else {
                            s.font_fallbacks.push(name.to_string());
                        }
                    });
                }),
            )
        }))
}

/// 二值开关控件（选中侧高亮），如连字开/关。
fn toggle_controls(
    cx: &mut Context<WorkspaceView>,
    on: (&'static str, bool),
    off: (&'static str, bool),
    current: bool,
    apply: impl Fn(&mut Appearance, bool) + Clone + 'static,
) -> gpui::Div {
    div()
        .flex()
        .gap_1()
        .children(
            [(on, true), (off, false)]
                .into_iter()
                .map(|((label, value), _)| {
                    let selected = value == current;
                    let apply = apply.clone();
                    setting_button(
                        label,
                        selected,
                        cx.listener(move |this, event, window, cx| {
                            this.change_setting(event, window, cx, |s| apply(s, value));
                        }),
                    )
                }),
        )
}

fn theme_controls(
    cx: &mut Context<WorkspaceView>,
    themes: &ThemeRegistry,
    current: &str,
) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(themes.names().into_iter().map(|name| {
            let selected = name == current;
            setting_button(
                name.clone(),
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.change_setting(event, window, cx, |s| s.theme = name.clone());
                }),
            )
        }))
}

fn shortcut_row(label: &str, keys: &str, conflicted: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(if conflicted {
            rgb(0xF7768E)
        } else {
            rgb(0xA9B1D6)
        })
        .child(format!(
            "{}{}",
            label,
            if conflicted { " ⚠ 冲突" } else { "" }
        ))
        .child(value_chip(keys))
}

fn info_row(label: &str, value: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(rgb(0xA9B1D6))
        .child(label.to_string())
        .child(value.to_string())
}

fn section_title(text: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(3.0)).h(px(14.0)).bg(rgb(0x7AA2F7)).rounded_sm())
        .child(div().text_color(rgb(0xC0CAF5)).child(text.to_string()))
}

fn setting_card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .p_3()
        .gap_1()
        .bg(rgb(0x252939))
        .border_1()
        .border_color(rgb(0x303548))
        .rounded_md()
}

fn setting_row(label: &str, chip: gpui::Div, controls: gpui::Div) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_2()
        .child(
            div()
                .w(px(64.0))
                .text_color(rgb(0xA9B1D6))
                .child(label.to_string()),
        )
        .child(chip)
        .child(controls)
}

fn value_chip(text: &str) -> gpui::Div {
    div()
        .min_w(px(64.0))
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .bg(rgb(0x1A1B26))
        .border_1()
        .border_color(rgb(0x343750))
        .rounded_sm()
        .text_color(rgb(0xC0CAF5))
        .child(text.to_string())
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

fn setting_button(
    label: impl Into<gpui::SharedString>,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let bg = if selected {
        rgb(0x2E4A6B)
    } else {
        rgb(0x252637)
    };
    let text = if selected {
        rgb(0x7DCEFF)
    } else {
        rgb(0xA9B1D6)
    };
    div()
        .h(px(30.0))
        .min_w(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .bg(bg)
        .border_1()
        .border_color(if selected {
            rgb(0x7AA2F7)
        } else {
            rgb(0x343750)
        })
        .rounded_sm()
        .text_color(text)
        .hover(|this| this.bg(rgb(0x303150)))
        .on_mouse_down(MouseButton::Left, listener)
        .child(label.into())
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
) -> (Entity<TerminalView>, Arc<SessionControl>) {
    // 会话的开启/spawn/回收线程全部内聚在 LocalSession（laterterm-local）。
    // PTY 初始尺寸直接用窗口实际尺寸：避免启动后 resize 触发 bash readline
    // 的 \r 重画，把光标拉回提示符行首。
    let shell = laterterm_local::default_shell();
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
fn spawn_local_session() -> Arc<dyn TerminalSession> {
    let shell = laterterm_local::default_shell();
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
        smooth_scrolling: settings.smooth_scrolling,
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

#[cfg(test)]
mod exit_regression_tests {
    use super::{
        PaneDirection, SplitDirection, WorkspaceView, config_event_requires_reload,
        keystroke_matches_shortcut, pane_direction_for_keystroke,
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
        let active = cx.debug_bounds("sidebar-active-tab").expect("活动标签必须存在");
        assert!(active.top() >= list.top() && active.bottom() <= list.bottom(), "新建的活动标签应滚入可视区域");
        workspace.update_in(cx, |view, window, cx| {
            let first = view.tabs[0].focused_pane;
            view.focus_pane(first, window, cx);
        });
        cx.run_until_parked();
        let list = cx.debug_bounds("sidebar-tabs").unwrap();
        let active = cx.debug_bounds("sidebar-active-tab").unwrap();
        assert!(active.top() >= list.top() && active.bottom() <= list.bottom(), "切换至顶部标签时应滚入可视区域");
        workspace.update_in(cx, |view, window, cx| {
            view.tab_scroll.set_offset(gpui::point(gpui::px(0.0), -view.tab_scroll.max_offset().height));
            view.toggle_left_panel(&MouseDownEvent::default(), window, cx);
            view.toggle_left_panel(&MouseDownEvent::default(), window, cx);
        });
        cx.run_until_parked();
        let list = cx.debug_bounds("sidebar-tabs").unwrap();
        let active = cx.debug_bounds("sidebar-active-tab").unwrap();
        assert!(active.top() >= list.top() && active.bottom() <= list.bottom(), "重新打开侧栏应显示活动标签");
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
        let new_button = cx.debug_bounds("server-new-button").expect("新建按钮应可见");
        let export = cx.debug_bounds("server-export-button").expect("导出按钮应可见");
        assert!(new_button.right() <= export.left(), "服务器工具栏按钮不可重叠");
        let address = cx.debug_bounds("server-address").expect("服务器地址应可见");
        let shortcut = cx.debug_bounds("server-shortcut").expect("快捷键入口应可见");
        assert!(address.bottom() <= shortcut.top(), "服务器地址应排在快捷键之前");
        let edit = cx.debug_bounds("server-edit").expect("编辑按钮应可见");
        let delete = cx.debug_bounds("server-delete").expect("删除按钮应可见");
        assert_eq!(shortcut.top(), edit.top(), "次要操作应在同一行");
        assert_eq!(edit.top(), delete.top(), "编辑和删除应在同一行");
        workspace.read_with(cx, |view, _| {
            for pane in &view.panes { pane.session.close(); }
        });
    }

    #[test]
    fn sidebar_shortcut_label_remains_concise_and_identifiable() {
        assert_eq!(super::sidebar_shortcut_label(None), "设置快捷键");
        assert_eq!(super::sidebar_shortcut_label(Some("ctrl+shift+k")), "ctrl+shift+k");
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
            view.close_tab(tab, &MouseDownEvent::default(), window, cx);
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
