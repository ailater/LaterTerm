//! 设置面板 UI：右内容区单页连续滚动 + 左导航 scroll-spy 双向联动。
//!
//! 由 workspace.rs 迁入（原 settings_overlay / settings_navigation /
//! settings_content / shortcuts_page / settings_panel 与全部设置专用控件）。
//! 挂为 workspace 的**子模块**是评审硬伤1 的可见性策略：迁移函数需要访问
//! `WorkspaceView` 的私有字段与方法（settings / themes / quake_status /
//! all_shortcut_bindings / change_setting 等），Rust 可见性规则允许子孙模块
//! 看到祖先模块的私有项，`main.rs` 顶层平级模块做不到。
//!
//! 结构：右内容 = 单一滚动容器，直接子元素恰好等于 `SettingsSection::ALL`
//! 个分区块（gpui 把 child_bounds 按直接子元素记录，多包一层或多插尾部
//! 元素都会让 scroll_to_top_of_item 与 scroll-spy 整体错位一节）。「恢复默认」
//! 按钮挂在「滚动与主题」分区块内部（原外观页最后一个子元素），因此不会
//! 多出滚动容器的直接子元素。
//!
//! 快捷键录制链路不动：录制控件 shortcut_editor_row 只渲染于窗口级模态弹窗
//! （面板/命令/服务器/动态码编辑），不在设置面板内；本模块里的「编辑快捷键」
//! 按钮只是弹窗入口，原样平移。

use super::{BindingOwner, ShortcutBinding, WebDavOutcome, WorkspaceView, dialog_error_line};
use crate::command_text_field::CommandTextField;
use crate::onboarding::FONT_PRESETS;
use crate::update_check::{self, Outcome};
use crate::webdav_sync::{WebDavTaskKind, WebDavTaskState};
use gpui::{
    AnyElement, App, AppContext, Context, Div, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder, px, rgb,
};
use laterterm_config::theme::ThemeColors;
use laterterm_config::{Appearance, ReconnectSettings, TriggerRule};
use laterterm_ui::tokens::{H_CTRL, H_NAV};
use laterterm_ui::{
    ButtonKind, IconButtonSpec, UiPalette, icon_button, setting_switch, tint_hsla, ui_button,
};

// ---------------------------------------------------------------------------
// 色板口径（原 Tokyo Night 固定常量组已删，全量改走 UiPalette 参数）：
// 面板底/内凹底=terminal_bg（主题背景原色，TN 下与原 0x1A1B26 等值）、
// 导航底=base_bg（比面板深一档，TN 下 0x0E0E14 略深于原 0x16161E，有意）、
// 卡底=card_bg、描边/分隔线=border_strong（提亮档，约定④，有意提亮）、
// 次级/弱化文字=text_secondary/text_muted（TN 下 0x888CA0/0x414868，较原
// 0xA9B1D6/0x565A78 有意变暗，4K 走查项②）。若需回固定深色，仅需在
// workspace 调用侧传 `UiPalette::from_theme(&ThemeColors::tokyo_night())`。
// ---------------------------------------------------------------------------

/// 字体 / 备选芯片固定宽度：内容区两列对齐，避免按文字长短流式换行参差。
const FONT_CHIP_WIDTH: f32 = 184.0;

/// scroll-spy 判定线：视口顶部下探 64px 落在哪个分区内就高亮哪个分区。
pub(crate) const SCROLL_SPY_LEAD_PX: f32 = 64.0;
/// 内容超出视口不足该值视为「不超高」，跳过侦测（max_offset≈0 时
/// at_bottom 恒真会把高亮错拉到末节，见 LaterScreen settings_ui 同款门控）。
pub(crate) const SCROLL_SPY_EPSILON_PX: f32 = 2.0;

// ---------------------------------------------------------------------------
// 分区模型与 scroll-spy 纯函数
// ---------------------------------------------------------------------------

/// 设置分区：左导航顺序即右内容排布顺序（单页滚动锚点，不再是独立页签）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    /// 终端外观（原外观页「终端外观」组）。
    #[default]
    Appearance,
    /// 字体。
    Font,
    /// 滚动与主题。
    Scrolling,
    /// 快捷键（含 Linux Quake 卡）。
    Shortcuts,
    /// 终端。
    Terminal,
    /// 服务器。
    Servers,
    /// 配置同步（WebDAV 手动备份/合并，见 webdav_sync.rs）。
    ConfigSync,
    /// macOS 权限（辅助功能/监听事件/完全磁盘访问；仅 macOS 进导航，见
    /// ALL 的 cfg 与 settings_permissions_body）。
    Permissions,
    /// 关于（品牌卡 + 仓库/许可信息）。
    About,
    /// 检查更新（手动触发，见 update_check.rs）。
    UpdateCheck,
}

impl SettingsSection {
    /// 导航/内容顺序即滚动容器直接子元素顺序；label / anchor_title /
    /// debug_key 的 match 均不带通配分支，新增变体漏加任一处都会编译失败。
    /// 权限节仅 macOS 进导航（非 macOS 无需授权，Linux CI 断言也按 ALL 长度
    /// 动态对位）。
    #[cfg(target_os = "macos")]
    pub(crate) const ALL: [Self; 10] = [
        Self::Appearance,
        Self::Font,
        Self::Scrolling,
        Self::Shortcuts,
        Self::Terminal,
        Self::Servers,
        Self::ConfigSync,
        Self::Permissions,
        Self::About,
        Self::UpdateCheck,
    ];

    /// 非 macOS 的导航列表：无权限节。
    #[cfg(not(target_os = "macos"))]
    pub(crate) const ALL: [Self; 9] = [
        Self::Appearance,
        Self::Font,
        Self::Scrolling,
        Self::Shortcuts,
        Self::Terminal,
        Self::Servers,
        Self::ConfigSync,
        Self::About,
        Self::UpdateCheck,
    ];

    /// 左导航短标签。
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Font => "字体",
            Self::Scrolling => "滚动",
            Self::Shortcuts => "快捷键",
            Self::Terminal => "终端",
            Self::Servers => "服务器",
            Self::ConfigSync => "同步",
            Self::Permissions => "权限",
            Self::About => "关于",
            Self::UpdateCheck => "更新",
        }
    }

    /// 内容区分区锚点标题（外观三节沿用原分组名，语义不变）。
    fn anchor_title(self) -> &'static str {
        match self {
            Self::Appearance => "终端外观",
            Self::Font => "字体",
            Self::Scrolling => "滚动与主题",
            Self::Shortcuts => "快捷键",
            Self::Terminal => "终端",
            Self::Servers => "服务器",
            Self::ConfigSync => "配置同步",
            Self::Permissions => "macOS 权限",
            Self::About => "关于",
            Self::UpdateCheck => "检查更新",
        }
    }

    /// debug_selector 后缀（测试取界：`settings-section-<key>`）。
    fn debug_key(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::Font => "font",
            Self::Scrolling => "scrolling",
            Self::Shortcuts => "shortcuts",
            Self::Terminal => "terminal",
            Self::Servers => "servers",
            Self::ConfigSync => "config-sync",
            Self::Permissions => "permissions",
            Self::About => "about",
            Self::UpdateCheck => "update",
        }
    }

    /// 在 ALL 中的下标 = 滚动容器直接子元素下标（跳转对位用）。
    pub(crate) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0)
    }
}

/// 滚动侦测（scroll-spy）：视口顶部下探 `line` 像素的判定线落在哪个分区
/// 区间内，就高亮哪个分区；滚到最底时强制选中最后一节——矮分区永远碰
/// 不到判定线，不兜底的话末节永远高亮不了。`tops` 为各分区起点在内容
/// 坐标系里的 y（同一空间内单调递增即可，与绝对原点无关）。
/// 纯函数无 GPUI 运行时依赖，测试见 workspace_tests.rs。
pub(crate) fn settings_scroll_spy(
    tops: &[(SettingsSection, f32)],
    line: f32,
    at_bottom: bool,
) -> SettingsSection {
    let mut current = tops
        .first()
        .map_or(SettingsSection::default(), |&(section, _)| section);
    for &(section, top) in tops {
        if top <= line {
            current = section;
        }
    }
    if at_bottom && let Some(&(section, _)) = tops.last() {
        current = section;
    }
    current
}

// ---------------------------------------------------------------------------
// 主题色板预览（纯函数 + 数据，无 GPUI 依赖；UI 卡见 theme_preview_card）
// ---------------------------------------------------------------------------

