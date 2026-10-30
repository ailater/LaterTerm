use crate::command_text_field::CommandTextField;
use gpui::{
    App, AppContext, AsyncApp, Context, Edges, Entity, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, StatefulInteractiveElement,
    Styled, WeakEntity, Window, div, prelude::FluentBuilder, px, rgb,
};
use laterterm_config::{
    Appearance, CommandTemplate, Settings, Theme, ThemeRegistry, app_config_dir,
};
use laterterm_core::TerminalSession;
use laterterm_local::{LocalOptions, LocalSession};
use laterterm_termview::{ColorPalette, TerminalConfig, TerminalView};
use notify::Watcher;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const TITLEBAR_HEIGHT: f32 = 34.0;
const PANEL_WIDTH: f32 = 220.0;

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
}

/// 标签内的分屏布局树，叶子是 PaneId。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LayoutNode {
    Leaf(PaneId),
    Split {
        direction: SplitDirection,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
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
    left_panel: bool,
    right_panel: bool,
    settings_open: bool,
    settings_page: SettingsPage,
    settings: Settings,
    themes: ThemeRegistry,
    shortcut_tx: flume::Sender<(PaneId, WorkspaceShortcut)>,
    shortcut_rx: flume::Receiver<(PaneId, WorkspaceShortcut)>,
    pending_shortcuts: Vec<(PaneId, WorkspaceShortcut)>,
    command_values: BTreeMap<String, String>,
    command_preview: Option<(String, bool)>,
    command_template: Option<CommandTemplate>,
    command_target: Option<PaneId>,
    command_fields: Vec<(String, Entity<CommandTextField>)>,
}

