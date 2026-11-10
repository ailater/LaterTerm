//! 品牌资源与窗口身份：编译期嵌入，不依赖当前工作目录。

use gpui::{
    AssetSource, Image, ImageFormat, IntoElement, ParentElement, SharedString, Styled, StyledImage,
    div, img, px,
};
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

pub(crate) const APP_ID: &str = "laterterm";
const LOGO_BYTES: &[u8] = include_bytes!("../../../assets/icons/png/laterterm-128.png");

/// 图标资源表：`icon_button` 通过 `svg().path("icons/xxx.svg")` 引用，
/// 路径须与本表键一致。全部为 Lucide 线性风（24 网格 / stroke 2 / 圆端点），
/// 渲染色由 `svg().text_color()` 统一染（文件内 stroke 颜色仅占位）。
const ICONS: &[(&str, &[u8])] = &[
    (
        "icons/close.svg",
        include_bytes!("../../../assets/icons/svg/close.svg"),
    ),
    (
        "icons/maximize.svg",
        include_bytes!("../../../assets/icons/svg/maximize.svg"),
    ),
    (
        "icons/menu.svg",
        include_bytes!("../../../assets/icons/svg/menu.svg"),
    ),
    (
        "icons/minus.svg",
        include_bytes!("../../../assets/icons/svg/minus.svg"),
    ),
    (
        "icons/panel-right.svg",
        include_bytes!("../../../assets/icons/svg/panel-right.svg"),
    ),
    (
        "icons/pen.svg",
        include_bytes!("../../../assets/icons/svg/pen.svg"),
    ),
    (
        "icons/pin.svg",
        include_bytes!("../../../assets/icons/svg/pin.svg"),
    ),
    (
        "icons/pin-off.svg",
        include_bytes!("../../../assets/icons/svg/pin-off.svg"),
    ),
    (
        "icons/plus.svg",
        include_bytes!("../../../assets/icons/svg/plus.svg"),
    ),
    (
        "icons/settings.svg",
        include_bytes!("../../../assets/icons/svg/settings.svg"),
    ),
    (
        "icons/split-h.svg",
        include_bytes!("../../../assets/icons/svg/split-h.svg"),
    ),
    (
        "icons/split-v.svg",
        include_bytes!("../../../assets/icons/svg/split-v.svg"),
    ),
    (
        "icons/trash.svg",
        include_bytes!("../../../assets/icons/svg/trash.svg"),
    ),
];

/// 应用级资源源：挂到 `Application::with_assets`，为 SVG 图标提供字节。
pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(name, _)| SharedString::from(*name))
            .filter(|name| name.starts_with(path))
            .collect())
    }
}

pub(crate) fn logo_image() -> Arc<Image> {
    static LOGO: OnceLock<Arc<Image>> = OnceLock::new();
    LOGO.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, LOGO_BYTES.to_vec())))
        .clone()
}

pub(crate) fn titlebar_logo() -> impl IntoElement {
    use gpui::InteractiveElement;

    div()
        .debug_selector(|| "brand-logo".into())
        .size(px(20.0))
        .flex_shrink_0()
        .child(
            img(logo_image())
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .with_fallback(|| div().size_full().into_any_element()),
        )
}

/// 设置面板「关于」卡的品牌标：40px 大尺寸版（同一编译期嵌入 PNG，
/// ObjectFit::Contain 防非方形拉伸；不改动标题栏 20px 版）。
pub(crate) fn about_logo() -> impl IntoElement {
    use gpui::InteractiveElement;

    div()
        .debug_selector(|| "about-logo".into())
        .size(px(40.0))
        .flex_shrink_0()
        .child(
            img(logo_image())
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .with_fallback(|| div().size_full().into_any_element()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_is_embedded_and_cached_with_desktop_identity() {
        assert_eq!(APP_ID, "laterterm");
        assert!(LOGO_BYTES.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&LOGO_BYTES[16..24], &[0, 0, 0, 128, 0, 0, 0, 128]);
        assert!(Arc::ptr_eq(&logo_image(), &logo_image()));
    }
}