/// 主题色板预览数据：背景/前景/光标三基色 + ANSI 8×2（普通行/亮色行）。
/// 纯数据无 GPUI 依赖，行序契约见 `palette_preview`，测试见 workspace_tests.rs。
pub(crate) struct PalettePreview {
    /// 主题背景色（示例终端区打底）。
    pub(crate) background: u32,
    /// 主题前景色（示例文字着色）。
    pub(crate) foreground: u32,
    /// 主题光标色（示例行内光标色块）。
    pub(crate) cursor: u32,
    /// ANSI 普通行：固定黑→白序（black/red/green/yellow/blue/magenta/cyan/white）。
    pub(crate) normal: [(&'static str, u32); 8],
    /// ANSI 亮色行：固定亮黑→亮白序（bright_black..bright_white）。
    pub(crate) bright: [(&'static str, u32); 8],
}

/// 从 `ThemeColors` 抽取预览数据：ANSI 行序固定黑→白、亮黑→亮白，逐字段
/// 拷贝不重排——映射只此一处，UI 与测试都以此为准，改行序必须连测试同改。
pub(crate) fn palette_preview(colors: &ThemeColors) -> PalettePreview {
    PalettePreview {
        background: colors.background,
        foreground: colors.foreground,
        cursor: colors.cursor,
        normal: [
            ("黑", colors.black),
            ("红", colors.red),
            ("绿", colors.green),
            ("黄", colors.yellow),
            ("蓝", colors.blue),
            ("紫", colors.magenta),
            ("青", colors.cyan),
            ("白", colors.white),
        ],
        bright: [
            ("亮黑", colors.bright_black),
            ("亮红", colors.bright_red),
            ("亮绿", colors.bright_green),
            ("亮黄", colors.bright_yellow),
            ("亮蓝", colors.bright_blue),
            ("亮紫", colors.bright_magenta),
            ("亮青", colors.bright_cyan),
            ("亮白", colors.bright_white),
        ],
    }
}

// ---------------------------------------------------------------------------
// 覆盖层与导航
// ---------------------------------------------------------------------------

/// 设置覆盖层：遮罩 + 居中面板（头部 52 / 导航+内容 / 底部 44，几何不变只换令牌）。
/// `ui` 由 workspace render 传入（随主题换肤）。
pub(crate) fn settings_overlay(
    ui: &UiPalette,
    scrim_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    close_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    navigation: Div,
    content: Stateful<Div>,
) -> Div {
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
                .bg(ui.terminal_bg)
                .border_1()
                .border_color(ui.border_strong)
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
                        .border_color(ui.divider)
                        .child(div().text_color(ui.text).child("设置"))
                        // 关闭钮统一图标钮（原文本「×」，danger 悬停警示）
                        .child(icon_button(
                            "settings-close",
                            "icons/close.svg",
                            ui,
                            IconButtonSpec {
                                size: 22.0,
                                reveal_on_group: None,
                                danger_hover: true,
                                disabled: false,
                            },
                            close_listener,
                        )),
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
                        .border_color(ui.divider)
                        .text_color(ui.text_muted)
                        .child(format!(
                            "修改即时生效 · 共 {} 节，上下滚动查看全部设置",
                            SettingsSection::ALL.len()
                        ))
                        .child("LaterTerm"),
                ),
        )
}

/// 左导航：当前分区指示 + 点击跳转（滚动跟随由 workspace 的 scroll-spy 驱动）。
pub(crate) fn settings_navigation(
    ui: &UiPalette,
    active: SettingsSection,
    cx: &mut Context<WorkspaceView>,
) -> Div {
    div()
        .w(px(150.0))
        .h_full()
        .p_3()
        .bg(ui.base_bg)
        // 与内容区的分界走 hairline：浅色系下不再是生硬的底色交界带
        .border_r_1()
        .border_color(ui.divider)
        .flex()
        .flex_col()
        .gap_1()
        .children(
            SettingsSection::ALL
                .into_iter()
                .map(|section| settings_nav_item(section, section == active, ui, cx)),
        )
}

/// 导航项：H_NAV（32px）行高 + 选中 accent tint 底 + 左侧 3px accent 指示条 +
/// 主文字色，未选中悬停中性提亮。（行高有意比 LaterScreen 参照 34px 紧凑，
/// 设置面板只有文字无图标，32px 足够点击目标。）
fn settings_nav_item(
    section: SettingsSection,
    selected: bool,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Div {
    let (bg, hover_bg, text) = if selected {
        (
            tint_hsla(ui.accent, 0x33),
            tint_hsla(ui.accent, 0x40),
            ui.text,
        )
    } else {
        (ui.base_bg, ui.hover_tint, ui.text_secondary)
    };
    div()
        .debug_selector(move || {
            if selected {
                "settings-nav-active".into()
            } else {
                "settings-nav-item".into()
            }
        })
        .w_full()
        .h(px(H_NAV))
        .relative()
        .flex()
        .items_center()
        .px_3()
        .rounded_sm()
        .bg(bg)
        .hover(move |style| style.bg(hover_bg))
        .text_color(text)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event, window, cx| {
                this.select_settings_section(section, event, window, cx)
            }),
        )
        // 左侧 accent 指示条：让「当前在哪一节」不依赖底色也能读出来
        .when(selected, |item| {
            item.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(6.0))
                    .bottom(px(6.0))
                    .w(px(3.0))
                    .rounded_full()
                    .bg(ui.accent),
            )
        })
        .child(section.label())
}

// ---------------------------------------------------------------------------
// 单页滚动内容
// ---------------------------------------------------------------------------

