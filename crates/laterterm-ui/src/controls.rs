//! SPDX-License-Identifier: MIT OR Apache-2.0

//! 共享控件工厂：统一按钮/图标钮/菜单项/浮层阴影，集中管控风格。
//!
//! 蓝本为主程序 `workspace.rs` 的 `ui_button`/`icon_button`（231-294 行）与
//! 标签右键菜单的 `item` 工厂（3551-3581 行），差异均有意收紧，迁移时以
//! 本 crate 为准：
//! 1. Secondary 钮常态边从 `border`（暗档）改为 [`UiPalette::border_strong`]
//!    （提亮档），对齐约定④「可交互控件常态边一律 border_strong」；
//! 2. 菜单项的收起动作不再耦合 `WorkspaceView`，回调由调用方自带
//!    （收菜单、执行动作都在回调里做），工厂只负责样式与禁用态；
//! 3. 图标钮参数化：尺寸三档（[`crate::tokens::icon_size_for`]）、可配
//!    「随组显现」（悬停才出现的关闭钮）与危险悬停（红色警示）；
//!    另补按压态（`.active`）与禁用态、指针光标，三态语义完整。

use gpui::prelude::*;
use gpui::{
    App, BoxShadow, Div, MouseButton, MouseDownEvent, SharedString, Stateful, Window, div, hsla,
    point, px, svg,
};

use crate::palette::UiPalette;
use crate::tokens::{H_CTRL, H_MENU, TEXT_BODY_SM, icon_size_for};

/// 按钮语义（全应用统一）：主操作 / 次操作。
/// 主操作 = accent 实心 + 反色文字；次操作 = 中性描边 + 主文字。
/// 图标类按钮一律走 [`icon_button`]（SVG 线性图标），勿用本枚举拼文字钮。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonKind {
    /// 主操作（保存/连接/发送/去下载）：accent 实心 + 反色文字。
    Primary,
    /// 次操作（取消/普通项）：中性描边 + 主文字。
    Secondary,
}

/// 图标钮参数束（[`icon_button`] 的样式开关；id/图标/回调走函数参数）。
#[derive(Clone, Default)]
pub struct IconButtonSpec {
    /// 控件边长（正方形）：SVG 尺寸按 [`icon_size_for`] 取档
    /// （30→16、26→14、22→12），行内小钮传 22.0、弹窗关闭钮传 26.0。
    pub size: f32,
    /// 随组显现：Some(组名) 时容器默认全透明，悬停该组才浮现
    /// （窗格角标关闭钮挂 "pane"、标签行关闭钮挂 "tab"；与原
    /// `opacity(0.0) + group_hover` 逐句等值）。None = 常显。
    pub reveal_on_group: Option<SharedString>,
    /// 危险悬停：hover 时图标变 `ui.danger`（关闭/删除类钮用，
    /// 与原「hover 红字」口径一致）。
    pub danger_hover: bool,
    /// 禁用态：置灰、无 hover/按压/指针，不挂点击监听。
    pub disabled: bool,
}

/// 统一按钮工厂：按语义给出底色/边框/文字/hover，集中管控风格。
///
/// 几何口径：高 [`H_CTRL`]（30px）、最小宽 36px、水平内边距 `px_3`、圆角
/// `rounded_sm`（4px，约定①）、字号 [`TEXT_BODY`]（13px）。hover 为 gpui
/// 即时 refine（约定②），无插值；按压态整钮沉入 [`UiPalette::active_bg`]。
/// 禁用态：文字 `text_disabled` + 边 `border_strong`（提亮档；border 暗
/// 档压深底上近隐形，约定④），无 hover/按压/指针，也不挂监听。
///
/// `id` 须窗口内唯一（用作 Stateful div 的元素 id，按压态依赖它）。
/// 返回 [`Stateful`]<[Div]>：启用态带 `id` 才有按压细化（`.active`）；
/// 禁用态同样挂 `id`（无监听即无交互，仅为两分支返回类型一致）。
pub fn ui_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    ui: &UiPalette,
    disabled: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let id = id.into();
    let (bg, border, text, hover_bg, hover_text) = match kind {
        // 主操作：accent 实心 + 反色文字（accent 底上用最深底色保证对比度），
        // hover 提亮 accent 10% L、文字不变。
        ButtonKind::Primary => (
            ui.accent,
            ui.accent,
            ui.base_bg,
            ui.accent_hover,
            ui.base_bg,
        ),
        // 次操作：卡片底 + 提亮档边（约定④；主程序旧实现用 border 暗档，
        // 迁移时统一到本口径），hover 换行 hover 底、文字不变。
        ButtonKind::Secondary => (ui.card_bg, ui.border_strong, ui.text, ui.hover_bg, ui.text),
    };
    if disabled {
        // 禁用：底色退到卡片底（主/次同底，只留文字与边的退隐差异）。
        // 挂同款 id 保持 Stateful<Div> 返回类型一致；无监听即无交互。
        return div()
            .id(id)
            .h(px(H_CTRL))
            .min_w(px(36.0))
            .flex()
            .items_center()
            .justify_center()
            .px_3()
            .rounded_sm()
            .border_1()
            // 禁用边走 border_strong 提亮档：border 暗档压黑底上近隐形；
            // 退隐差异由文字（text_disabled）与无 hover/按压承担
            .border_color(ui.border_strong)
            .bg(ui.card_bg)
            .text_color(ui.text_disabled)
            .child(label.into());
    }
    div()
        .id(id)
        .h(px(H_CTRL))
        .min_w(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(text)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, listener)
        .child(label.into())
        .hover(move |style| style.bg(hover_bg).text_color(hover_text))
        // 按压态：按下瞬间沉入按压底（Stateful div 才有 active 细化）。
        .active(move |style| style.bg(ui.active_bg))
}

