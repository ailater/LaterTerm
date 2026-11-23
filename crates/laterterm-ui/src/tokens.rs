//! SPDX-License-Identifier: MIT OR Apache-2.0

//! UI 设计 token：动效时长与四条硬约定。
//!
//! 本模块是几何/动效写法的单一真值。约定一经立此存照，app 与 termview
//! 迁移时必须对齐；新增 token 前先找消费点，**无主 token 一律不定义**。
//!
//! # 四条硬约定
//!
//! **① 圆角只用宏预设，不写 px 圆角。**
//! 全应用圆角一律 `.rounded_sm()` / `.rounded_md()` / `.rounded_full()` 三个
//! gpui 宏预设，禁止 `.rounded(px(4.0))` 这类手写 px 圆角。rems 语义：
//! `rounded_sm` = `rems(0.25)`、`rounded_md` = `rems(0.375)`，rem=16px 时即
//! 4px/6px，根字号/rem 变化时圆角随全局缩放（4K 缩放下不糊的前提之一）；
//! `rounded_full` = `px(9999.)`，专用于胶囊/圆形。出处：gpui-macros-0.2.2
//! `src/styles.rs:658,1273`（宏生成，未 vendor；本机 cargo registry 可查证：
//! `rounded_sm → rems(0.25) = "4px (0.25rem)"`、`rounded_md → rems(0.375) =
//! "6px (0.375rem)"`、`rounded_full → px(9999.) = "9999px"`）。
//!
//! **② 「过渡」一律走 `Animation`，禁止自起线程/timer 刷帧。**
//! 本版 gpui 无 CSS 式 transition 插值：hover/active 是即时
//! `StyleRefinement::refine`（vendor/gpui/src/elements/div.rs:2506-2509，
//! hover 命中即整块 refine，已核源码），写 `.hover(|s| s.bg(...))` 只会
//! 瞬变，不会渐变。需要渐变时一律：
//! `Animation::new(DUR_*).with_easing(...)` 配合
//! `el.with_animation(id, anim, |el, delta| ...)`（AnimationExt 签名见
//! vendor/gpui/src/elements/animation.rs:54；`Animation::new` 默认 oneshot +
//! linear，循环加 `.repeat()`）。教训：标签拖拽滑动曾用 per-frame timer 衰减 +
//! `with_animation` 重启混用导致闪动（57ddb19 引入、8e3dd05 撤下），
//! 重启风暴根因即动画随状态重挂载反复重置。
//!
//! **③ 缓动签名分两类，注意是否带调用括号。**
//! - 即时函数（`f32 -> f32`，传函数名**不带括号**）：
//!   `linear` / `quadratic` / `ease_in_out`（animation.rs:213-223）。
//!   示例：`.with_easing(linear)`
//! - 构造器（返回 `impl Fn(f32) -> f32`，**必须带调用括号**）：
//!   `ease_out_quint()`（animation.rs:233，快出缓停，位移/展开首选）、
//!   `pulsating_between(min, max)`（animation.rs:249，循环脉动 alpha）。
//!   示例：`.with_easing(ease_out_quint())`、
//!   `.with_easing(pulsating_between(0.4, 1.0))`
//!
//! **④ 边框双档语义（字段见 [`crate::palette::UiPalette`]）。**
//! - `border_strong`（提亮档，bright_black +12% L）：可交互控件常态边、
//!   浮层（菜单/弹窗）边、分割线、输入框边一律用它，保证深色主题下可见；
//!   **禁用态也不例外**——禁用 = `border_strong` 边 + `text_disabled` 字，
//!   退隐差异全由文字承担（button/搜索栏实现同此口径）；
//! - `border`（暗档，bright_black -20% L，现 `UiColors::border` 同值）：
//!   仅用于侧栏既有口径分隔，不再用于可交互控件（含禁用态）。

use gpui::{Pixels, SharedString, px};
use std::time::Duration;

// ───────────────────── 间距阶梯 ─────────────────────

/// 间距 token（px）：4/8/12/16/20/24 六档，替代散写的 gap/px 魔法数。
/// 消费点：`gap_1()..gap_3()`（快捷键录制行、命令输入框内距等）与其余
/// `SPACE_*` 直接取值处（app 层控件迁移按需引用）。
pub const SPACE_1: f32 = 4.0;
pub const SPACE_2: f32 = 8.0;
pub const SPACE_3: f32 = 12.0;
pub const SPACE_4: f32 = 16.0;
pub const SPACE_5: f32 = 20.0;
pub const SPACE_6: f32 = 24.0;

/// 常用间距的便捷函数（返回 [`gpui::Pixels`]，直填 `.gap(...)` / `.px(...)`）。
/// gap_1=4、gap_2=8、gap_3=12（一/二/三档）。
pub fn gap_1() -> Pixels {
    px(SPACE_1)
}