/// 右内容：全部分区连续排布在同一滚动页里（VS Code 设置式）。children 恰好
/// = `SettingsSection::ALL.len()` 个分区块——滚动容器的 child_bounds 按直接
/// 子元素记录，索引对位是 scroll_to_top_of_item 与 scroll-spy 的硬约束。
pub(crate) fn settings_content(
    workspace: &mut WorkspaceView,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Stateful<Div> {
    let appearance = workspace.settings.appearance.clone();
    let mut children = Vec::with_capacity(SettingsSection::ALL.len());
    for (index, section) in SettingsSection::ALL.iter().enumerate() {
        let body: Vec<AnyElement> = match section {
            SettingsSection::Appearance => settings_appearance_body(&appearance, ui, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            SettingsSection::Font => settings_font_body(&appearance, ui, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            SettingsSection::Scrolling => settings_scrolling_body(workspace, &appearance, ui, cx),
            SettingsSection::Shortcuts => settings_shortcuts_body(workspace, ui, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            SettingsSection::Terminal => settings_terminal_body(workspace, &appearance, ui, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            SettingsSection::Servers => {
                settings_servers_body(&workspace.settings.reconnect, ui, cx)
                    .into_iter()
                    .map(IntoElement::into_any_element)
                    .collect()
            }
            SettingsSection::ConfigSync => settings_config_sync_body(workspace, ui, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            SettingsSection::Permissions => {
                // 非 macOS 不进 ALL，此臂恒不渲染；变体存在故 match 仍需覆盖
                #[cfg(target_os = "macos")]
                {
                    settings_permissions_body(workspace, ui, cx)
                        .into_iter()
                        .map(IntoElement::into_any_element)
                        .collect()
                }
                #[cfg(not(target_os = "macos"))]
                {
                    Vec::new()
                }
            }
            SettingsSection::About => settings_about_body(ui, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
            SettingsSection::UpdateCheck => settings_update_body(workspace, ui, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect(),
        };
        let block = div()
            .debug_selector(move || format!("settings-section-{}", section.debug_key()))
            // 单页滚动里相邻节容易读成嵌套（评审反馈「外观面板里出现字体
            // 区块」即此观感）：非首节加通宽分隔线 + 上留白加倍
            .when(index > 0, |block| {
                block.border_t_1().border_color(ui.divider)
            })
            .pt_4()
            .pb_5()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title(ui, section.anchor_title()))
            .children(body);
        children.push(block);
    }
    div()
        .id("settings-content")
        .debug_selector(|| "settings-content".into())
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .overflow_y_scroll()
        .track_scroll(&workspace.settings_scroll)
        .flex()
        .flex_col()
        .text_color(ui.text)
        .children(children)
}

/// 「终端外观」卡：字号 / 行高 / 内边距 / 背景不透明度 / 背景模糊。
fn settings_appearance_body(
    appearance: &Appearance,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    // Linux 下的透明降级提示会追加到 card；其他平台无可变消费方。
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut card = setting_card(ui)
        .child(setting_row(
            ui,
            "字号",
            value_chip(ui, &format!("{:.0} px", appearance.font_size)),
            stepper_controls(
                ui,
                cx,
                "font-size",
                |s: &mut Appearance| s.font_size = (s.font_size - 1.0).max(8.0),
                |s: &mut Appearance| s.font_size = (s.font_size + 1.0).min(32.0),
            ),
        ))
        .child(setting_row(
            ui,
            "行高",
            value_chip(ui, &format!("{:.1}", appearance.line_height)),
            stepper_controls(
                ui,
                cx,
                "line-height",
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
            ui,
            "内边距",
            value_chip(ui, &format!("{:.0} px", appearance.padding)),
            stepper_controls(
                ui,
                cx,
                "padding",
                |s: &mut Appearance| s.padding = (s.padding - 1.0).max(0.0),
                |s: &mut Appearance| s.padding = (s.padding + 1.0).min(24.0),
            ),
        ))
        .child(setting_row(
            // ±5% 步进，两位小数取整防浮点尾巴；钳制与配置层 sanitize 同口径
            ui,
            "背景不透明度",
            value_chip(
                ui,
                &format!("{:.0}%", (appearance.background_opacity * 100.0).round()),
            ),
            stepper_controls(
                ui,
                cx,
                "background-opacity",
                |s: &mut Appearance| {
                    s.background_opacity = ((s.background_opacity - 0.05) * 100.0).round() / 100.0;
                    s.background_opacity = s.background_opacity.clamp(0.15, 1.0);
                },
                |s: &mut Appearance| {
                    s.background_opacity = ((s.background_opacity + 0.05) * 100.0).round() / 100.0;
                    s.background_opacity = s.background_opacity.clamp(0.15, 1.0);
                },
            ),
        ))
        .child(setting_row(
            ui,
            "背景模糊",
            div(),
            toggle_controls(
                ui,
                cx,
                "背景模糊",
                ("开", true),
                ("关", false),
                appearance.background_blur,
                |s: &mut Appearance, value| s.background_blur = value,
            ),
        ));
    // 降级提示（仅 Linux）：基于 gpui::guess_compositor() 的静态会话判定。
    // gpui 未暴露「blur 是否真实生效」的查询口（X11 无 blur 路径、Wayland
    // 非 KWin 合成器静默降级纯透明），无法运行时探测，只能静态提示兜底。
    #[cfg(target_os = "linux")]
    if let Some(hint) = transparency_hint(appearance) {
        card = card.child(
            div()
                .pt_1()
                .text_size(px(11.0))
                .text_color(ui.text_muted)
                .child(hint),
        );
    }
    vec![card]
}

/// 背景透明/模糊的降级提示（仅 Linux 编译；X11/Wayland 之外的会话类型
/// 如 Headless 不提示）。判定值与 gpui 选后端同源（platform.rs
/// guess_compositor，gpui.rs `pub use platform::*` 公开重导出），不自造
/// 镜像判定。o=1（完全不透明）时两平台都无提示。
#[cfg(target_os = "linux")]
fn transparency_hint(appearance: &Appearance) -> Option<&'static str> {
    let transparent = appearance.background_opacity < 1.0;
    match gpui::guess_compositor() {
        "X11" => {
            if appearance.background_blur && transparent {
                // X11 后端没有 blur 实现路径（Blurred 静默等同 Transparent）；
                // o=1 时窗口映射 Opaque，无透明可降级，不提示
                Some("X11 会话不支持背景模糊，将按纯透明显示")
            } else if transparent {
                Some("透明需运行中的合成器（picom/KWin 等），否则透明区域显示为黑色")
            } else {
                None
            }
        }
        "Wayland" => {
            if appearance.background_blur && transparent {
                Some("背景模糊需合成器支持 KDE blur 协议（KWin），不支持时按纯透明显示")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 「字体」卡：字体族 / 连字 / 光标闪烁 / 光标样式 / 平滑滚动 / 选中即复制 / 备选。
fn settings_font_body(
    appearance: &Appearance,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    vec![
        setting_card(ui)
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
                            .text_color(ui.text_secondary)
                            .child("字体"),
                    )
                    .child(font_family_controls(ui, cx, &appearance.font_family)),
            )
            .child(setting_row(
                ui,
                "连字",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "连字",
                    ("开", true),
                    ("关", false),
                    appearance.ligatures,
                    |s: &mut Appearance, value| s.ligatures = value,
                ),
            ))
            .child(setting_row(
                ui,
                "光标闪烁",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "光标闪烁",
                    ("开", true),
                    ("关", false),
                    appearance.cursor_blink,
                    |s: &mut Appearance, value| s.cursor_blink = value,
                ),
            ))
            .child(setting_row(
                ui,
                "光标样式",
                div(),
                cursor_style_controls(ui, cx, appearance.cursor_style),
            ))
            .child(setting_row(
                ui,
                "平滑滚动",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "平滑滚动",
                    ("开", true),
                    ("关", false),
                    appearance.smooth_scrolling,
                    |s: &mut Appearance, value| s.smooth_scrolling = value,
                ),
            ))
            .child(setting_row(
                // 拖拽/双击/三击出选区松开即复制（Deepin 同款），不只限拖拽
                ui,
                "选中即复制",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "选中即复制",
                    ("开", true),
                    ("关", false),
                    appearance.copy_on_select,
                    |s: &mut Appearance, value| s.copy_on_select = value,
                ),
            ))
            .child(setting_row(
                // 剪贴板内容含换行（含单行命令+尾回车）时先弹确认，防误粘贴即执行
                ui,
                "多行粘贴确认",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "多行粘贴确认",
                    ("开", true),
                    ("关", false),
                    appearance.confirm_multiline_paste,
                    |s: &mut Appearance, value| s.confirm_multiline_paste = value,
                ),
            ))
            .child(setting_row(
                // 关闭窗格/标签/窗口时若探测到会话内仍有前台或子进程在运行，
                // 先弹确认再执行关闭（防误杀 sleep/编辑器/后台 job）
                ui,
                "关闭运行中确认",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "关闭运行中确认",
                    ("开", true),
                    ("关", false),
                    appearance.confirm_close_running,
                    |s: &mut Appearance, value| s.confirm_close_running = value,
                ),
            ))
            .child(setting_row(
                // 复制成功后窗口右下角短暂显示「已复制 N 字符」
                ui,
                "复制提示",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "复制提示",
                    ("开", true),
                    ("关", false),
                    appearance.copy_toast,
                    |s: &mut Appearance, value| s.copy_toast = value,
                ),
            ))
            .child(setting_row(
                // 终端 bell 响铃时弹系统桌面通知；默认关（只视觉提示，测试不外发）
                ui,
                "响铃系统通知",
                div(),
                toggle_controls(
                    ui,
                    cx,
                    "响铃系统通知",
                    ("开", true),
                    ("关", false),
                    appearance.bell_notification,
                    |s: &mut Appearance, value| s.bell_notification = value,
                ),
            ))
            .child(setting_row(
                // bash 提示符集成（OSC 133/7）：经环境注入 PROMPT_COMMAND/PS0，只对新开的
                // 本地 bash 会话生效；best-effort——用户 .bashrc 重赋值这两个变量即静默失效
                //（zsh/fish 自装片段见 README）
                ui,
                "Shell 集成",
                div(),
                // 开关旁提示文案（行内追加，不占独立行）
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(toggle_controls(
                        ui,
                        cx,
                        "Shell 集成",
                        ("开", true),
                        ("关", false),
                        appearance.shell_integration,
                        |s: &mut Appearance, value| s.shell_integration = value,
                    ))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(ui.text_muted)
                            .child("仅 bash（提示符跳转/失败标记）"),
                    ),
            ))
            .child(setting_row(
                // kitty keyboard protocol（CSI > 1u / CSI ? u 协商）：默认关——手写编码面
                // 未经真实应用验证前不默认开启
                ui,
                "kitty 键盘协议",
                div(),
                // 开关旁提示文案：开启只是「允许」应用协商，应用经 ?u 查询
                // 自动启用（fish 4+/tmux 等会主动查）
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(toggle_controls(
                        ui,
                        cx,
                        "kitty 键盘协议",
                        ("开", true),
                        ("关", false),
                        appearance.kitty_keyboard,
                        |s: &mut Appearance, value| s.kitty_keyboard = value,
                    ))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(ui.text_muted)
                            .child("开启后应用经 ?u 查询自动启用"),
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
                            .text_color(ui.text_secondary)
                            .child("备选"),
                    )
                    .child(font_fallback_controls(ui, cx, &appearance.font_fallbacks)),
            ),
    ]
}

/// 「滚动与主题」卡 + 恢复默认按钮。按钮原是外观页最后一个子元素，单页化
/// 后挂在末个外观分区块内部——保持「恢复默认作用于外观设置」的语义与视觉
/// 位置，同时不增加滚动容器直接子元素数（六节对位约束）。
///
/// 主题行为预览语义：色板预览卡常驻（读生效主题），点芯片只改预览态；
/// 预览激活时在卡内追加提示行与「保存主题 / 还原」按钮。底栏「修改即时
/// 生效」对其余设置仍成立，主题的例外文案只出现在本卡内，不全局反复横跳。
fn settings_scrolling_body(
    workspace: &WorkspaceView,
    appearance: &Appearance,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<AnyElement> {
    // 生效主题唯一读口（预览优先）：芯片选中态、色板卡、窗格颜色三处同源。
    let effective = workspace.effective_theme_name().to_string();
    let preview = palette_preview(&workspace.themes.resolve(&effective).colors);
    let previewing = workspace.theme_preview.is_some();
    let mut theme_card = setting_card(ui)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .py_2()
                .child(
                    div()
                        .w(px(64.0))
                        .text_color(ui.text_secondary)
                        .child("回滚"),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(scrollback_controls(ui, cx, appearance)),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .py_2()
                .child(
                    div()
                        .w(px(64.0))
                        .text_color(ui.text_secondary)
                        .child("主题"),
                )
                .child(theme_controls(ui, cx, &workspace.themes, &effective)),
        )
        .child(theme_preview_card(&preview, ui));
    // 预览激活才出现的提交/放弃控件与提示行——全部塞进本分区块内部，
    // 不得新增滚动容器直接子元素（settings_content 六节对位硬约束）。
    if previewing {
        theme_card = theme_card
            .child(
                div()
                    .pt_1()
                    .text_size(px(11.0))
                    .text_color(ui.accent)
                    .child("预览中：保存后生效，关面板自动还原"),
            )
            .child(
                div()
                    .debug_selector(|| "theme-preview-actions".into())
                    .flex()
                    .gap_1()
                    .child(setting_button(
                        ui,
                        "保存主题",
                        true,
                        cx.listener(|this, event, window, cx| {
                            this.commit_theme_preview(event, window, cx)
                        }),
                    ))
                    .child(setting_button(
                        ui,
                        "还原",
                        false,
                        cx.listener(|this, _event, window, cx| {
                            cx.stop_propagation();
                            this.cancel_theme_preview(Some(window), cx);
                        }),
                    )),
            );
    }
    vec![
        theme_card.into_any_element(),
        setting_button(
            ui,
            "恢复默认",
            false,
            cx.listener(WorkspaceView::reset_settings),
        )
        .into_any_element(),
    ]
}

/// 主题色板预览卡：背景/前景/光标三色块 + ANSI 8×2 网格 + 示例终端文本，
/// 数据全部来自 `palette_preview`（生效主题；预览期间即预览主题）。中文名
/// 以小字标签直接排在色块下——gpui tooltip 需要 AnyView 工厂无字符串糖
/// （评审建议2），全仓尚无 .tooltip( 先例，不为此 UI 卡引入首例。
fn theme_preview_card(preview: &PalettePreview, ui: &UiPalette) -> Div {
    let color_chip = |name: &'static str, color: u32| {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .w(px(22.0))
                    .h(px(14.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(ui.border_strong)
                    .bg(rgb(color)),
            )
            .child(
                div()
                    .text_size(px(9.0))
                    .text_color(ui.text_muted)
                    .child(name),
            )
    };
    div()
        .debug_selector(|| "theme-preview-card".into())
        .mt_1()
        .p_2()
        .flex()
        .flex_col()
        .gap_2()
        .bg(ui.terminal_bg)
        .border_1()
        .border_color(ui.border_strong)
        .rounded_sm()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(color_chip("背景", preview.background))
                .child(color_chip("前景", preview.foreground))
                .child(color_chip("光标", preview.cursor))
                .child(
                    // 示例终端行：主题背景打底、前景着色、光标色块
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(rgb(preview.background))
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(preview.foreground))
                                .child("user@laterterm:~$ ls"),
                        )
                        .child(div().w(px(6.0)).h(px(12.0)).bg(rgb(preview.cursor))),
                ),
        )
        .child(
            div().flex().gap_1().children(
                preview
                    .normal
                    .iter()
                    .map(|(name, color)| color_chip(name, *color)),
            ),
        )
        .child(
            div().flex().gap_1().children(
                preview
                    .bright
                    .iter()
                    .map(|(name, color)| color_chip(name, *color)),
            ),
        )
}

