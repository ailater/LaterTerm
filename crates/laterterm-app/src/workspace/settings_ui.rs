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
//! 六个分区块（gpui 把 child_bounds 按直接子元素记录，多包一层或多插尾部
//! 元素都会让 scroll_to_top_of_item 与 scroll-spy 整体错位一节）。「恢复默认」
//! 按钮挂在「滚动与主题」分区块内部（原外观页最后一个子元素），因此不会
//! 成为第七个直接子元素。
//!
//! 快捷键录制链路不动：录制控件 shortcut_editor_row 只渲染于窗口级模态弹窗
//! （面板/命令/服务器/动态码编辑），不在设置面板内；本模块里的「编辑快捷键」
//! 按钮只是弹窗入口，原样平移。

use super::{BindingOwner, ButtonKind, ShortcutBinding, UiColors, WorkspaceView, ui_button};
use crate::onboarding::FONT_PRESETS;
use crate::update_check::{self, Outcome};
use gpui::{
    App, Context, Div, InteractiveElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, rgb, rgba,
};
use laterterm_config::Appearance;
use laterterm_config::theme::ThemeColors;

// ---------------------------------------------------------------------------
// Tokyo Night 色板收敛（评审硬伤5：逐处替换、只动本模块，不按色值全局替换
// ——0x35415B/0x343750 等值横跨右侧面板与标题栏分隔线，全局替换必误伤）
// ---------------------------------------------------------------------------

/// 面板底 / 芯片内凹底。
const COLOR_OVERLAY_BG: u32 = 0x1A1B26;
/// 导航底（比面板深一档，融入页框）。
const COLOR_NAV_BG: u32 = 0x16161E;
/// 卡片底。
const COLOR_CARD_BG: u32 = 0x24283B;
/// 卡片描边 / 分隔线。
const COLOR_STROKE: u32 = 0x2F334D;
/// 强调色。
const COLOR_ACCENT: u32 = 0x7AA2F7;
/// 主文字。
const COLOR_TEXT: u32 = 0xC0CAF5;
/// 次级文字。
const COLOR_TEXT_SECONDARY: u32 = 0xA9B1D6;
/// 弱化文字。
const COLOR_TEXT_MUTED: u32 = 0x565A78;
/// 危险 / 冲突。
const COLOR_DANGER: u32 = 0xF7768E;
/// 选中态 accent 半透明 tint（gpui::rgba 低 8 位是 alpha，26/255 ≈ 10%）。
/// 注：LaterScreen 暗色模式用「中性提亮」（压暗的红在暗底上发闷）；这里
/// 有意保留品牌色选中态——Tokyo Night 的蓝在深底上不闷，红/蓝气质不同。
/// 属与参照相反的决策，留 4K 实机走查复核。
const COLOR_ACCENT_TINT: u32 = 0x7AA2F726;
/// 选中态 hover：tint 加深一档。
const COLOR_ACCENT_TINT_HOVER: u32 = 0x7AA2F733;
/// 未选中 hover：中性提亮（半透明次级色，31/255 ≈ 12%）。
const COLOR_HOVER_TINT: u32 = 0xA9B1D61F;
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
    /// 关于（品牌卡 + 仓库/许可信息）。
    About,
    /// 检查更新（手动触发，见 update_check.rs）。
    UpdateCheck,
}

