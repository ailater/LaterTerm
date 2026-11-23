//! SPDX-License-Identifier: MIT OR Apache-2.0

//! 语义色板：从主题 [`ThemeColors`]（19 个 `0xRRGGBB`）派生全部 UI 语义色。
//!
//! 派生原则与主程序 `workspace.rs` 的 `UiColors::from_theme` 一致：
//! 底 = background 的亮度变体；正文 = foreground；强调 = blue；危险 = red；
//! 成功 = green；次要/弱化文字与边框系 = bright_black。提亮/压暗用 HSL
//! 亮度平移（保证任意主题下可读性），两色混合按权重线性插值。
//!
//! 字段一律存 [`gpui::Hsla`]（含 alpha 的 `hover_tint`/`overlay_scrim` 与
//! 不透明色同型，控件工厂直接 `.bg(field)` / `.border_color(field)`），
//! 不透明色的 alpha 恒为 1.0，与 `rgb(0x…)` 构造逐位等值。
//!
//! 边框双档语义（约定④，全文见 [`crate::tokens`]）：
//! - [`UiPalette::border`]（暗档）：仅侧栏既有口径分隔与禁用态退隐；
//! - [`UiPalette::border_strong`]（提亮档）：可交互控件常态边、浮层边、
//!   分割线、输入框边。

use gpui::Hsla;
use laterterm_config::theme::ThemeColors;

/// `0xRRGGBB` + 8bit alpha → [`Hsla`]。alpha 语义：`0x1F` ≈ 12% 微提示底，
/// `0xFF` = 不透明。与 `rgba(0xRRGGBBAA)` 等价，仅换 Hsla 出参方便直填样式。
///
/// 消费点：[`UiPalette::from_theme`] 的 `hover_tint`/`overlay_scrim` 等带
/// 透明度字段，以及需要局部微调透明度的调用方。
pub fn tint(color: u32, alpha: u8) -> Hsla {
    gpui::rgba((color << 8) | alpha as u32).into()
}

/// 带透明度的语义色：对已是 [`Hsla`] 的色板字段**只换 alpha、不动 RGB**。
///
/// 色板字段存 Hsla（含 f32 通道），走 u32 往返会有精度折损；本函数在 Hsla
/// 域内直接覆写 alpha，与 `rgba((RGB << 8) | alpha)` 语义逐位等价。
/// 消费点：调用方的「语义色 + 透明度」组合，如聚焦窗格顶条
/// `tint_hsla(ui.accent, 0xB3)`、toast 底 `tint_hsla(ui.card_bg, 0xEB)`。
pub fn tint_hsla(color: Hsla, alpha: u8) -> Hsla {
    Hsla {
        a: alpha as f32 / 255.0,
        ..color
    }
}

/// 语义色板（`Copy`，按当前主题一次性派生，渲染热路径零分配）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiPalette {
    /// 根容器/标题栏底色（background 压暗 6%，比边栏更深，拉开层次）。
    pub base_bg: Hsla,
    /// 终端区底色（主题 background 原色，不做亮度调整）。
    pub terminal_bg: Hsla,
    /// 边栏底色（background 提亮 4%）。
    pub panel_bg: Hsla,
    /// 卡片底色（background 提亮 8%）。
    pub card_bg: Hsla,
    /// 浮层卡片底（background 提亮 10%）：模态弹窗、命令面板弹层等
    /// 比普通卡片再高一层的承载面（消费点：modal_dialog 卡底）。
    pub elevated_bg: Hsla,
    /// 行 hover 底（background 提亮 12%）。
    pub hover_bg: Hsla,
    /// 按压态底（background 提亮 16%）：可点行/钮 `.active(..)` 的瞬间反馈
    /// （消费点：controls::ui_button 按压、侧栏会话行按压）。
    pub active_bg: Hsla,
    /// 选中行底（blue 与 background 按 0.28 权重混合）。
    pub selected_bg: Hsla,
    /// 边框/分隔线**暗档**（bright_black 压暗 20%）：仅侧栏既有口径分隔与
    /// 禁用态退隐，可交互控件不得再用它（约定④）。
    pub border: Hsla,
    /// 边框/分隔线**提亮档**（bright_black 提亮 12%）：可交互控件常态边、
    /// 浮层（菜单/弹窗）边、分割线、输入框边一律用它（约定④）。
    /// 提亮步长与 `hover_bg` 同为 +12%，保证边框在 card_bg/hover_bg 之上
    /// 仍可辨。
    pub border_strong: Hsla,
    /// 正文（foreground 原色）。
    pub text: Hsla,
    /// 次要文字（bright_black 提亮 25%）。
    pub text_secondary: Hsla,
    /// 弱化文字（bright_black 原色）。与主程序现 `UiColors::text_muted`
    /// 口径一致（settings 侧切主题时随之变化属有意调整）。
    pub text_muted: Hsla,
    /// 禁用文字（bright_black 压暗 8%）：禁用态控件文字，弱于
    /// text_muted 一档但保证在 card_bg/黑 scrim 底上可辨——压暗 30%
    /// 会近乎隐形（TN 0x06070A），退隐不得退化成控件消失
    /// （消费点：controls 工厂 disabled 分支、termview 搜索栏、sftp chip）。
    pub text_disabled: Hsla,
    /// 强调色（blue 原色）：主操作钮、选中态、焦点环。
    pub accent: Hsla,
    /// 强调色 hover（blue 提亮 10% L）：主操作钮悬停底（消费点：ui_button Primary）。
    pub accent_hover: Hsla,
    /// 危险/删除（red 原色）。
    pub danger: Hsla,
    /// 警示（yellow 原色）：Quake 待确认等中间态（消费点：quake::text_color）。
    pub warning: Hsla,
    /// 成功提示（green 原色）。
    pub success: Hsla,
    /// 悬停微提示底（white @ 12% alpha）：覆盖在任意底色上的轻 hover 提亮。
    /// 基色取 `colors.white`（比 `hover_bg` 的 background 系更中性），Tokyo
    /// Night 下 = `0xA9B1D61F`，与主程序 `settings_ui::COLOR_HOVER_TINT`
    /// 逐位等值（迁移时直接替换该常量）。
    pub hover_tint: Hsla,
    /// 浮层遮罩（welcome 等全屏覆盖层底）：background 压暗 2% + 96% 不透明。
    /// Tokyo Night 下 ≈ `0x161720 @ 0.96`，对现 welcome 硬编码
    /// `0x161722 @ 0.96`（workspace.rs:9111）仅蓝通道差 2/255，视觉等值。
    pub overlay_scrim: Hsla,
}