/// 「终端」节：只读信息卡（语义不变，随单页平移）+ 会话日志卡
/// （全局默认开关 + 打开日志目录按钮）+ 触发器卡（CRUD）。
fn settings_terminal_body(
    workspace: &WorkspaceView,
    appearance: &Appearance,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    // 目录行如实显示解析结果（定位失败给提示而非假路径）；目录本体由审计
    // 工作线程在真正开启日志时惰性创建。
    let dir_text = crate::audit::logs_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|| "（无法定位状态目录）".into());
    // 下载目录行：空值提示默认路径（~/Downloads）；路径在会话装配时固化，
    // 改动只对新连接会话生效——文案如实注明，防「改了不生效」困惑。
    let downloads_text = if appearance.downloads_dir.is_empty() {
        "~/Downloads（未配置，默认）".to_string()
    } else {
        appearance.downloads_dir.clone()
    };
    vec![
        setting_card(ui)
            .child(info_row(ui, "Shell", "$SHELL / bash"))
            .child(info_row(ui, "TERM", "xterm-256color"))
            .child(info_row(ui, "COLORTERM", "truecolor"))
            .child(info_row(ui, "滚动", "自然滚动，支持历史回看"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .py_2()
                    .text_color(ui.text_secondary)
                    .child("下载目录")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(ui.text)
                                    .child(downloads_text),
                            )
                            .child(setting_button(
                                ui,
                                "选择目录",
                                false,
                                cx.listener(|this, event, window, cx| {
                                    this.browse_download_dir(event, window, cx);
                                }),
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(ui.text_muted)
                    .child("终端内传输（trz/sz/rz）的落盘位置，留空 = ~/Downloads；改动仅对新连接的会话生效（已连接会话不热切）"),
            ),
        // 会话日志卡：全局默认只约束「新会话初值」，运行中窗格在标签右键
        // 菜单逐标签热开/热关。默认关——PTY 输出含潜在敏感内容（回显型
        // 密码提示仍可能入盘），安全优先（change_setting 即时落盘生效）。
        setting_card(ui)
            .child(setting_row(
                ui,
                "会话日志",
                value_chip(ui, "新会话默认"),
                toggle_controls(
                    ui,
                    cx,
                    "会话日志",
                    ("开", true),
                    ("关", false),
                    appearance.session_logging,
                    |s: &mut Appearance, value| s.session_logging = value,
                ),
            ))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(ui.text_secondary)
                                    .child("逐会话开关：标签右键 → 会话日志"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(ui.text_muted)
                                    .child(dir_text),
                            ),
                    )
                    .child(setting_button(
                        ui,
                        "打开日志目录",
                        false,
                        // 后台线程文件管理器进程，失败只 eprintln 不阻塞 UI
                        |_, _, _| crate::audit::open_logs_dir(),
                    )),
            ),
        trigger_card(workspace, ui, cx),
    ]
}

/// 触发器卡：规则行（名称/模式/动作/启用开关/删除，点名称进编辑）+
/// 「新增」弹窗入口。保存/启停/删除即时落盘并热替换审计规则集。
fn trigger_card(workspace: &WorkspaceView, ui: &UiPalette, cx: &mut Context<WorkspaceView>) -> Div {
    let mut card = setting_card(ui).child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .py_1()
            .child(div().text_color(ui.text_secondary).child("输出触发器"))
            .child(setting_button(
                ui,
                "新增",
                false,
                cx.listener(|this, _event, window, cx| {
                    this.begin_trigger_edit(None, window, cx);
                }),
            )),
    );
    for rule in &workspace.settings.triggers {
        card = card.child(trigger_row(workspace, rule, ui, cx));
    }
    if workspace.settings.triggers.is_empty() {
        card = card.child(
            div()
                .text_size(px(11.0))
                .text_color(ui.text_muted)
                .child("暂无规则：输出命中正则时可发桌面通知或点亮标签"),
        );
    }
    // 卡级错误行：启停/删除失败的落盘错误（弹窗打开时错误在弹窗内显示，
    // 不在此重复）；command_error 先例的就地可见口径
    card.when(workspace.trigger_dialog.is_none(), |card| {
        card.when_some(workspace.trigger_error.clone(), |card, error| {
            card.child(dialog_error_line(ui, error))
        })
    })
}

/// 一条触发器规则行：名称（点击进编辑弹窗，CRUD 的 Update 入口）/ 模式 /
/// 动作 / 启用开关 / 删除（二次确认，命令删除同款）。
fn trigger_row(
    workspace: &WorkspaceView,
    rule: &TriggerRule,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Div {
    let name = rule.name.clone();
    let pattern = rule.pattern.clone();
    let action_label = rule.action.label();
    let draft = rule.clone(); // 监听器要求 'static：带原规则克隆进编辑弹窗
    let confirming = workspace.trigger_delete_confirm.as_deref() == Some(rule.name.as_str());
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        // 名称：点击编辑（begin_trigger_edit 带原规则进弹窗）
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(96.0))
                .overflow_hidden()
                .text_color(ui.text)
                .cursor_pointer()
                .hover(|style| style.text_color(ui.accent))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| {
                        this.begin_trigger_edit(Some(&draft), window, cx);
                    }),
                )
                .child(name),
        )
        // 模式：代码展示（截断防撑爆行）
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_size(px(11.0))
                .text_color(ui.text_secondary)
                .child(pattern),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(11.0))
                .text_color(ui.text_muted)
                .child(action_label),
        )
        // 启用开关：启停即落盘 + 规则集热替换（toggle_trigger）
        .child({
            let name = rule.name.clone();
            // 行级重复 label：id 按规则名拼唯一（见 setting_button_keyed）
            setting_button_keyed(
                ui,
                format!("trigger-{name}-toggle"),
                if rule.enabled {
                    "已启用"
                } else {
                    "已停用"
                },
                rule.enabled,
                cx.listener(move |this, _event, _window, cx| {
                    this.toggle_trigger(&name, cx);
                }),
            )
        })
        // 删除：二次确认（command_delete_confirm 同款，首击变「确认删除」）
        .child({
            let name = rule.name.clone();
            setting_button_keyed(
                ui,
                format!("trigger-{name}-delete"),
                if confirming { "确认删除" } else { "删除" },
                false,
                cx.listener(move |this, _event, _window, cx| {
                    this.delete_trigger(&name, cx);
                }),
            )
        })
}

/// 「服务器」节：SSH 自动重连卡（开关 + 次数 stepper）+ 只读信息卡
/// （语义不变，随单页平移）。
fn settings_servers_body(
    reconnect: &ReconnectSettings,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    vec![
        // 自动重连卡：断线自动重拨（指数退避 1s/2s/4s…）；密钥环命中免补输，
        // 缺失走既有补输对话框。改设置只影响后续新触发的重连。
        setting_card(ui)
            .child(setting_row(
                ui,
                "自动重连",
                value_chip(ui, "SSH 断线"),
                reconnect_toggle_controls(
                    ui,
                    cx,
                    "自动重连",
                    ("开", true),
                    ("关", false),
                    reconnect.enabled,
                    |s: &mut ReconnectSettings, value| s.enabled = value,
                ),
            ))
            .child(setting_row(
                ui,
                "重试次数",
                value_chip(ui, &reconnect.max_attempts.to_string()),
                reconnect_stepper_controls(ui, cx, reconnect.max_attempts),
            ))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(ui.text_muted)
                    .child("0 = 断线后只提示不自动重试；重连中窗格标题显示进度"),
            ),
        setting_card(ui)
            .child(info_row(ui, "本地 Shell", "已内置"))
            .child(info_row(ui, "SSH", "已接入（断线自动重连，见上方设置）"))
            // SOCKS5 动态转发 defer 说明（v1 只落本地/远程两类隧道）
            .child(info_row(
                ui,
                "端口转发",
                "服务器编辑弹窗内配置本地/远程隧道，随连接建立；SOCKS5 动态转发规划在后续版本",
            )),
    ]
}

