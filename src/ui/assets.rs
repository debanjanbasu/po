//! Asset source for the gpui app (the port of `asset!` + manganis).
//!
//! gpui loads assets through [`gpui::AssetSource`]; desktop reads from disk,
//! web fetches or embeds. The logo is small enough to embed.

pub struct EmbeddedAssets;

impl gpui::AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match path {
            "logo.svg" => Ok(Some(std::borrow::Cow::Borrowed(LOGO_SVG))),
            _ => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    Ok(std::fs::read(path).ok().map(std::borrow::Cow::Owned))
                }
                #[cfg(target_arch = "wasm32")]
                {
                    Ok(None)
                }
            }
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<gpui::SharedString>> {
        let entries: Vec<gpui::SharedString> =
            std::fs::read_dir(path).map_or(Vec::<gpui::SharedString>::new(), |entries| {
                entries
                    .filter_map(|entry| {
                        Some(entry.ok()?.path().to_string_lossy().into_owned().into())
                    })
                    .collect()
            });
        Ok(entries)
    }
}

/// The brand logo, embedded at compile time (was `asset!("/assets/logo.svg")`).
pub const LOGO_SVG: &[u8] = include_bytes!("../../assets/logo.svg");

/// The logo for `img()` (embedded; web has no filesystem).
pub const fn logo() -> &'static [u8] {
    LOGO_SVG
}
