//! Embedded SVG icons (rendered by GPUI as tintable masks).

use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

pub struct Assets;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        fn icon_bytes(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("icons/", $name, ".svg") => Some(include_bytes!(concat!("../assets/icons/", $name, ".svg"))),)*
                _ => None,
            }
        }
        const ICON_PATHS: &[&str] = &[$(concat!("icons/", $name, ".svg")),*];
    };
}

icons!("logo", "refresh", "back", "home", "recents", "record", "stop", "volume", "mute", "rotate", "plug", "phone", "usb", "camera");

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(icon_bytes(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICON_PATHS.iter().filter(|p| p.starts_with(path)).map(|p| SharedString::from(*p)).collect())
    }
}