/// ReconnectSettings 二值开关控件：与 `toggle_controls` 同形，落点换
/// ReconnectSettings（change_reconnect_setting 分路，quake_toggle_controls 先例）。
fn reconnect_toggle_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    key: &'static str,
    on: (&'static str, bool),
    off: (&'static str, bool),
    current: bool,
    apply: impl Fn(&mut ReconnectSettings, bool) + Clone + 'static,
) -> Div {
    div()
        .flex()
        .gap_1()
        .children(
            [(on, true), (off, false)]
                .into_iter()
                .map(|((label, value), _)| {
                    let selected = value == current;
                    let apply = apply.clone();
                    setting_button_keyed(
                        ui,
                        format!("{key}-{label}"),
                        label,
                        selected,
                        cx.listener(move |this, event, window, cx| {
                            this.change_reconnect_setting(event, window, cx, |s| apply(s, value));
                        }),
                    )
                }),
        )
}

/// 重试次数步进控件（−/+）：与 stepper_controls 同规格（icon_button），
/// 落点换 ReconnectSettings；上限由载入期 sanitize（0..=10）双保险。
fn reconnect_stepper_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    current: u32,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(icon_button(
            "reconnect-attempts-decrease",
            "icons/minus.svg",
            ui,
            IconButtonSpec {
                size: 30.0,
                reveal_on_group: None,
                danger_hover: false,
                disabled: false,
            },
            cx.listener(|this, event, window, cx| {
                this.change_reconnect_setting(event, window, cx, |s| {
                    s.max_attempts = s.max_attempts.saturating_sub(1);
                });
            }),
        ))
        .child(icon_button(
            "reconnect-attempts-increase",
            "icons/plus.svg",
            ui,
            IconButtonSpec {
                size: 30.0,
                reveal_on_group: None,
                danger_hover: false,
                disabled: false,
            },
            cx.listener(|this, event, window, cx| {
                this.change_reconnect_setting(event, window, cx, |s| {
                    s.max_attempts = s.max_attempts.saturating_add(1).min(10);
                });
            }),
        ))
        .child(
            div()
                .min_w(px(24.0))
                .text_size(px(12.0))
                .text_color(ui.text)
                .child(format!("{current} 次")),
        )
}

/// 「关于」节：品牌卡（40px logo + 名称 + 版本芯片 + 一句话简介）与
/// 仓库链接 / 许可行。版本号读 workspace 单一真源 CARGO_PKG_VERSION；
/// 简介取自仓库 AGENTS.md 的定位一句话。
fn settings_about_body(ui: &UiPalette, cx: &mut Context<WorkspaceView>) -> Vec<Div> {
    vec![
        setting_card(ui)
            .child(
                div()
                    .debug_selector(|| "about-brand".into())
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1()
                    .child(crate::branding::about_logo())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(15.0))
                                            .text_color(ui.text)
                                            .child("LaterTerm"),
                                    )
                                    .child(value_chip(
                                        ui,
                                        &format!("v{}", env!("CARGO_PKG_VERSION")),
                                    )),
                            )
                            .child(
                                div().text_size(px(12.0)).text_color(ui.text_muted).child(
                                    "跨平台 GPU 终端：Deepin 终端功能 × Tabby 颜值 × 原生性能",
                                ),
                            ),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "about-repo-link".into())
                    .flex()
                    .items_center()
                    .py_1()
                    .text_size(px(12.0))
                    .text_color(ui.accent)
                    .cursor_pointer()
                    .hover(|style| style.text_color(ui.text))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _event, _window, cx| {
                            cx.stop_propagation();
                            laterterm_termview::open_url(&update_check::release_page_url());
                        }),
                    )
                    .child("github.com/ailater/LaterTerm"),
            )
            .child(info_row(ui, "许可", "MIT OR Apache-2.0")),
    ]
}

/// 「macOS 权限」节：辅助功能 / 监听事件 / 完全磁盘访问三行，状态点即查即显
/// （AX 与 ListenEvent 是廉价系统查询，每次渲染现查；磁盘访问沿用启动探测的
/// `volume_access_denied` 字段，「重新检测」重跑探测）。「去授权」只深链到
/// 对应系统设置面板，由用户手动勾选——申请/授权动作绝不代办。
/// 仅 macOS 渲染（ALL 的 cfg 隔离），探测实现见 macos_permissions.rs。
#[cfg(target_os = "macos")]
fn settings_permissions_body(
    workspace: &mut WorkspaceView,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    use crate::macos_permissions as perms;
    let ax = perms::accessibility_trusted();
    let listen = perms::listen_event_access();
    vec![
        setting_card(ui)
            .child(perm_row(
                ui,
                "辅助功能",
                "全局热键、读窗口等自动化能力的门槛",
                Some(ax),
                perms::ACCESSIBILITY_PANE,
            ))
            .child(perm_row(
                ui,
                "监听事件",
                "被动监听键盘事件（macOS 15+ 的独立权限项）",
                listen,
                perms::LISTEN_EVENT_PANE,
            ))
            .child(perm_row(
                ui,
                "完全磁盘访问",
                "访问 /Volumes 等受保护目录（跨卷文件操作、SFTP 本地面板）",
                Some(!workspace.volume_access_denied),
                perms::ALL_FILES_PANE,
            ))
            .child(div().flex().justify_end().child(setting_button(
                ui,
                "重新检测",
                false,
                cx.listener(|_workspace, _event, _window, cx| {
                    // AX/监听是即查即显的，通知一帧刷新即可；磁盘访问
                    // 要读真文件系统，后台线程探测完写回（同启动探测口径）
                    let (tx, rx) = flume::bounded(1);
                    std::thread::spawn(move || {
                        let _ = tx.send(super::probe_data_volume_access());
                    });
                    cx.spawn(async move |this, cx| {
                        if let Ok(denied) = rx.recv_async().await {
                            this.update(cx, |workspace, cx| {
                                workspace.volume_access_denied = denied;
                                cx.notify();
                            })
                            .ok();
                        }
                    })
                    .detach();
                    cx.notify();
                }),
            ))),
    ]
}

/// 权限行：状态点 + 名称/说明（左），状态文字 + 「去授权」按钮（右）。
/// `granted` 为 `None` = 本系统没有这项权限（如 <macOS 15 的监听事件），
/// 显示「不适用」中性色，不算未授权。
#[cfg(target_os = "macos")]
fn perm_row(ui: &UiPalette, name: &str, desc: &str, granted: Option<bool>, pane: &str) -> Div {
    use crate::macos_permissions as perms;
    let (dot, status) = match granted {
        Some(true) => (ui.success, "已授权"),
        Some(false) => (ui.warning, "未授权"),
        None => (ui.text_muted, "不适用"),
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_2()
        .child(
            div()
                .w(px(8.0))
                .h(px(8.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(dot),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_0p5()
                .child(div().text_color(ui.text).child(name.to_string()))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(ui.text_muted)
                        .child(desc.to_string()),
                ),
        )
        .child(
            div()
                .text_size(px(11.5))
                .text_color(dot)
                .child(status.to_string()),
        )
        // 三行同 label「去授权」：按 setting_button 注记必须带行唯一键，
        // 否则共享元素 id、按压态跨行串扰
        .child(setting_button_keyed(
            ui,
            format!("perm-{name}-grant"),
            "去授权",
            false,
            {
                let pane = pane.to_string();
                move |_, _window, _cx| perms::open_privacy(&pane)
            },
        ))
}

/// 「检查更新」节：结果横幅 + 按钮 + 状态行三态（Idle / 已是最新 / 未能确认）。
/// 手动触发而非后台自动联网（静默外连是黑盒行为）；网络 IO 在后台执行器
/// （workspace.rs check_for_update），失败一律「未能确认」静默文案不弹窗，
/// 绝不把失败说成「已是最新」。
fn settings_update_body(
    workspace: &WorkspaceView,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    let check = &workspace.update_check;
    let checking = check.checking();
    let mut card = setting_card(ui);
    // 新版本横幅：只在确实查到更新时出现，紧贴按钮上方——检查动作发生在
    // 哪，结果就出现在哪；放页顶会落在滚动视野外（LaterScreen v0.11.4 教训）
    if let Some(Ok(Outcome::Newer(tag))) = check.result() {
        card = card.child(
            div()
                .debug_selector(|| "update-newer-banner".into())
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .bg(tint_hsla(ui.accent, 0x26))
                .border_1()
                .border_color(ui.accent)
                .rounded_sm()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(ui.accent)
                        .child(format!("发现新版本 {tag}")),
                )
                .child(
                    setting_button(
                        ui,
                        "去下载",
                        true,
                        cx.listener(|_, _event, _window, cx| {
                            cx.stop_propagation();
                            laterterm_termview::open_url(&update_check::release_page_url());
                        }),
                    )
                    .debug_selector(|| "update-download-btn".into()),
                )
                .child(setting_button(
                    ui,
                    "知道了",
                    false,
                    cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.update_check.clear();
                        cx.notify();
                    }),
                )),
        );
    }
    // 检查按钮：checking 时 listener 内被 start 幂等短路（不重复发请求），
    // 渲染用 muted 色 + 文案区分禁用感——不给 setting_button 扩禁用签名，
    // 避免波及全部既有调用点（评审风险4）
    let mut check_button = setting_button(
        ui,
        if checking {
            "检查中…"
        } else {
            "检查更新"
        },
        false,
        cx.listener(|this, _event, _window, cx| this.check_for_update(cx)),
    );
    if checking {
        check_button = check_button.text_color(ui.text_muted);
    }
    let mut row = div().flex().items_center().gap_3().py_1().child(
        check_button
            .debug_selector(|| "update-check-btn".into())
            .cursor_pointer(),
    );
    // 状态行三态：未检查过只报当前版本，不暗示任何结论；失败可读但静默
    //（无弹窗）；有新版本时不重复文案——横幅就在按钮正上方
    match check.result() {
        None if !checking => {
            row = row.child(
                div()
                    .debug_selector(|| "update-status-idle".into())
                    .text_size(px(12.0))
                    .text_color(ui.text_muted)
                    .child(format!("当前 v{}", env!("CARGO_PKG_VERSION"))),
            );
        }
        Some(Ok(Outcome::UpToDate)) => {
            row = row.child(
                div()
                    .debug_selector(|| "update-status-uptodate".into())
                    .text_size(px(12.0))
                    .text_color(ui.text_muted)
                    .child(format!("当前 v{}，已是最新", env!("CARGO_PKG_VERSION"))),
            );
        }
        Some(Err(message)) => {
            row = row.child(
                div()
                    .debug_selector(|| "update-status-error".into())
                    .text_size(px(12.0))
                    .text_color(ui.text_muted)
                    .child(format!("未能确认：{message}")),
            );
        }
        // 检查中 / 有新版本：不追加状态文字
        _ => {}
    }
    vec![card.child(row)]
}