impl SettingsSection {
    /// 导航/内容顺序即滚动容器直接子元素顺序；label / anchor_title /
    /// debug_key 的 match 均不带通配分支，新增变体漏加任一处都会编译失败。
    pub(crate) const ALL: [Self; 8] = [
        Self::Appearance,
        Self::Font,
        Self::Scrolling,
        Self::Shortcuts,
        Self::Terminal,
        Self::Servers,
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
pub(crate) fn settings_overlay(
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
                .bg(rgb(COLOR_OVERLAY_BG))
                .border_1()
                .border_color(rgb(COLOR_STROKE))
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
                        .border_color(rgb(COLOR_STROKE))
                        .child(div().text_color(rgb(COLOR_TEXT)).child("设置"))
                        .child(
                            div()
                                .h(px(30.0))
                                .w(px(30.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(COLOR_DANGER))
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
                        .border_color(rgb(COLOR_STROKE))
                        .text_color(rgb(COLOR_TEXT_SECONDARY))
                        .child(format!(
                            "修改即时生效 · 共 {} 节，上下滚动查看全部设置",
                            SettingsSection::ALL.len()
                        ))
                        .child("LaterTerm"),
                ),
        )
}

/// 左导航：当前分区指示 + 点击跳转（滚动跟随由 workspace 的 scroll-spy 驱动）。
pub(crate) fn settings_navigation(active: SettingsSection, cx: &mut Context<WorkspaceView>) -> Div {
    div()
        .w(px(150.0))
        .h_full()
        .p_3()
        .bg(rgb(COLOR_NAV_BG))
        .flex()
        .flex_col()
        .gap_1()
        .children(
            SettingsSection::ALL
                .into_iter()
                .map(|section| settings_nav_item(section, section == active, cx)),
        )
}

/// 导航项：32px 行高 + 选中 accent tint 底 + 左侧 3px accent 指示条 +
/// 主文字色，未选中悬停中性提亮。（行高有意比 LaterScreen 参照 34px 紧凑，
/// 设置面板只有文字无图标，32px 足够点击目标。）
fn settings_nav_item(
    section: SettingsSection,
    selected: bool,
    cx: &mut Context<WorkspaceView>,
) -> Div {
    let (bg, hover_bg, text) = if selected {
        (
            rgba(COLOR_ACCENT_TINT),
            rgba(COLOR_ACCENT_TINT_HOVER),
            rgb(COLOR_TEXT),
        )
    } else {
        (
            rgba(COLOR_NAV_BG),
            rgba(COLOR_HOVER_TINT),
            rgb(COLOR_TEXT_SECONDARY),
        )
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
        .h(px(32.0))
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
                    .bg(rgb(COLOR_ACCENT)),
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
    cx: &mut Context<WorkspaceView>,
) -> Stateful<Div> {
    let appearance = workspace.settings.appearance.clone();
    let mut children = Vec::with_capacity(SettingsSection::ALL.len());
    for section in SettingsSection::ALL {
        let body: Vec<Div> = match section {
            SettingsSection::Appearance => settings_appearance_body(&appearance, cx),
            SettingsSection::Font => settings_font_body(&appearance, cx),
            SettingsSection::Scrolling => settings_scrolling_body(workspace, &appearance, cx),
            SettingsSection::Shortcuts => settings_shortcuts_body(workspace, cx),
            SettingsSection::Terminal => settings_terminal_body(),
            SettingsSection::Servers => settings_servers_body(),
            SettingsSection::About => settings_about_body(cx),
            SettingsSection::UpdateCheck => settings_update_body(workspace, cx),
        };
        let block = div()
            .debug_selector(move || format!("settings-section-{}", section.debug_key()))
            .pt_2()
            .pb_5()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title(section.anchor_title()))
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
        .text_color(rgb(COLOR_TEXT))
        .children(children)
}

/// 「终端外观」卡：字号 / 行高 / 内边距。
fn settings_appearance_body(appearance: &Appearance, cx: &mut Context<WorkspaceView>) -> Vec<Div> {
    vec![
        setting_card()
            .child(setting_row(
                "字号",
                value_chip(&format!("{:.0} px", appearance.font_size)),
                stepper_controls(
                    cx,
                    |s: &mut Appearance| s.font_size = (s.font_size - 1.0).max(8.0),
                    |s: &mut Appearance| s.font_size = (s.font_size + 1.0).min(32.0),
                ),
            ))
            .child(setting_row(
                "行高",
                value_chip(&format!("{:.1}", appearance.line_height)),
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
                value_chip(&format!("{:.0} px", appearance.padding)),
                stepper_controls(
                    cx,
                    |s: &mut Appearance| s.padding = (s.padding - 1.0).max(0.0),
                    |s: &mut Appearance| s.padding = (s.padding + 1.0).min(24.0),
                ),
            )),
    ]
}

/// 「字体」卡：字体族 / 连字 / 光标闪烁 / 光标样式 / 平滑滚动 / 选中即复制 / 备选。
fn settings_font_body(appearance: &Appearance, cx: &mut Context<WorkspaceView>) -> Vec<Div> {
    vec![
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
                            .text_color(rgb(COLOR_TEXT_SECONDARY))
                            .child("字体"),
                    )
                    .child(font_family_controls(cx, &appearance.font_family)),
            )
            .child(setting_row(
                "连字",
                div(),
                toggle_controls(
                    cx,
                    ("开", true),
                    ("关", false),
                    appearance.ligatures,
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
                    appearance.cursor_blink,
                    |s: &mut Appearance, value| s.cursor_blink = value,
                ),
            ))
            .child(setting_row(
                "光标样式",
                div(),
                cursor_style_controls(cx, appearance.cursor_style),
            ))
            .child(setting_row(
                "平滑滚动",
                div(),
                toggle_controls(
                    cx,
                    ("开", true),
                    ("关", false),
                    appearance.smooth_scrolling,
                    |s: &mut Appearance, value| s.smooth_scrolling = value,
                ),
            ))
            .child(setting_row(
                // 拖拽/双击/三击出选区松开即复制（Deepin 同款），不只限拖拽
                "选中即复制",
                div(),
                toggle_controls(
                    cx,
                    ("开", true),
                    ("关", false),
                    appearance.copy_on_select,
                    |s: &mut Appearance, value| s.copy_on_select = value,
                ),
            ))
            .child(setting_row(
                // 终端 bell 响铃时弹系统桌面通知；默认关（只视觉提示，测试不外发）
                "响铃系统通知",
                div(),
                toggle_controls(
                    cx,
                    ("开", true),
                    ("关", false),
                    appearance.bell_notification,
                    |s: &mut Appearance, value| s.bell_notification = value,
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
                            .text_color(rgb(COLOR_TEXT_SECONDARY))
                            .child("备选"),
                    )
                    .child(font_fallback_controls(cx, &appearance.font_fallbacks)),
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
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    // 生效主题唯一读口（预览优先）：芯片选中态、色板卡、窗格颜色三处同源。
    let effective = workspace.effective_theme_name().to_string();
    let preview = palette_preview(&workspace.themes.resolve(&effective).colors);
    let previewing = workspace.theme_preview.is_some();
    let mut theme_card = setting_card()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .py_2()
                .child(
                    div()
                        .w(px(64.0))
                        .text_color(rgb(COLOR_TEXT_SECONDARY))
                        .child("回滚"),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(scrollback_controls(cx, appearance)),
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
                        .text_color(rgb(COLOR_TEXT_SECONDARY))
                        .child("主题"),
                )
                .child(theme_controls(cx, &workspace.themes, &effective)),
        )
        .child(theme_preview_card(&preview));
    // 预览激活才出现的提交/放弃控件与提示行——全部塞进本分区块内部，
    // 不得新增滚动容器直接子元素（settings_content 六节对位硬约束）。
    if previewing {
        theme_card = theme_card
            .child(
                div()
                    .pt_1()
                    .text_size(px(11.0))
                    .text_color(rgb(COLOR_ACCENT))
                    .child("预览中：保存后生效，关面板自动还原"),
            )
            .child(
                div()
                    .debug_selector(|| "theme-preview-actions".into())
                    .flex()
                    .gap_1()
                    .child(setting_button(
                        "保存主题",
                        true,
                        cx.listener(|this, event, window, cx| {
                            this.commit_theme_preview(event, window, cx)
                        }),
                    ))
                    .child(setting_button(
                        "还原",
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.cancel_theme_preview(cx);
                        }),
                    )),
            );
    }
    vec![
        theme_card,
        setting_button(
            "恢复默认",
            false,
            cx.listener(WorkspaceView::reset_settings),
        ),
    ]
}

/// 主题色板预览卡：背景/前景/光标三色块 + ANSI 8×2 网格 + 示例终端文本，
/// 数据全部来自 `palette_preview`（生效主题；预览期间即预览主题）。中文名
/// 以小字标签直接排在色块下——gpui tooltip 需要 AnyView 工厂无字符串糖
/// （评审建议2），全仓尚无 .tooltip( 先例，不为此 UI 卡引入首例。
fn theme_preview_card(preview: &PalettePreview) -> Div {
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
                    .border_color(rgb(COLOR_STROKE))
                    .bg(rgb(color)),
            )
            .child(
                div()
                    .text_size(px(9.0))
                    .text_color(rgb(COLOR_TEXT_MUTED))
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
        .bg(rgb(COLOR_OVERLAY_BG))
        .border_1()
        .border_color(rgb(COLOR_STROKE))
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

/// 「终端」节：只读信息卡（语义不变，随单页平移）。
fn settings_terminal_body() -> Vec<Div> {
    vec![
        setting_card()
            .child(info_row("Shell", "$SHELL / bash"))
            .child(info_row("TERM", "xterm-256color"))
            .child(info_row("COLORTERM", "truecolor"))
            .child(info_row("滚动", "自然滚动，支持历史回看")),
    ]
}

/// 「服务器」节：只读信息卡（语义不变，随单页平移）。
fn settings_servers_body() -> Vec<Div> {
    vec![
        setting_card()
            .child(info_row("本地 Shell", "已内置"))
            .child(info_row("SSH", "远程模块接入后可用")),
    ]
}

/// 「关于」节：品牌卡（40px logo + 名称 + 版本芯片 + 一句话简介）与
/// 仓库链接 / 许可行。版本号读 workspace 单一真源 CARGO_PKG_VERSION；
/// 简介取自仓库 AGENTS.md 的定位一句话。
fn settings_about_body(cx: &mut Context<WorkspaceView>) -> Vec<Div> {
    vec![
        setting_card()
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
                                            .text_color(rgb(COLOR_TEXT))
                                            .child("LaterTerm"),
                                    )
                                    .child(value_chip(&format!("v{}", env!("CARGO_PKG_VERSION")))),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(COLOR_TEXT_MUTED))
                                    .child(
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
                    .text_color(rgb(COLOR_ACCENT))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(COLOR_TEXT)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _event, _window, cx| {
                            cx.stop_propagation();
                            laterterm_termview::open_url(&update_check::release_page_url());
                        }),
                    )
                    .child("github.com/ailater/LaterTerm"),
            )
            .child(info_row("许可", "MIT OR Apache-2.0")),
    ]
}