pub fn gap_2() -> Pixels {
    px(SPACE_2)
}

pub fn gap_3() -> Pixels {
    px(SPACE_3)
}

// ───────────────────── 控件高度阶梯 ─────────────────────

/// 标准控件高 30px：`ui_button`/`icon_button` 大档/输入框
/// （消费点：controls 工厂、command_text_field 输入框）。
pub const H_CTRL: f32 = 30.0;

/// 菜单/紧凑行高 26px：`menu_item` 与模态/标签行的关闭钮
/// （消费点：controls::menu_item、modal 右上角 icon_button）。
pub const H_MENU: f32 = 26.0;

/// 小控件高 24px：termview 搜索栏按钮（termview-shell 线消费，先立档防散写）。
pub const H_CTRL_SM: f32 = 24.0;

/// 设置面板导航行高 32px（消费点：app 层 settings 导航行）。
pub const H_NAV: f32 = 32.0;

/// 侧栏会话行高 38px（消费点：app 层 left_sidebar 会话行，原魔法数）。
pub const H_ROW: f32 = 38.0;

// ───────────────────── 图标尺寸阶梯 ─────────────────────

/// 图标大档 16px：30px 控件（标题栏/侧栏头图标钮）内的 SVG 尺寸。
pub const ICON_LG: f32 = 16.0;
/// 图标中档 14px：26px 控件（模态关闭钮）内的 SVG 尺寸。
pub const ICON_MD: f32 = 14.0;
/// 图标小档 12px：22px 行内控件（列表行编辑/删除/关闭）内的 SVG 尺寸。
pub const ICON_SM: f32 = 12.0;

/// 按控件边长取图标档：30→16、26→14、22→12（其余就近取小档）。
/// 唯一消费点：[`crate::controls::icon_button`] 内部，禁止调用方自算。
pub fn icon_size_for(control_size: f32) -> f32 {
    if control_size >= 30.0 {
        ICON_LG
    } else if control_size >= 26.0 {
        ICON_MD
    } else {
        ICON_SM
    }
}

// ───────────────────── 字阶 ─────────────────────

/// 辅助说明/徽标字号 11px。
pub const TEXT_CAPTION: f32 = 11.0;
/// 菜单项/次要行字号 12px（`menu_item`、toast、行内说明）。
pub const TEXT_BODY_SM: f32 = 12.0;
/// 正文默认字号 13px：根容器缺省字阶（原 GPUI 默认 16px 的 app 层替代）。
pub const TEXT_BODY: f32 = 13.0;
/// 区块标题字号 16px。
pub const TEXT_TITLE: f32 = 16.0;
/// 主视觉标题字号 24px（消费点：welcome 品牌标题，原 28px 的收敛档）。
pub const TEXT_HERO: f32 = 24.0;

/// 等宽字体族唯一取口：跟随用户配置的终端字体，空/纯空白回退 `"monospace"`
/// （与 termview `TerminalConfig::font_family` 的默认回退同值，view.rs:145/256）。
/// 消费点：弹窗命令预览、快捷键键帽 chip——与终端等宽同源，码点对齐。
pub fn mono_family(configured: &str) -> SharedString {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        "monospace".into()
    } else {
        trimmed.to_owned().into()
    }
}

/// 微交互时长（160ms）：位移/淡入类小过渡。
///
/// 取值理由：历史标签滑动微动画的字面值（57ddb19：
/// `Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint())`），
/// 160ms 是「跟手但可感知」的下限档。消费点：约定②的过渡实现；
/// 该滑动动画本体已随 8e3dd05 撤下（重启风暴），时长口径保留供后续线复用。
pub const DUR_UI_FAST: Duration = Duration::from_millis(160);

/// 标准过渡时长（240ms）：面板/浮层级过渡（弹出、展开、收起）。
///
/// 取值理由：大于微交互一档（160→240ms），面积越大的元素过渡越长才有
/// 「稳」感；再长（>300ms）交互就开始显得迟钝。消费点：toast/welcome
/// 卡片入场淡入（`ease_out_quint()` 配套）等一次性过渡。
pub const DUR_UI_NORMAL: Duration = Duration::from_millis(240);

/// 循环脉动时长（1200ms）：`pulsating_between` 配套的呼吸/等待指示。
///
/// 取值理由：循环动画需要慢于一切一次性过渡，~1.2s/圈接近静息呼吸节奏，
/// 长时间等待不焦躁；配 `Animation::new(DUR_UI_PULSE).repeat()` 使用。
/// 消费点：快捷键录制中的呼吸边框（`pulsating_between` 配套）。
pub const DUR_UI_PULSE: Duration = Duration::from_millis(1200);