/// 「配置同步」节：WebDAV 三输入框 + 非 https 明文警示 + 三按钮 +
/// 状态行/合并差异明细。全程手动触发（零自动同步，静默外连是黑盒行为）；
/// 网络与密钥环在后台执行器（workspace.rs run_webdav_task），失败原文呈现。
/// 密码框只写不读（永不从密钥环回显，留空 = 保持已存密码，TOTP 密钥框同款）。
fn settings_config_sync_body(
    workspace: &mut WorkspaceView,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    // 输入框懒建（分区首帧按当前 [sync] 值初始化；totp_fields 同构）
    if workspace.webdav_fields.is_empty() {
        let url = cx.new(|cx| {
            CommandTextField::new(
                workspace.settings.sync.url.clone(),
                "https://dav.example.com/dav",
                cx,
            )
        });
        let username = cx
            .new(|cx| CommandTextField::new(workspace.settings.sync.username.clone(), "账号", cx));
        let password =
            cx.new(|cx| CommandTextField::new(String::new(), "留空保持已存密码", cx).masked());
        workspace.webdav_fields = vec![
            ("地址".into(), url),
            ("账号".into(), username),
            ("密码".into(), password),
        ];
    }
    let fields = workspace.webdav_fields.clone();
    // 警示行跟随地址输入实时变化：读输入框现值而非已保存值
    let url_now = fields
        .iter()
        .find(|(name, _)| name == "地址")
        .map(|(_, field)| field.read(cx).value().trim().to_string())
        .unwrap_or_default();
    let task = workspace.webdav_task;
    let mut card = setting_card(ui)
        // 固定说明行：合并语义的四类 local-only 特判（方案红线，恒本地）
        .child(
            div()
                .text_size(px(11.0))
                .text_color(ui.text_muted)
                .child("本机专属项不参与合并：WebDAV 账户配置、shell、接收目录"),
        )
        .child(div().text_size(px(11.0)).text_color(ui.text_muted).child(
            "备份在 WebDAV 目录累积保留档（laterterm-config-snapshot-<时间>.toml），可手动清理",
        ))
        .children(fields.into_iter().map(|(name, field)| {
            div()
                .pt_2()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(ui.text_secondary)
                        .child(name),
                )
                .child(field)
        }));
    // 非 https 前缀：Basic 凭据明文过网，常驻警示（横幅样式同
    // update-newer-banner，色用 danger；空地址不打扰）
    if !url_now.is_empty() && !url_now.starts_with("https://") {
        card = card.child(
            div()
                .debug_selector(|| "config-sync-plain-http-banner".into())
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .bg(tint_hsla(ui.danger, 0x26))
                .border_1()
                .border_color(ui.danger)
                .rounded_sm()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(ui.danger)
                        .child("地址未使用 https://：账号与密码将明文传输，存在被截获风险"),
                ),
        );
    }
    // 三按钮：在途时整排 muted + 当前任务换进度文案（幂等门兜底连点；
    // 不给 setting_button 扩禁用签名，检查更新按钮同款口径）
    let mut buttons = div().flex().flex_wrap().gap_1().pt_2();
    for (kind, idle_label) in [
        (WebDavTaskKind::Test, "测试连接"),
        (WebDavTaskKind::Upload, "备份到远程"),
        (WebDavTaskKind::Merge, "从远程合并"),
    ] {
        let active = task.busy() && webdav_task_matches(task, kind);
        let mut button = setting_button(
            ui,
            if active {
                task.busy_label()
            } else {
                idle_label
            },
            false,
            cx.listener(move |this, _event, _window, cx| this.run_webdav_task(kind, cx)),
        )
        .debug_selector(move || {
            format!(
                "config-sync-{}-btn",
                match kind {
                    WebDavTaskKind::Test => "test",
                    WebDavTaskKind::Upload => "upload",
                    WebDavTaskKind::Merge => "merge",
                }
            )
        });
        if task.busy() {
            button = button.text_color(ui.text_muted);
        }
        buttons = buttons.child(button);
    }
    let mut card = card.child(buttons);
    // 状态行三态 + 合并差异明细（更新状态行同款三态口径）
    let mut status = div()
        .debug_selector(|| "config-sync-status".into())
        .flex()
        .flex_col()
        .gap(px(2.0))
        .pt_1();
    match workspace.webdav_result.as_ref() {
        None if !task.busy() => {
            status = status.child(
                div()
                    .text_size(px(12.0))
                    .text_color(ui.text_muted)
                    .child("尚未同步：备份会把本机设置与自定义主题打包为单文件快照上传"),
            );
        }
        Some(Ok(WebDavOutcome::ProbeSucceeded)) => {
            status = status.child(
                div()
                    .text_size(px(12.0))
                    .text_color(ui.text_secondary)
                    .child("连接成功：服务器与凭据可用"),
            );
        }
        Some(Ok(WebDavOutcome::BackupDone { archive })) => {
            let text = match archive {
                Some(name) => format!("已备份到远程（上一份已存为保留档 {name}）"),
                None => "已备份到远程（首次备份，无保留档）".to_string(),
            };
            status = status.child(
                div()
                    .text_size(px(12.0))
                    .text_color(ui.text_secondary)
                    .child(text),
            );
        }
        Some(Ok(WebDavOutcome::MergeApplied(report))) => {
            status = status
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(ui.text)
                        .child("合并完成（已落盘并生效）"),
                )
                .children(
                    [
                        (!report.added.is_empty())
                            .then(|| format!("远端新增：{}", report.added.join("、"))),
                        (!report.overridden.is_empty()).then(|| {
                            format!(
                                "远程覆盖：{}（服务器/命令等列表整体以远程为准）",
                                report.overridden.join("、")
                            )
                        }),
                        (!report.themes_added.is_empty()).then(|| {
                            format!(
                                "远端新增主题：{}",
                                report
                                    .themes_added
                                    .iter()
                                    .map(|n| format!("{n}.toml"))
                                    .collect::<Vec<_>>()
                                    .join("、")
                            )
                        }),
                        (!report.themes_overwritten.is_empty()).then(|| {
                            let backup = report
                                .themes_backup_dir
                                .as_deref()
                                .map(|dir| format!("，已备份于 {dir}/"))
                                .unwrap_or_default();
                            format!(
                                "远端覆盖主题：{}{}",
                                report
                                    .themes_overwritten
                                    .iter()
                                    .map(|n| format!("{n}.toml"))
                                    .collect::<Vec<_>>()
                                    .join("、"),
                                backup
                            )
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .map(|line| {
                        div()
                            .text_size(px(11.0))
                            .text_color(ui.text_muted)
                            .child(line)
                    }),
                )
                .child(div().text_size(px(11.0)).text_color(ui.text_muted).child(
                    match report.settings_backup.as_deref() {
                        Some(name) => format!("已备份：{name}；本机独有主题未改动"),
                        None => "本机独有主题未改动（本机此前无配置文件）".to_string(),
                    },
                ));
        }
        Some(Err(message)) => {
            status = status.child(
                div()
                    .text_size(px(12.0))
                    .text_color(ui.danger)
                    .child(format!("同步失败：{message}")),
            );
        }
        // 在途：按钮进度文案已表达，不重复
        _ => {}
    }
    card = card.child(status);
    vec![card]
}

/// 任务状态机与动作种类的对位（在途按钮的进度文案归属）。
fn webdav_task_matches(state: WebDavTaskState, kind: WebDavTaskKind) -> bool {
    matches!(
        (state, kind),
        (WebDavTaskState::Testing, WebDavTaskKind::Test)
            | (WebDavTaskState::Uploading, WebDavTaskKind::Upload)
            | (WebDavTaskState::Merging, WebDavTaskKind::Merge)
    )
}

/// 「快捷键」节：全量绑定总览（内置 + 命令 + 服务器 + 动态码），冲突标红。
/// 绑定/计数/归一化均读 [`super::WorkspaceView::shortcut_bindings_cache`]
/// 的派生缓存（settings 变化才重算），条目行按借用渲染、点击时才取整条。
fn settings_shortcuts_body(
    workspace: &mut WorkspaceView,
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    // 快捷键键帽与终端等宽同源（A7）；下方 shortcut_row 全部复用
    let mono = laterterm_ui::tokens::mono_family(&workspace.settings.appearance.font_family);
    let cache = workspace.shortcut_bindings_cache();
    let row = |binding: &ShortcutBinding| {
        let conflicted = cache
            .counts
            .get(binding.normalized.as_str())
            .copied()
            .unwrap_or(1)
            > 1;
        setting_card(ui).child(shortcut_row(
            ui,
            &mono,
            &binding.label,
            &binding.normalized,
            conflicted,
        ))
    };
    let mut body: Vec<Div> = Vec::new();
    // 系统级热键（Quake）：仅 Linux 提供——portal GlobalShortcuts 是 Wayland 下
    // 全局热键的唯一正道，其余平台无此卡片（cfg 隔离，AGENTS.md 平台差异约定）。
    #[cfg(target_os = "linux")]
    {
        let quake = workspace.settings.quake.clone();
        let status = workspace.quake_status.clone();
        body.push(group_title(ui, "系统级热键（Quake）"));
        body.push(
            setting_card(ui)
                .child(setting_row(
                    ui,
                    "全局呼出",
                    div(),
                    quake_toggle_controls(
                        ui,
                        cx,
                        "全局呼出",
                        ("开", true),
                        ("关", false),
                        quake.enabled,
                        |settings, value| settings.enabled = value,
                    ),
                ))
                .child(setting_row(
                    ui,
                    "失焦收起",
                    div(),
                    quake_toggle_controls(
                        ui,
                        cx,
                        "失焦收起",
                        ("开", true),
                        ("关", false),
                        quake.auto_hide_on_blur,
                        |settings, value| settings.auto_hide_on_blur = value,
                    ),
                ))
                .child(
                    div()
                        .py_1()
                        .text_color(status.text_color(ui))
                        .child(status.text()),
                )
                .child(
                    div()
                        .pt_1()
                        .text_size(px(11.0))
                        .text_color(ui.text_muted)
                        .child(
                            "首次启用会弹出系统确认对话框，请在桌面环境快捷键设置中授权；\
                             实际键位可在桌面快捷键设置中修改。窗口的 Quake 几何（全屏宽 × \
                             50% 高、顶部居中）在下次启动时生效。",
                        ),
                ),
        );
    }
    body.push(group_title(ui, "内置"));
    for binding in cache
        .bindings
        .iter()
        .filter(|binding| matches!(binding.owner, BindingOwner::Builtin))
    {
        body.push(row(binding));
    }
    body.push(group_title(ui, "面板"));
    // 归一化读缓存（下标与 panel_normalized 对位），不再每帧 normalize。
    // 第四元 = 未设置时的行内文案：窗格缩放永远有生效键（None = 默认
    // Ctrl+Shift+Z，清空 = 恢复默认），文案注明以区别于面板项的「未设置」。
    for (index, (label, shortcut, unset_text, owner)) in [
        (
            "命令面板",
            workspace.settings.shortcuts.commands_panel.as_deref(),
            "未设置",
            BindingOwner::CommandsPanel,
        ),
        (
            "服务器面板",
            workspace.settings.shortcuts.servers_panel.as_deref(),
            "未设置",
            BindingOwner::ServersPanel,
        ),
        (
            "窗格缩放",
            workspace.settings.shortcuts.zoom_pane.as_deref(),
            "未设置（默认 Ctrl+Shift+Z）",
            BindingOwner::PaneZoom,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let normalized = cache.panel_normalized.get(index).cloned().flatten();
        let invalid = shortcut.is_some() && normalized.is_none();
        let conflicted = normalized
            .as_deref()
            .is_some_and(|key| cache.counts.get(key).copied().unwrap_or(0) > 1);
        body.push(
            setting_card(ui)
                .child(shortcut_row(
                    ui,
                    &mono,
                    label,
                    shortcut.unwrap_or(unset_text),
                    invalid || conflicted,
                ))
                .child(setting_button_keyed(
                    ui,
                    format!("shortcut-panel-{index}-edit"),
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
    // 行内只借用渲染 + 持有名称，点击时才查整条（免每帧深拷贝全表）。
    body.push(group_title(ui, "命令"));
    for (index, command) in workspace.settings.commands.iter().enumerate() {
        let normalized = cache.command_normalized.get(index).cloned().flatten();
        let shortcut = command.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = command.shortcut.is_some() && normalized.is_none();
        let conflicted = normalized
            .as_deref()
            .is_some_and(|key| cache.counts.get(key).copied().unwrap_or(0) > 1);
        let name = command.name.clone();
        body.push(
            setting_card(ui)
                .child(shortcut_row(
                    ui,
                    &mono,
                    &command.name,
                    shortcut,
                    invalid || conflicted,
                ))
                .child(setting_button_keyed(
                    ui,
                    format!("shortcut-command-{name}-edit"),
                    "编辑快捷键",
                    false,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(command) = this
                            .settings
                            .commands
                            .iter()
                            .find(|item| item.name == name)
                            .cloned()
                        {
                            this.edit_command(Some(command), window, cx);
                            this.start_shortcut_recording(window, cx);
                        }
                    }),
                )),
        );
    }
    body.push(group_title(ui, "服务器"));
    for (index, entry) in workspace.settings.servers.iter().enumerate() {
        let normalized = cache.server_normalized.get(index).cloned().flatten();
        let shortcut = entry.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = entry.shortcut.is_some() && normalized.is_none();
        let conflicted = normalized
            .as_deref()
            .is_some_and(|key| cache.counts.get(key).copied().unwrap_or(0) > 1);
        let name = entry.name.clone();
        body.push(
            setting_card(ui)
                .child(shortcut_row(
                    ui,
                    &mono,
                    &entry.name,
                    shortcut,
                    invalid || conflicted,
                ))
                .child(setting_button_keyed(
                    ui,
                    format!("shortcut-server-{name}-edit"),
                    "编辑快捷键",
                    false,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(entry) = this
                            .settings
                            .servers
                            .iter()
                            .find(|item| item.name == name)
                            .cloned()
                        {
                            this.edit_server(Some(&entry), window, cx);
                            this.start_shortcut_recording(window, cx);
                        }
                    }),
                )),
        );
    }
    body.push(group_title(ui, "动态码"));
    for (index, entry) in workspace.settings.totp.iter().enumerate() {
        let normalized = cache.totp_normalized.get(index).cloned().flatten();
        let shortcut = entry.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = entry.shortcut.is_some() && normalized.is_none();
        let conflicted = normalized
            .as_deref()
            .is_some_and(|key| cache.counts.get(key).copied().unwrap_or(0) > 1);
        let name = entry.name.clone();
        body.push(
            setting_card(ui)
                .child(shortcut_row(
                    ui,
                    &mono,
                    &entry.name,
                    shortcut,
                    invalid || conflicted,
                ))
                .child(setting_button_keyed(
                    ui,
                    format!("shortcut-totp-{name}-edit"),
                    "编辑快捷键",
                    false,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(entry) = this
                            .settings
                            .totp
                            .iter()
                            .find(|item| item.name == name)
                            .cloned()
                        {
                            this.edit_totp(Some(&entry), window, cx);
                            this.start_shortcut_recording(window, cx);
                        }
                    }),
                )),
        );
    }
    let has_conflict = cache.counts.values().any(|count| *count > 1);
    let invalid = &cache.invalid_rows;
    if !invalid.is_empty() {
        body.push(group_title(ui, "无效快捷键"));
        for (label, key) in invalid.iter() {
            body.push(setting_card(ui).child(shortcut_row(ui, &mono, label, key, true)));
        }
    }
    body.push(
        div()
            .pt_2()
            .text_color(if has_conflict || !invalid.is_empty() {
                ui.danger
            } else {
                ui.text_muted
            })
            .child(if has_conflict || !invalid.is_empty() {
                "存在冲突或无效绑定：请在对应编辑弹窗里修改（方向切换优先）"
            } else {
                "绑定在命令 / 服务器 / 动态码的编辑弹窗里设置，保存时自动查重"
            }),
    );
    body
}

