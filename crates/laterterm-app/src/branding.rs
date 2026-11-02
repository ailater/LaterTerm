//! 品牌资源与窗口身份：编译期嵌入，不依赖当前工作目录。

use gpui::{Image, ImageFormat, IntoElement, ParentElement, Styled, StyledImage, div, img, px};
use std::sync::{Arc, OnceLock};

pub(crate) const APP_ID: &str = "laterterm";
const LOGO_BYTES: &[u8] = include_bytes!("../../../assets/icons/png/laterterm-128.png");

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
