//! The title bar uses the same design embedded in the Windows icon resources.
use std::sync::{Arc, LazyLock};
use gpui_kit::{Image, ImageFormat};

static ICON: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../assets/app-64.png").to_vec()))
});

pub fn image() -> Arc<Image> { ICON.clone() }