/// 「检查更新」节：结果横幅 + 按钮 + 状态行三态（Idle / 已是最新 / 未能确认）。
/// 手动触发而非后台自动联网（静默外连是黑盒行为）；网络 IO 在后台执行器
/// （workspace.rs check_for_update），失败一律「未能确认」静默文案不弹窗，
/// 绝不把失败说成「已是最新」。
fn settings_update_body(workspace: &WorkspaceView, cx: &mut Context<WorkspaceView>) -> Vec<Div> {
    let check = &workspace.update_check;
    let checking = check.checking();
    let mut card = setting_card();
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
                .bg(rgba(COLOR_ACCENT_TINT))
                .border_1()
                .border_color(rgb(COLOR_ACCENT))
                .rounded_sm()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(rgb(COLOR_ACCENT))
                        .child(format!("发现新版本 {tag}")),
                )
                .child(
                    setting_button(
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
        if checking {
            "检查中…"
        } else {
            "检查更新"
        },
        false,
        cx.listener(|this, _event, _window, cx| this.check_for_update(cx)),
    );
    if checking {
        check_button = check_button.text_color(rgb(COLOR_TEXT_MUTED));
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
                    .text_color(rgb(COLOR_TEXT_MUTED))
                    .child(format!("当前 v{}", env!("CARGO_PKG_VERSION"))),
            );
        }
        Some(Ok(Outcome::UpToDate)) => {
            row = row.child(
                div()
                    .debug_selector(|| "update-status-uptodate".into())
                    .text_size(px(12.0))
                    .text_color(rgb(COLOR_TEXT_MUTED))
                    .child(format!("当前 v{}，已是最新", env!("CARGO_PKG_VERSION"))),
            );
        }
        Some(Err(message)) => {
            row = row.child(
                div()
                    .debug_selector(|| "update-status-error".into())
                    .text_size(px(12.0))
                    .text_color(rgb(COLOR_TEXT_MUTED))
                    .child(format!("未能确认：{message}")),
            );
        }
        // 检查中 / 有新版本：不追加状态文字
        _ => {}
    }
    vec![card.child(row)]
}