// ---------------------------------------------------------------------------
// 设置专用控件（自 workspace.rs 原样平移，色值收敛为模块色板令牌）
// ---------------------------------------------------------------------------

/// 步进控件（−/+）：SVG 线性图标钮，与标题栏同规格（icon_button）。
/// `group` 为实例前缀：三个步进行（字号/行高/内边距）各自唯一，
/// group 名窗口级解析，同名会互相覆盖 hover。
fn stepper_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    group: &'static str,
    decrease: impl Fn(&mut Appearance) + Clone + 'static,
    increase: impl Fn(&mut Appearance) + Clone + 'static,
) -> Div {
    div()
        .flex()
        .gap_1()
        .child(icon_button(
            format!("{group}-decrease"),
            "icons/minus.svg",
            ui,
            IconButtonSpec {
                size: 30.0,
                reveal_on_group: None,
                danger_hover: false,
                disabled: false,
            },
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, decrease.clone());
            }),
        ))
        .child(icon_button(
            format!("{group}-increase"),
            "icons/plus.svg",
            ui,
            IconButtonSpec {
                size: 30.0,
                reveal_on_group: None,
                danger_hover: false,
                disabled: false,
            },
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, increase.clone());
            }),
        ))
}

fn scrollback_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    settings: &Appearance,
) -> Div {
    div().flex().gap_1().children(
        [(10_000usize, "10k"), (50_000, "50k"), (100_000, "100k")]
            .into_iter()
            .map(|(value, label)| {
                let selected = settings.scrollback == value;
                setting_button(
                    ui,
                    label,
                    selected,
                    cx.listener(move |this, event, window, cx| {
                        this.change_setting(event, window, cx, move |s| s.scrollback = value);
                    }),
                )
            }),
    )
}

