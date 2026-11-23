//! LaterTerm UI 设计 token 与共享控件单一真值。
//!
//! 颜色一律由 [`palette::UiPalette::from_theme`] 从
//! [`laterterm_config::theme::ThemeColors`] 派生（主题切换即整体换肤），
//! **禁止新增裸 hex**；几何圆角走 gpui 宏预设（见 `tokens` 模块四条硬约定），
//! 过渡动效走 `Animation` + `with_animation`，禁止自起线程/timer 刷帧。
//!
//! 消费方式：`laterterm-app` / `laterterm-termview` 在 Cargo.toml 里
//! `laterterm-ui.workspace = true`，然后 `use laterterm_ui::UiPalette` 等。
//!
//! 模块划分：
//! - [`tokens`]：动效时长/间距/控件高/图标尺寸/字阶 token 与使用硬约定
//!   （圆角/过渡/缓动/边框双档）；
//! - [`palette`]：语义色板 [`UiPalette`](palette::UiPalette) 与 `tint`/
//!   `tint_hsla` 助手，基础 13 字段公式与主程序 `workspace.rs` 的
//!   `UiColors::from_theme` 完全一致，扩展字段同族公式派生；
//! - [`controls`]：统一按钮/图标钮/菜单项/浮层阴影工厂，集中管控风格。

pub mod controls;
pub mod palette;
pub mod tokens;

pub use controls::{
    ButtonKind, IconButtonSpec, icon_button, menu_item, overlay_shadow, setting_switch, ui_button,
};
pub use palette::{ActivePalette, UiPalette, active_palette, tint, tint_hsla};