/// 「快捷键」节：全量绑定总览（内置 + 命令 + 服务器 + 动态码），冲突标红。
/// 绑定/计数/归一化均读 [`super::WorkspaceView::shortcut_bindings_cache`]
/// 的派生缓存（settings 变化才重算），条目行按借用渲染、点击时才取整条。
fn settings_shortcuts_body(
    workspace: &mut WorkspaceView,
    cx: &mut Context<WorkspaceView>,
) -> Vec<Div> {
    let cache = workspace.shortcut_bindings_cache();
    let row = |binding: &ShortcutBinding| {
        let conflicted = cache
            .counts
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
    let mut body: Vec<Div> = Vec::new();
    // 系统级热键（Quake）：仅 Linux 提供——portal GlobalShortcuts 是 Wayland 下
    // 全局热键的唯一正道，其余平台无此卡片（cfg 隔离，AGENTS.md 平台差异约定）。
    #[cfg(target_os = "linux")]
    {
        let quake = workspace.settings.quake.clone();
        let status = workspace.quake_status.clone();
        body.push(group_title("系统级热键（Quake）"));
        body.push(
            setting_card()
                .child(setting_row(
                    "全局呼出",
                    div(),
                    quake_toggle_controls(
                        cx,
                        ("开", true),
                        ("关", false),
                        quake.enabled,
                        |settings, value| settings.enabled = value,
                    ),
                ))
                .child(setting_row(
                    "失焦收起",
                    div(),
                    quake_toggle_controls(
                        cx,
                        ("开", true),
                        ("关", false),
                        quake.auto_hide_on_blur,
                        |settings, value| settings.auto_hide_on_blur = value,
                    ),
                ))
                .child(
                    div()
                        .py_1()
                        .text_color(rgb(status.text_color()))
                        .child(status.text()),
                )
                .child(
                    div()
                        .pt_1()
                        .text_size(px(11.0))
                        .text_color(rgb(COLOR_TEXT_MUTED))
                        .child(
                            "首次启用会弹出系统确认对话框，请在桌面环境快捷键设置中授权；\
                             实际键位可在桌面快捷键设置中修改。窗口的 Quake 几何（全屏宽 × \
                             50% 高、顶部居中）在下次启动时生效。",
                        ),
                ),
        );
    }
    body.push(group_title("内置"));
    for binding in cache
        .bindings
        .iter()
        .filter(|binding| matches!(binding.owner, BindingOwner::Builtin))
    {
        body.push(row(binding));
    }
    body.push(group_title("面板"));
    // 归一化读缓存（下标与 panel_normalized 对位），不再每帧 normalize
    for (index, (label, shortcut, owner)) in [
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
    // 行内只借用渲染 + 持有名称，点击时才查整条（免每帧深拷贝全表）。
    body.push(group_title("命令"));
    for (index, command) in workspace.settings.commands.iter().enumerate() {
        let normalized = cache.command_normalized.get(index).cloned().flatten();
        let shortcut = command.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = command.shortcut.is_some() && normalized.is_none();
        let conflicted = normalized
            .as_deref()
            .is_some_and(|key| cache.counts.get(key).copied().unwrap_or(0) > 1);
        let name = command.name.clone();
        body.push(
            setting_card()
                .child(shortcut_row(&command.name, shortcut, invalid || conflicted))
                .child(setting_button(
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
    body.push(group_title("服务器"));
    for (index, entry) in workspace.settings.servers.iter().enumerate() {
        let normalized = cache.server_normalized.get(index).cloned().flatten();
        let shortcut = entry.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = entry.shortcut.is_some() && normalized.is_none();
        let conflicted = normalized
            .as_deref()
            .is_some_and(|key| cache.counts.get(key).copied().unwrap_or(0) > 1);
        let name = entry.name.clone();
        body.push(
            setting_card()
                .child(shortcut_row(&entry.name, shortcut, invalid || conflicted))
                .child(setting_button(
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
    body.push(group_title("动态码"));
    for (index, entry) in workspace.settings.totp.iter().enumerate() {
        let normalized = cache.totp_normalized.get(index).cloned().flatten();
        let shortcut = entry.shortcut.as_deref().unwrap_or("未绑定");
        let invalid = entry.shortcut.is_some() && normalized.is_none();
        let conflicted = normalized
            .as_deref()
            .is_some_and(|key| cache.counts.get(key).copied().unwrap_or(0) > 1);
        let name = entry.name.clone();
        body.push(
            setting_card()
                .child(shortcut_row(&entry.name, shortcut, invalid || conflicted))
                .child(setting_button(
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
        body.push(group_title("无效快捷键"));
        for (label, key) in invalid.iter() {
            body.push(setting_card().child(shortcut_row(label, key, true)));
        }
    }
    body.push(
        div()
            .pt_2()
            .text_color(if has_conflict || !invalid.is_empty() {
                rgb(COLOR_DANGER)
            } else {
                rgb(COLOR_TEXT_MUTED)
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

fn stepper_controls(
    cx: &mut Context<WorkspaceView>,
    decrease: impl Fn(&mut Appearance) + Clone + 'static,
    increase: impl Fn(&mut Appearance) + Clone + 'static,
) -> Div {
    // −/+ 是图标钮：走 Ghost 语义（透明底 hover 浮现），不再与单选芯片同款
    div()
        .flex()
        .gap_1()
        .child(ui_button(
            "−",
            ButtonKind::Ghost,
            panel_ui(),
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, decrease.clone());
            }),
        ))
        .child(ui_button(
            "+",
            ButtonKind::Ghost,
            panel_ui(),
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, increase.clone());
            }),
        ))
}

fn scrollback_controls(cx: &mut Context<WorkspaceView>, settings: &Appearance) -> Div {
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

/// 常用 CJK 备选字体预设；空表表示交回系统默认回退。
const FALLBACK_PRESETS: [&str; 3] = ["Sarasa Mono SC", "Noto Sans Mono CJK SC", "无"];

/// 字体族芯片：固定等宽 + flex_wrap，按内容区自然排成整齐两列。
/// 原实现芯片宽度随字长变化，6 个预设换行成 1/2/2/1，视觉上参差。
fn font_family_controls(cx: &mut Context<WorkspaceView>, current: &str) -> Div {
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
            .w(px(FONT_CHIP_WIDTH))
        }))
}

/// 备选字体是多选（可同时启用多个回退），用 `multi_select_button` 的
/// 勾选语义，不再复用单选芯片——避免两个蓝框看起来像「两个单选都选中」。
fn font_fallback_controls(cx: &mut Context<WorkspaceView>, current: &[String]) -> Div {
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
fn toggle_controls(
    cx: &mut Context<WorkspaceView>,
    on: (&'static str, bool),
    off: (&'static str, bool),
    current: bool,
    apply: impl Fn(&mut Appearance, bool) + Clone + 'static,
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

/// Quake 设置的二值开关控件：与 `toggle_controls` 同形，但落点是
/// `QuakeSettings` 并联动 worker 生命周期（change_quake_setting）。
#[cfg(target_os = "linux")]
fn quake_toggle_controls(
    cx: &mut Context<WorkspaceView>,
    on: (&'static str, bool),
    off: (&'static str, bool),
    current: bool,
    apply: impl Fn(&mut laterterm_config::QuakeSettings, bool) + Clone + 'static,
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
                    setting_button(
                        label,
                        selected,
                        cx.listener(move |this, event, window, cx| {
                            this.change_quake_setting(event, window, cx, |s| apply(s, value));
                        }),
                    )
                }),
        )
}

/// 主题芯片行：选中态读生效主题（预览期间高亮跟随预览，与色板卡/窗格
/// 同源）；点击走预览语义（不落盘），保存/还原在预览卡下方的条件按钮。
/// 芯片列表每次 render 从 `themes.names()` 现取——热加载删掉的自定义主题
/// 自然不再渲染，预览态残留名字由 commit 的严格 get 校验兜底。
fn theme_controls(
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
                label,
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.change_setting(event, window, cx, move |s| s.cursor_style = style);
                }),
            )
        }),
    )
}

fn shortcut_row(label: &str, keys: &str, conflicted: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(if conflicted {
            rgb(COLOR_DANGER)
        } else {
            rgb(COLOR_TEXT_SECONDARY)
        })
        .child(format!(
            "{}{}",
            label,
            if conflicted { " ⚠ 冲突" } else { "" }
        ))
        .child(value_chip(keys))
}

fn info_row(label: &str, value: &str) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(rgb(COLOR_TEXT_SECONDARY))
        .child(label.to_string())
        .child(value.to_string())
}

/// 分区锚点标题：accent 竖条 + 14px 主文字（骨架沿用原 section_title）。
fn section_title(text: &str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(3.0))
                .h(px(16.0))
                .bg(rgb(COLOR_ACCENT))
                .rounded_sm(),
        )
        .child(
            div()
                .text_size(px(14.0))
                .text_color(rgb(COLOR_TEXT))
                .child(text.to_string()),
        )
}

/// 组内子标题：比分区锚点低一级（快捷键节的内置/面板/命令等分组）。
fn group_title(text: &str) -> Div {
    div()
        .pt_1()
        .text_size(px(12.0))
        .text_color(rgb(COLOR_TEXT_MUTED))
        .child(text.to_string())
}

fn setting_card() -> Div {
    div()
        .flex()
        .flex_col()
        .p_3()
        .gap_1()
        .bg(rgb(COLOR_CARD_BG))
        .border_1()
        .border_color(rgb(COLOR_STROKE))
        .rounded_md()
}

fn setting_row(label: &str, chip: Div, controls: Div) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_2()
        .child(
            div()
                .w(px(64.0))
                .text_color(rgb(COLOR_TEXT_SECONDARY))
                .child(label.to_string()),
        )
        .child(chip)
        .child(controls)
}

fn value_chip(text: &str) -> Div {
    div()
        .min_w(px(64.0))
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .bg(rgb(COLOR_OVERLAY_BG))
        .border_1()
        .border_color(rgb(COLOR_STROKE))
        .rounded_sm()
        .text_color(rgb(COLOR_TEXT))
        .child(text.to_string())
}

/// 设置面板/对话框按钮的统一色系：面板是固定深色 overlay（不随终端主题
/// 换肤），按钮色固定由内置 Tokyo Night 派生，与面板底色自洽；几何与语义
/// 则全部走 workspace 的 ui_button 工厂（标题栏/边栏按钮同源）。
fn panel_ui() -> UiColors {
    UiColors::from_theme(&ThemeColors::tokyo_night())
}

/// 面板/对话框共用的按钮芯片（对话框沿用同一观感，随本模块收敛为
/// Tokyo Night——sftp_panel 的注释「对齐 setting_button 观感」即此约定）。
/// 选中态 = 主操作（实心 accent + 反色文字）：单选组里只应有一个实心块，
/// 与未选中的中性描边一眼可分；对话框的主按钮（保存主题 / 去下载 / 保存）
/// 复用同一语义。多选语义走 `multi_select_button`，不要复用本控件——
/// 否则「多个蓝框」会被误读成多个单选同时高亮。
pub(crate) fn setting_button(
    label: impl Into<SharedString>,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let kind = if selected {
        ButtonKind::Primary
    } else {
        ButtonKind::Secondary
    };
    ui_button(label, kind, panel_ui(), listener)
}

/// 多选芯片：选中 = 中性提亮底 + ✓ + 主文字，描边始终中性，不带 accent。
/// 用于「备选字体」这类可同时选中多项的组，与单选的实心 accent 明确区分。
fn multi_select_button(
    label: impl Into<SharedString>,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let label: SharedString = label.into();
    // ✓ 直接并入文字（前缀），保持「先勾后名」的阅读顺序
    let label: SharedString = if selected {
        format!("✓ {label}").into()
    } else {
        label
    };
    ui_button(label, ButtonKind::Secondary, panel_ui(), listener)
}