/// 常用 CJK 备选字体预设；空表表示交回系统默认回退。
const FALLBACK_PRESETS: [&str; 3] = ["Sarasa Mono SC", "Noto Sans Mono CJK SC", "无"];

/// 字体族芯片：固定等宽 + flex_wrap，按内容区自然排成整齐两列。
/// 原实现芯片宽度随字长变化，6 个预设换行成 1/2/2/1，视觉上参差。
fn font_family_controls(ui: &UiPalette, cx: &mut Context<WorkspaceView>, current: &str) -> Div {
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(FONT_PRESETS.into_iter().map(|name| {
            let selected = name == current;
            setting_button(
                ui,
                name,
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.change_setting(event, window, cx, |s| s.font_family = name.to_string());
                }),
            )
            // 所见即所得：芯片文字用该字体本身渲染（等宽一目了然，缺失时
            // gpui 回落默认字体，无崩溃路径）
            .font_family(name)
            .w(px(FONT_CHIP_WIDTH))
        }))
}

/// 备选字体是多选（可同时启用多个回退），用 `multi_select_button` 的
/// 勾选语义，不再复用单选芯片——避免两个蓝框看起来像「两个单选都选中」。
fn font_fallback_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    current: &[String],
) -> Div {
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
            multi_select_button(
                ui,
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
            .w(px(FONT_CHIP_WIDTH))
        }))
}

/// 二值开关控件（选中侧高亮），如连字开/关。
/// `key` = 所在行的行键（如 "连字"）：开/关 label 全面板重复，元素 id 以
/// 行键拼唯一（`{key}-{label}`），否则按压态跨行串扰（A6，见
/// setting_button_keyed）。
fn toggle_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    key: &'static str,
    _on: (&'static str, bool),
    _off: (&'static str, bool),
    current: bool,
    apply: impl Fn(&mut Appearance, bool) + Clone + 'static,
) -> Div {
    // 滑动开关（macOS 系统设置心智模型）；on/off 文字参数已废弃不渲染，
    // 签名保留以免十余处调用点机械翻改。点击即取反。
    let next = !current;
    div().child(setting_switch(
        format!("{key}-switch"),
        ui,
        current,
        cx.listener(move |this, event, window, cx| {
            this.change_setting(event, window, cx, |s| apply(s, next));
        }),
    ))
}

/// Quake 设置的二值开关控件：与 `toggle_controls` 同形，但落点是
/// `QuakeSettings` 并联动 worker 生命周期（change_quake_setting）。
/// `key` = 行键，同 toggle_controls（开/关 id 去重）。
#[cfg(target_os = "linux")]
fn quake_toggle_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    key: &'static str,
    _on: (&'static str, bool),
    _off: (&'static str, bool),
    current: bool,
    apply: impl Fn(&mut laterterm_config::QuakeSettings, bool) + Clone + 'static,
) -> Div {
    // 滑动开关，口径同 toggle_controls（文字参数废弃不渲染）。
    let next = !current;
    div().child(setting_switch(
        format!("{key}-switch"),
        ui,
        current,
        cx.listener(move |this, event, window, cx| {
            this.change_quake_setting(event, window, cx, |s| apply(s, next));
        }),
    ))
}

/// 主题芯片行：选中态读生效主题（预览期间高亮跟随预览，与色板卡/窗格
/// 同源）；点击走预览语义（不落盘），保存/还原在预览卡下方的条件按钮。
/// 芯片列表每次 render 从 `themes.names()` 现取——热加载删掉的自定义主题
/// 自然不再渲染，预览态残留名字由 commit 的严格 get 校验兜底。
fn theme_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    themes: &laterterm_config::ThemeRegistry,
    current: &str,
) -> Div {
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(themes.names().into_iter().map(|name| {
            let selected = name == current;
            setting_button(
                ui,
                name.clone(),
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.preview_theme(event, name.clone(), window, cx);
                }),
            )
        }))
}

/// 光标形状三选一（选中侧高亮）；点击经 change_setting 自动保存并热更新全部窗格。
/// 光标样式枚举一律写全路径——`use gpui::CursorStyle` 是指针样式，不能裸名撞车。
fn cursor_style_controls(
    ui: &UiPalette,
    cx: &mut Context<WorkspaceView>,
    current: laterterm_config::CursorStyle,
) -> Div {
    div().flex().gap_1().children(
        [
            ("方块", laterterm_config::CursorStyle::Block),
            ("竖线", laterterm_config::CursorStyle::Bar),
            ("下划线", laterterm_config::CursorStyle::Underline),
        ]
        .into_iter()
        .map(|(label, style)| {
            let selected = style == current;
            setting_button(
                ui,
                label,
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.change_setting(event, window, cx, move |s| s.cursor_style = style);
                }),
            )
        }),
    )
}

fn shortcut_row(ui: &UiPalette, mono: &str, label: &str, keys: &str, conflicted: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(if conflicted {
            ui.danger
        } else {
            ui.text_secondary
        })
        .child(format!(
            "{}{}",
            label,
            if conflicted { " ⚠ 冲突" } else { "" }
        ))
        .child(value_chip(ui, keys).font_family(mono.to_string()))
}

fn info_row(ui: &UiPalette, label: &str, value: &str) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(ui.text_secondary)
        .child(label.to_string())
        .child(value.to_string())
}

/// 分区锚点标题：accent 竖条 + 14px 主文字（骨架沿用原 section_title）。
fn section_title(ui: &UiPalette, text: &str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(3.0)).h(px(16.0)).bg(ui.accent).rounded_sm())
        .child(
            div()
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ui.text)
                .child(text.to_string()),
        )
}

/// 组内子标题：比分区锚点低一级（快捷键节的内置/面板/命令等分组）。
fn group_title(ui: &UiPalette, text: &str) -> Div {
    div()
        .pt_1()
        .text_size(px(12.0))
        .text_color(ui.text_muted)
        .child(text.to_string())
}

fn setting_card(ui: &UiPalette) -> Div {
    div()
        .flex()
        .flex_col()
        .p_3()
        .gap_1()
        .bg(ui.card_bg)
        .border_1()
        // 卡片与区域分隔同档 hairline：浅色系下深绿描边会把设置项
        // 框得很重；控件级描边（数值框/按钮）仍保留 border_strong
        .border_color(ui.divider)
        .rounded_md()
}

fn setting_row(ui: &UiPalette, label: &str, chip: Div, controls: Div) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_2()
        .child(
            div()
                .w(px(64.0))
                .text_color(ui.text_secondary)
                .child(label.to_string()),
        )
        .child(chip)
        .child(controls)
}

fn value_chip(ui: &UiPalette, text: &str) -> Div {
    div()
        .min_w(px(64.0))
        .h(px(H_CTRL))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .bg(ui.terminal_bg)
        .border_1()
        .border_color(ui.border_strong)
        .rounded_sm()
        .text_color(ui.text)
        .child(text.to_string())
}

/// 面板/对话框共用的按钮芯片（对话框沿用同一观感；色板随主题——若需回
/// 固定深色，仅调用侧传 `UiPalette::from_theme(&ThemeColors::tokyo_night())`
/// 一处可切）。选中态 = 主操作（实心 accent + 反色文字）：单选组里只应有
/// 一个实心块，与未选中的中性描边一眼可分；对话框的主按钮（保存主题 /
/// 去下载 / 保存）复用同一语义。多选语义走 `multi_select_button`，不要复用
/// 本控件——否则「多个蓝框」会被误读成多个单选同时高亮。
///
/// 元素 id 取 label：**仅当同帧元素树内没有第二个同 label 实例时才安全**
/// （匿名 div 不产生 GlobalElementId 路径分量，跨行/跨区同名 label 会共享
/// id，按压态互相串扰）。行级重复 label（开/关、删除、每行同款按钮）必须
/// 走 [`setting_button_keyed`] 传行唯一键。
pub(crate) fn setting_button(
    ui: &UiPalette,
    label: impl Into<SharedString>,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    setting_button_keyed(ui, label.clone(), label, selected, listener)
}

/// [`setting_button`] 的带键变体：id 由调用方拼出行/组唯一键（如
/// `format!("tunnel-{index}-delete")`，同 server-connect-{name} 做法），
/// 行级重复 label 的按钮各自持有独立 GlobalElementId，按压态（A6 依赖
/// InteractiveElementState 按 GlobalElementId 寻址）不再跨行串扰。
pub(crate) fn setting_button_keyed(
    ui: &UiPalette,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let kind = if selected {
        ButtonKind::Primary
    } else {
        ButtonKind::Secondary
    };
    // A6：按压态（.active）依赖跨帧稳定的 ElementId（InteractiveElementState
    // 按 GlobalElementId 寻址、只结转本帧被访问的键）；逐帧自增 id 会让按
    // 压态永远 miss——id 必须由调用方给出行/组稳定键。
    ui_button(id, label, kind, ui, false, listener)
}

/// 多选芯片：选中 = 中性提亮底 + ✓ + 主文字，描边始终中性，不带 accent。
/// 用于「备选字体」这类可同时选中多项的组，与单选的实心 accent 明确区分。
fn multi_select_button(
    ui: &UiPalette,
    label: impl Into<SharedString>,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    // id 用去 ✓ 前缀的原 label：选中态切换不改 id，按压 stateful 状态不重置
    //（A6，同 setting_button 注记）
    let chip_id = label.clone();
    // ✓ 直接并入文字（前缀），保持「先勾后名」的阅读顺序
    let label: SharedString = if selected {
        format!("✓ {label}").into()
    } else {
        label
    };
    ui_button(chip_id, label, ButtonKind::Secondary, ui, false, listener)
}