/// 图标钮统一工厂：方形（边长 `spec.size`）+ SVG 线性图标（尺寸随边长取档），
/// 静止透明底、hover 浮现底色并提亮图标，指针光标内置。
///
/// `id` 须窗口内唯一：用作 group 名（hover 时容器与 SVG 联动变色，
/// GroupHitboxes 按名全局解析，同名会互相覆盖）；列表循环内必须带条目键
/// （如 `format!("server-edit-{name}")`）。
pub fn icon_button(
    id: impl Into<SharedString>,
    icon: &'static str,
    ui: &UiPalette,
    spec: IconButtonSpec,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let id = id.into();
    let (icon_rest, icon_hover) = if spec.disabled {
        (ui.text_disabled, ui.text_disabled)
    } else if spec.danger_hover {
        (ui.text_secondary, ui.danger)
    } else {
        (ui.text_secondary, ui.text)
    };
    let mut button = div()
        .group(id.clone())
        .size(px(spec.size))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .child(
            svg()
                .path(icon)
                .size(px(icon_size_for(spec.size)))
                .text_color(icon_rest)
                .group_hover(id, move |style| style.text_color(icon_hover)),
        );
    if spec.disabled {
        // 禁用：无 hover/按压/指针，不挂监听。
        return button;
    }
    button = button
        .cursor_pointer()
        .hover(move |style| style.bg(ui.hover_bg))
        .on_mouse_down(MouseButton::Left, listener);
    if let Some(reveal_group) = spec.reveal_on_group {
        // 随组显现：默认全透明，悬停所在组（窗格/标签行）才浮现。
        button = button
            .opacity(0.0)
            .group_hover(reveal_group, |style| style.opacity(1.0));
    }
    button
}

/// 菜单项工厂：行高 [`H_MENU`]（26px）/ 字号 [`TEXT_BODY_SM`]（12px），
/// 水平内边距 px_3。禁用项置灰（text_muted）且不可点、无 hover；启用态
/// hover 浮现行底；`danger` 项文字用 `ui.danger`（删除类动作）。
/// 收起菜单等副作用由 `listener` 自理（工厂不感知具体视图）；点击不穿透
/// （stop_propagation），避免误触底下的终端/标签。
pub fn menu_item(
    label: impl Into<SharedString>,
    ui: &UiPalette,
    danger: bool,
    disabled: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let text = if danger { ui.danger } else { ui.text };
    let base = div()
        .h(px(H_MENU))
        .px_3()
        .flex()
        .items_center()
        .text_size(px(TEXT_BODY_SM));
    if disabled {
        base.text_color(ui.text_muted).child(label.into())
    } else {
        base.text_color(text)
            .cursor_pointer()
            .hover(move |style| style.bg(ui.hover_bg))
            .on_mouse_down(MouseButton::Left, move |event, window, app| {
                // 菜单项点击不穿透（主程序菜单同口径）。
                app.stop_propagation();
                listener(event, window, app);
            })
            .child(label.into())
    }
}

/// 浮层阴影（右键菜单/弹窗/浮面板的立体层级）：双层黑色柔影，
/// 字面值取 gpui 宏预设 `shadow_xl`（gpui-macros-0.2.2 styles.rs:509-525）。
/// 阴影是中性黑、与主题无关，故无参；用法 `.shadow(overlay_shadow())`
/// （`Styled::shadow` 收 `Vec<BoxShadow>`）。行级控件不加阴影。
pub fn overlay_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: hsla(0., 0., 0., 0.1),
            offset: point(px(0.), px(20.)),
            blur_radius: px(25.),
            spread_radius: px(-5.),
        },
        BoxShadow {
            color: hsla(0., 0., 0., 0.1),
            offset: point(px(0.), px(8.)),
            blur_radius: px(10.),
            spread_radius: px(-6.),
        },
    ]
}