impl UiPalette {
    /// 从主题颜色派生语义色板。基础 13 字段公式与主程序
    /// `workspace.rs::UiColors::from_theme`（114-130 行）完全一致，
    /// 扩展字段（elevated/active/disabled/accent_hover/warning）按同族
    /// 公式派生；数值由 [`palette_tests`] 快照逐字段锁定（不依赖手算）。
    pub fn from_theme(colors: &ThemeColors) -> Self {
        Self {
            base_bg: solid(darken(colors.background, 0.06)),
            terminal_bg: solid(colors.background),
            panel_bg: solid(lighten(colors.background, 0.04)),
            card_bg: solid(lighten(colors.background, 0.08)),
            elevated_bg: solid(lighten(colors.background, 0.10)),
            hover_bg: solid(lighten(colors.background, 0.12)),
            active_bg: solid(lighten(colors.background, 0.16)),
            selected_bg: solid(mix(colors.blue, colors.background, 0.28)),
            border: solid(darken(colors.bright_black, 0.20)),
            border_strong: solid(lighten(colors.bright_black, 0.12)),
            text: solid(colors.foreground),
            text_secondary: solid(lighten(colors.bright_black, 0.25)),
            text_muted: solid(colors.bright_black),
            text_disabled: solid(darken(colors.bright_black, 0.08)),
            accent: solid(colors.blue),
            accent_hover: solid(lighten(colors.blue, 0.10)),
            danger: solid(colors.red),
            warning: solid(colors.yellow),
            success: solid(colors.green),
            hover_tint: tint(colors.white, 0x1F),
            overlay_scrim: solid(darken(colors.background, 0.02)).opacity(0.96),
        }
    }

    /// Tokyo Night 兜底色板：无主题上下文时的初值（消费点：termview
    /// 视图兜底初值；设置面板固定深色口径的回退开关）。
    pub fn tokyo_night_default() -> Self {
        Self::from_theme(&ThemeColors::tokyo_night())
    }
}

/// 不透明色：`0xRRGGBB` → alpha=1.0 的 [`Hsla`]（内部助手）。
fn solid(color: u32) -> Hsla {
    Hsla::from(gpui::rgb(color))
}

/// 当前生效色板的 gpui 全局（workspace 每帧写入，无主题句柄的独立视图
/// ——输入框/快捷键录制器等——按 `try_global` 兜底读取）。
///
/// 为什么是 Global：`CommandTextField`/`ShortcutRecorder` 等控件实体在
/// 弹窗字段懒建链路里创建，拿不到 `WorkspaceView` 句柄，逐实体透传色板
/// 会把所有构造点签名搅乱；gpui Global 恰好是「进程内单一当前值」的
/// 官方通道（`App::set_global`，app.rs:1497）。色板是 `Copy` 小结构，
/// 每帧覆写零分配；读侧 miss 时回落 Tokyo Night 兜底（纯测试窗口）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActivePalette(pub UiPalette);

impl gpui::Global for ActivePalette {}