impl WorkspaceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let weak_workspace = cx.weak_entity();
        let settings = Settings::load_or_default();
        settings.ensure_settings_file();
        let themes = ThemeRegistry::load();
        let (shortcut_tx, shortcut_rx) = flume::unbounded();
        let first_pane = PaneId(1);
        let first_tab = TabId(1);
        let theme = themes.resolve(&settings.appearance.theme).clone();
        let (terminal, session) = create_local_terminal(
            first_pane,
            weak_workspace,
            cx,
            &settings.appearance,
            &theme,
            shortcut_tx.clone(),
        );
        let view = Self {
            settings,
            themes,
            shortcut_tx,
            shortcut_rx,
            pending_shortcuts: Vec::new(),
            command_values: BTreeMap::new(),
            command_preview: None,
            command_template: None,
            command_target: None,
            command_fields: Vec::new(),
            settings_page: SettingsPage::Appearance,
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
            left_panel: false,
            right_panel: false,
            settings_open: false,
        };
        Self::watch_config_files(cx);
        view.start_shortcut_pump(cx);
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
                self.active_tab = self.tabs[tab_index].id;
                self.tabs[tab_index].focused_pane = pane_id;
                self.handle_shortcut(shortcut, window, cx);
            }
        }
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
        }
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

    fn new_tab(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.spawn_tab(window, cx);
    }

    /// 新建标签：一个标签一个全新窗格与分屏树根。
    fn spawn_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane_id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let tab_id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let theme = self.themes.resolve(&self.settings.appearance.theme).clone();
        let (terminal, session) = create_local_terminal(
            pane_id,
            cx.weak_entity(),
            cx,
            &self.settings.appearance,
            &theme,
            self.shortcut_tx.clone(),
        );
        self.panes.push(Pane {
            id: pane_id,
            title: format!("终端 {}", pane_id.0),
            terminal,
            session,
            status: TabStatus::Running,
        });
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
        let pane_id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let theme = self.themes.resolve(&self.settings.appearance.theme).clone();
        let (terminal, session) = create_local_terminal(
            pane_id,
            cx.weak_entity(),
            cx,
            &self.settings.appearance,
            &theme,
            self.shortcut_tx.clone(),
        );
        self.panes.push(Pane {
            id: pane_id,
            title: format!("终端 {}", pane_id.0),
            terminal,
            session,
            status: TabStatus::Running,
        });
        let tab_index = self.active_tab_index();
        let replacement = LayoutNode::Split {
            direction,
            first: Box::new(LayoutNode::Leaf(old)),
            second: Box::new(LayoutNode::Leaf(pane_id)),
        };
        if !self.tabs[tab_index].layout.replace_leaf(old, replacement) {
            self.panes.pop();
            return;
        }
        self.focus_pane(pane_id, window, cx);
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
    /// 窗格实体随退出回调逐个清理；标签立即从标签栏消失。
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
        change(&mut self.settings.appearance);
        self.settings.save();
        self.apply_appearance(cx);
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
                if event.is_ok() {
                    let _ = event_tx.send(());
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

    fn open_command(
        &mut self,
        command: &CommandTemplate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_template = Some(command.clone());
        self.command_target = Some(self.focused_pane_id());
        self.command_fields.clear();
        self.command_values.clear();
        if let Ok(variables) = command.variables() {
            for variable in variables {
                let field = cx.new(|cx| CommandTextField::new(String::new(), variable.clone(), cx));
                self.command_fields.push((variable, field));
            }
        }
        if self.command_fields.is_empty() && !command.confirm {
            self.send_command_to_pane(
                self.command_target.unwrap_or(self.focused_pane_id()),
                command.template.clone(),
                window,
                cx,
            );
            self.command_template = None;
            self.command_target = None;
            return;
        }
        match command.expand(&self.command_values) {
            Ok(preview) => {
                self.command_preview =
                    Some((preview, command.confirm || !self.command_fields.is_empty()));
            }
            Err(_) => {
                // 参数未填齐时打开表单状态；不会向 PTY 写入半成品。
                self.command_preview = Some((
                    format!(
                        "参数：{}",
                        command.variables().unwrap_or_default().join(", ")
                    ),
                    true,
                ));
            }
        }
        cx.notify();
    }

    fn confirm_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_command_values(cx);
        let Some((command, _confirm)) = self.command_preview.take() else {
            return;
        };
        if command.starts_with("参数错误：") || command.starts_with("参数：") {
            return;
        }
        let target = self
            .command_target
            .take()
            .unwrap_or_else(|| self.focused_pane_id());
        self.send_command_to_pane(target, command, window, cx);
        self.command_template = None;
        self.command_fields.clear();
        cx.notify();
    }

    fn sync_command_values(&mut self, cx: &mut Context<Self>) {
        self.command_values = self
            .command_fields
            .iter()
            .map(|(name, field)| (name.clone(), field.read(cx).value().to_string()))
            .collect();
        if let Some(template) = &self.command_template {
            self.command_preview = Some(match template.expand(&self.command_values) {
                Ok(preview) => (preview, template.confirm || !self.command_fields.is_empty()),
                Err(error) => (format!("参数错误：{error}"), true),
            });
        }
    }

    fn cancel_command(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.command_preview = None;
        self.command_template = None;
        self.command_target = None;
        self.command_fields.clear();
        self.command_values.clear();
        cx.notify();
    }

    fn close_window(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        // 退出前向所有会话发送关闭请求；SessionControl.close 幂等，wait 线程负责回收子进程
        for pane in &self.panes {
            pane.session.close();
        }
        cx.quit();
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_terminal()
            .read(cx)
            .focus_handle()
            .focus(window);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.control
            && event.keystroke.modifiers.shift
            && key.eq_ignore_ascii_case("e")
        {
            self.split_focused(SplitDirection::Horizontal, window, cx);
            cx.stop_propagation();
        } else if event.keystroke.modifiers.control
            && event.keystroke.modifiers.shift
            && key.eq_ignore_ascii_case("o")
        {
            self.split_focused(SplitDirection::Vertical, window, cx);
            cx.stop_propagation();
        } else if event.keystroke.modifiers.control && event.keystroke.modifiers.shift && key == "["
        {
            self.cycle_focus(false, window, cx);
            cx.stop_propagation();
        } else if event.keystroke.modifiers.control && event.keystroke.modifiers.shift && key == "]"
        {
            self.cycle_focus(true, window, cx);
            cx.stop_propagation();
        } else if event.keystroke.modifiers.control && key.eq_ignore_ascii_case("t") {
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
        } else if event.keystroke.modifiers.control && key == "w" {
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
        let active_tab_id = self.active_tab;
        let active_layout = self.active_tab().layout.clone();
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
            .on_key_down(cx.listener(WorkspaceView::on_key_down))
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
                            .gap_1()
                            .px_2()
                            .child(button("☰", cx.listener(Self::toggle_left_panel)))
                            .child(div().text_color(rgb(0xC0CAF5)).child("LaterTerm")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .overflow_x_hidden()
                            .children(tab_labels.iter().map(|(id, title, count)| {
                                let id = *id;
                                let is_active = id == active_tab_id;
                                let label = if *count > 1 {
                                    format!("{title} ⊞{count}")
                                } else {
                                    title.clone()
                                };
                                div()
                                    .h_full()
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
                                    .child(
                                        div()
                                            .px_1()
                                            .text_color(rgb(0x7D829F))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, event, window, cx| {
                                                    this.close_tab(id, event, window, cx)
                                                }),
                                            )
                                            .child("×"),
                                    )
                            }))
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
                    .child(button("—", cx.listener(Self::minimize)))
                    .child(button("□", cx.listener(Self::zoom)))
                    .child(button_with_color(
                        "×",
                        0xF7768E,
                        cx.listener(Self::close_window),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .child(if left_panel {
                        div()
                            .w(px(PANEL_WIDTH))
                            .h_full()
                            .bg(rgb(0x202132))
                            .p_3()
                            .child(div().text_color(rgb(0xC0CAF5)).child("标签"))
                            .children(tab_labels.iter().map(|(id, title, _)| {
                                div()
                                    .py_1()
                                    .text_color(if *id == active_tab_id {
                                        rgb(0x7DCEFF)
                                    } else {
                                        rgb(0xA9B1D6)
                                    })
                                    .child(title.clone())
                            }))
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
                            .child(render_layout(&active_layout, &self.panes, focused_pane, cx)),
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
    }
}

fn render_layout(
    node: &LayoutNode,
    panes: &[Pane],
    focused: PaneId,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Div {
    match node {
        LayoutNode::Leaf(id) => {
            let Some(pane) = panes.iter().find(|pane| pane.id == *id) else {
                return div();
            };
            let close_id = *id;
            let mut pane_div = div()
                .relative()
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
                .on_key_down(cx.listener(WorkspaceView::on_key_down))
                .child(pane.terminal.clone())
                .child(
                    div()
                        .absolute()
                        .top_1()
                        .right_1()
                        .px_1()
                        .text_color(rgb(0x7D829F))
                        .bg(gpui::Hsla::from(rgb(0x202132)).opacity(0.85))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, window, cx| {
                                this.close_pane(close_id, window, cx)
                            }),
                        )
                        .child("×"),
                );
            if *id == focused {
                pane_div = pane_div.border_1().border_color(rgb(0x7AA2F7));
            }
            pane_div
        }
        LayoutNode::Split {
            direction,
            first,
            second,
        } => {
            let horizontal = matches!(direction, SplitDirection::Horizontal);
            let mut container = div().flex().flex_1().min_w(px(0.0)).min_h(px(0.0));
            if horizontal {
                container = container.flex_row();
            } else {
                container = container.flex_col();
            }
            container
                .child(render_layout(first, panes, focused, cx))
                .child(if horizontal {
                    div().w(px(1.0)).h_full().bg(rgb(0x343750))
                } else {
                    div().h(px(1.0)).w_full().bg(rgb(0x343750))
                })
                .child(render_layout(second, panes, focused, cx))
        }
    }
}

fn default_commands() -> Vec<CommandTemplate> {
    [
        ("htop", "htop"),
        ("tmux", "tmux"),
        ("vim", "vim"),
        ("清屏", "clear"),
    ]
    .into_iter()
    .map(|(name, template)| CommandTemplate {
        name: name.into(),
        template: template.into(),
        shortcut: None,
        confirm: false,
    })
    .collect()
}

fn quick_panel(workspace: &mut WorkspaceView, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let commands = if workspace.settings.commands.is_empty() {
        default_commands()
    } else {
        workspace.settings.commands.clone()
    };
    let preview = workspace.command_preview.clone();
    let fields = workspace.command_fields.clone();
    let live_preview = workspace.command_template.as_ref().map(|template| {
        let values = fields
            .iter()
            .map(|(name, field)| (name.clone(), field.read(cx).value().to_string()))
            .collect::<BTreeMap<_, _>>();
        (
            template.expand(&values),
            template.confirm || !fields.is_empty(),
        )
    });

    let mut panel = div()
        .w(px(PANEL_WIDTH))
        .h_full()
        .bg(rgb(0x202132))
        .p_3()
        .text_color(rgb(0xC0CAF5))
        .child(div().child("快捷命令"))
        .children(commands.into_iter().map(|command| {
            let label = if command.template == command.name {
                command.name.clone()
            } else {
                format!("{}  ·  {}", command.name, command.template)
            };
            div()
                .py_2()
                .text_color(rgb(0xA9B1D6))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| {
                        this.open_command(&command, window, cx);
                    }),
                )
                .child(label)
        }))
        .child(div().pt_4().child("快捷服务器"))
        .child(div().pt_2().text_color(rgb(0x7D829F)).child("本地 Shell"))
        .child(
            div()
                .pt_2()
                .text_color(rgb(0x565A78))
                .child("SSH 服务器将在 SSH 模块接入后可用"),
        );
    if let Some((command, confirm)) = preview {
        let (display_command, display_confirm) = live_preview
            .map(|(result, needs_confirm)| {
                (
                    result.unwrap_or_else(|error| format!("参数错误：{error}")),
                    needs_confirm,
                )
            })
            .unwrap_or((command, confirm));
        panel = panel.child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .bg(gpui::Hsla::from(rgb(0x161722)).opacity(0.98))
                .p_3()
                .child(div().text_color(rgb(0x7DCFFF)).child("命令预览"))
                .children(fields.into_iter().map(|(name, field)| {
                    div()
                        .pt_2()
                        .child(div().text_color(rgb(0x7D829F)).child(name))
                        .child(field)
                }))
                .child(
                    div()
                        .pt_3()
                        .text_color(rgb(0xC0CAF5))
                        .child(display_command),
                )
                .child(if display_confirm {
                    div()
                        .pt_4()
                        .flex()
                        .gap_1()
                        .child(setting_button(
                            "发送",
                            true,
                            cx.listener(|this, _event, window, cx| {
                                this.confirm_command(window, cx);
                            }),
                        ))
                        .child(setting_button(
                            "取消",
                            false,
                            cx.listener(WorkspaceView::cancel_command),
                        ))
                } else {
                    div()
                }),
        );
    }
    panel
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
        .on_mouse_down(MouseButton::Left, scrim_listener)
        .child(
            div()
                .w(px(640.0))
                .max_h(px(520.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(0x202132))
                .border_1()
                .border_color(rgb(0x3B3F5C))
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
                .bg(if selected {
                    rgb(0x303550)
                } else {
                    rgb(0x1B1C2A)
                })
                .text_color(if selected {
                    rgb(0x7DCEFF)
                } else {
                    rgb(0xA9B1D6)
                })
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
        SettingsPage::Shortcuts => div()
            .pt_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("标签"))
            .child(
                setting_card()
                    .child(shortcut_row("新建标签", "Ctrl+T"))
                    .child(shortcut_row("关闭标签", "Ctrl+W")),
            )
            .child(section_title("终端"))
            .child(
                setting_card()
                    .child(shortcut_row("搜索回滚", "Ctrl+Shift+F"))
                    .child(shortcut_row("下一个 / 上一个", "Enter / Shift+Enter")),
            ),
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

fn shortcut_row(label: &str, keys: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(rgb(0xA9B1D6))
        .child(label.to_string())
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
        .bg(rgb(0x252637))
        .border_1()
        .border_color(rgb(0x343750))
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
        .text_color(rgb(color))
        .on_mouse_down(MouseButton::Left, listener)
        .child(label)
}

fn create_local_terminal(
    id: PaneId,
    workspace: gpui::WeakEntity<WorkspaceView>,
    cx: &mut Context<WorkspaceView>,
    settings: &Appearance,
    theme: &Theme,
    shortcut_tx: flume::Sender<(PaneId, WorkspaceShortcut)>,
) -> (Entity<TerminalView>, Arc<SessionControl>) {
    // 会话的开启/spawn/回收线程全部内聚在 LocalSession（laterterm-local）
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let session: Arc<dyn TerminalSession> = Arc::new(
        LocalSession::spawn(LocalOptions {
            shell,
            args: vec![],
            env: vec![],
            cols: 110,
            rows: 30,
        })
        .expect("启动本地会话失败"),
    );
    let session_control = Arc::new(SessionControl::new(Arc::clone(&session)));

    let config = TerminalConfig {
        font_family: settings.font_family.clone(),
        font_size: px(settings.font_size),
        cols: 110,
        rows: 30,
        scrollback: settings.scrollback,
        line_height_multiplier: settings.line_height,
        padding: Edges::all(px(settings.padding)),
        colors: palette_for(theme),
        ligatures: settings.ligatures,
        font_fallbacks: settings.font_fallbacks.clone(),
    };

    let terminal = cx.new(|cx| {
        // resize 由视图默认回调转发给会话，无需在此另接
        let shortcut_tx = shortcut_tx.clone();
        TerminalView::new(Arc::clone(&session), config, cx)
            .with_key_handler(move |event| {
                let shortcut = if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key.eq_ignore_ascii_case("e")
                {
                    Some(WorkspaceShortcut::Split(SplitDirection::Horizontal))
                } else if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key.eq_ignore_ascii_case("o")
                {
                    Some(WorkspaceShortcut::Split(SplitDirection::Vertical))
                } else if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key == "["
                {
                    Some(WorkspaceShortcut::FocusPrevious)
                } else if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key == "]"
                {
                    Some(WorkspaceShortcut::FocusNext)
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
            .with_exit_callback(move |window, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.mark_exited(id, window, cx));
            })
    });
    (terminal, session_control)
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