/// 读取当前色板：无写入方（独立测试窗口/无 workspace 上下文）时回落
/// Tokyo Night，保证控件纯样式渲染永远有值。
pub fn active_palette(cx: &gpui::App) -> UiPalette {
    cx.try_global::<ActivePalette>()
        .map(|active| active.0)
        .unwrap_or_else(UiPalette::tokyo_night_default)
}

/// HSL 提亮：把 u32 RGB 转 HSL，L 通道加 `amount`，再转回 u32。
///
/// 实现与主程序 `workspace.rs:144-203` 的 `adjust_lightness` 语义一致
/// （迁移后以本 crate 为单一真值，主程序侧删除）。pub：主题派生公式
/// 的公开配套（特殊派生需求的调用方直接复用，不再本地重写）。
pub fn lighten(rgb: u32, amount: f32) -> u32 {
    adjust_lightness(rgb, amount)
}

/// [`Hsla`] 亮度平移（h/s/a 不变，L 加 `amount` 后钳制 0..1）。
/// 控件 hover 提亮用：palette 字段本就是 Hsla，直调无 u32 往返精度损失。
/// 暂无 crate 内消费点（hover 提亮已由 `accent_hover` 等预派生字段承担），
/// 作为 [`tint_hsla`] 的同族工具预留；接线时移除 allow。
#[allow(dead_code)]
pub(crate) fn lighten_hsla(color: Hsla, amount: f32) -> Hsla {
    Hsla {
        l: (color.l + amount).clamp(0.0, 1.0),
        ..color
    }
}

/// HSL 压暗（pub：[`lighten`] 的负向配套，公开口径同上）。
pub fn darken(rgb: u32, amount: f32) -> u32 {
    adjust_lightness(rgb, -amount)
}

/// 亮度调整实现（amount 正提亮负压暗，L 钳制 0..1）。
fn adjust_lightness(rgb: u32, amount: f32) -> u32 {
    let (r, g, b) = (rgb >> 16 & 0xFF, rgb >> 8 & 0xFF, rgb & 0xFF);
    let (rf, gf, bf) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let l = (max + min) / 2.0;
    let l = (l + amount).clamp(0.0, 1.0);
    let s = if (max - min).abs() < f32::EPSILON {
        0.0
    } else if l <= 0.5 {
        (max - min) / (max + min)
    } else {
        (max - min) / (2.0 - max - min)
    };
    let hue = |p: f32, q: f32, mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            return p + (q - p) * 6.0 * t;
        }
        if t < 1.0 / 2.0 {
            return q;
        }
        if t < 2.0 / 3.0 {
            return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
        }
        p
    };
    let (nr, ng, nb) = if s.abs() < f32::EPSILON {
        (l, l, l)
    } else {
        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        let h = if (max - min).abs() < f32::EPSILON {
            0.0
        } else if (max - rf).abs() < f32::EPSILON {
            ((gf - bf) / (max - min) % 6.0) / 6.0
        } else if (max - gf).abs() < f32::EPSILON {
            ((bf - rf) / (max - min) + 2.0) / 6.0
        } else {
            ((rf - gf) / (max - min) + 4.0) / 6.0
        };
        (
            hue(p, q, h + 1.0 / 3.0),
            hue(p, q, h),
            hue(p, q, h - 1.0 / 3.0),
        )
    };
    ((nr * 255.0).round() as u32) << 16
        | ((ng * 255.0).round() as u32) << 8
        | (nb * 255.0).round() as u32
}

/// 两色按权重混合（weight 为 color 的占比，1-weight 为 base 的占比）。
/// 实现与主程序 `workspace.rs:206-214` 的 `mix` 语义一致。
/// pub：主题派生公式的公开配套（同 [`lighten`]）。
pub fn mix(color: u32, base: u32, weight: f32) -> u32 {
    let (r1, g1, b1) = (color >> 16 & 0xFF, color >> 8 & 0xFF, color & 0xFF);
    let (r2, g2, b2) = (base >> 16 & 0xFF, base >> 8 & 0xFF, base & 0xFF);
    let w = weight.clamp(0.0, 1.0);
    let r = (r1 as f32 * w + r2 as f32 * (1.0 - w)).round() as u32;
    let g = (g1 as f32 * w + g2 as f32 * (1.0 - w)).round() as u32;
    let b = (b1 as f32 * w + b2 as f32 * (1.0 - w)).round() as u32;
    r << 16 | g << 8 | b
}

#[cfg(test)]
mod palette_tests {
    use super::UiPalette;
    use gpui::{Hsla, rgb};
    use laterterm_config::theme::ThemeColors;

    /// Tokyo Night 全字段快照（模式同 workspace.rs `titlebar_scrollbar_tests`
    /// 的字面量锁定）：任何派生公式改动都会在此显式留痕。
    /// 各期望值来自独立 rustc 程序执行 `adjust_lightness`/`mix` 原实现的
    /// 实际输出（/tmp/laterterm-ui-calc），非手算估算。
    #[test]
    fn tokyo_night_palette_pinned() {
        let p = UiPalette::from_theme(&ThemeColors::tokyo_night());
        // —— 13 个既有字段（公式 = workspace.rs UiColors::from_theme）——
        assert_eq!(p.base_bg, Hsla::from(rgb(0x0E0E14)));
        assert_eq!(p.terminal_bg, Hsla::from(rgb(0x1A1B26)));
        assert_eq!(p.panel_bg, Hsla::from(rgb(0x222432)));
        assert_eq!(p.card_bg, Hsla::from(rgb(0x2B2C3E)));
        assert_eq!(p.hover_bg, Hsla::from(rgb(0x33354A)));
        assert_eq!(p.selected_bg, Hsla::from(rgb(0x354161)));
        assert_eq!(p.border, Hsla::from(rgb(0x1A1D29)));
        assert_eq!(p.text, Hsla::from(rgb(0xC0CAF5)));
        assert_eq!(p.text_secondary, Hsla::from(rgb(0x888CA0)));
        assert_eq!(p.text_muted, Hsla::from(rgb(0x414868)));
        assert_eq!(p.accent, Hsla::from(rgb(0x7AA2F7)));
        assert_eq!(p.danger, Hsla::from(rgb(0xF7768E)));
        assert_eq!(p.success, Hsla::from(rgb(0x9ECE6A)));
        // —— 3 个新增字段 ——
        // 提亮档边框：bright_black +12% L
        assert_eq!(p.border_strong, Hsla::from(rgb(0x59628E)));
        // 悬停微提示底：white @ 12% alpha（= settings_ui::COLOR_HOVER_TINT）
        assert_eq!(p.hover_tint, Hsla::from(gpui::rgba(0xA9B1D61F)));
        // 浮层遮罩：background 压暗 2% @ 0.96（现 welcome 0x161722 的近似等值）
        assert_eq!(p.overlay_scrim, Hsla::from(rgb(0x161720)).opacity(0.96));
        // —— app-unified 线新增 5 字段（期望值来自 /tmp/lt-calc 独立 rustc
        // 程序执行同一 adjust_lightness 实现的实际输出，非手算）——
        assert_eq!(p.elevated_bg, Hsla::from(rgb(0x2F3144)));
        assert_eq!(p.active_bg, Hsla::from(rgb(0x3B3D56)));
        assert_eq!(p.text_disabled, Hsla::from(rgb(0x31374F)));
        assert_eq!(p.accent_hover, Hsla::from(rgb(0xAAC4FA)));
        assert_eq!(p.warning, Hsla::from(rgb(0xE0AF68)));
    }

    /// `tint_hsla` 只换 alpha、RGB 不动：与 `rgba((RGB << 8) | alpha)` 逐位等价
    /// （同一 RGB 基色 + 同一 alpha 字节走两条构造路径必须相等）。
    #[test]
    fn tint_hsla_matches_rgba_literal() {
        let accent = Hsla::from(rgb(0x7AA2F7));
        assert_eq!(
            super::tint_hsla(accent, 0xB3),
            Hsla::from(gpui::rgba(0x7AA2F7B3))
        );
        // alpha 覆写不污染原色
        assert_eq!(accent, Hsla::from(rgb(0x7AA2F7)));
    }

    /// 兜底初值 = Tokyo Night 派生（防手写偏差）。
    #[test]
    fn tokyo_night_default_matches_from_theme() {
        assert_eq!(
            UiPalette::tokyo_night_default(),
            UiPalette::from_theme(&ThemeColors::tokyo_night())
        );
    }

    /// `tint` 出参与 `rgba(0xRRGGBBAA)` 构造逐位等值（alpha 走同一 f32 通路）。
    #[test]
    fn tint_matches_rgba_literal() {
        assert_eq!(
            super::tint(0xA9B1D6, 0x1F),
            Hsla::from(gpui::rgba(0xA9B1D61F))
        );
        assert_eq!(super::tint(0x1A1B26, 0xFF), Hsla::from(rgb(0x1A1B26)));
    }

    /// 不同主题下派生确实随主题变化（防「写死 Tokyo Night」回归）。
    #[test]
    fn palette_follows_theme() {
        let tn = UiPalette::from_theme(&ThemeColors::tokyo_night());
        let storm = UiPalette::from_theme(&ThemeColors::tokyo_night_storm());
        // background 不同 → 底色系不同
        assert_ne!(tn.terminal_bg, storm.terminal_bg);
        assert_ne!(tn.base_bg, storm.base_bg);
        // ANSI 色相同的字段保持一致
        assert_eq!(tn.accent, storm.accent);
    }
}
