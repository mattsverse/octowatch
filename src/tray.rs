use anyhow::Result;
use gpui::App;
use semver::Version;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::store::PendingReview;

const SHOW_ID: &str = "show";
const QUIT_ID: &str = "quit";
const REFRESH_ID: &str = "refresh";
const RESTART_ID: &str = "restart";
const CHECK_UPDATES_ID: &str = "check-updates";
const INSTALL_ID: &str = "install-update";
/// Menu ids for pull requests are their URL behind this prefix, so a click
/// carries everything needed to open it.
const OPEN_PREFIX: &str = "open:";
const MAX_TITLE_CHARS: usize = 60;

/// What the tray menu offers about updates.
pub enum UpdateItem<'a> {
    Check,
    Checking,
    Available(&'a Version),
    Installing(&'a Version),
    Ready(&'a Version),
}

/// The menu bar icon: its menu lists the pull requests waiting on a review,
/// and its title shows how many there are.
pub struct Tray {
    icon: TrayIcon,
}

impl Tray {
    pub fn new(pending: &[PendingReview]) -> Result<Self> {
        let builder = TrayIconBuilder::new();
        // Templates are a macOS notion; elsewhere the icon is drawn as is.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(tray_icon());
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(tray_icon());
        let icon = builder
            .with_tooltip("Octowatcher")
            .with_menu(Box::new(build_menu(pending, UpdateItem::Check)?))
            .build()?;
        let tray = Self { icon };
        tray.set_count(pending.len());
        Ok(tray)
    }

    pub fn update(&self, pending: &[PendingReview], update: UpdateItem) -> Result<()> {
        self.icon
            .set_menu(Some(Box::new(build_menu(pending, update)?)));
        self.set_count(pending.len());
        Ok(())
    }

    fn set_count(&self, count: usize) {
        self.icon.set_title((count > 0).then(|| count.to_string()));
    }
}

/// Routes clicks on the tray menu for as long as the app runs.
pub fn listen(cx: &mut App) {
    let (tx, rx) = async_channel::unbounded();
    // The handler fires on the main thread from AppKit; hand events over to
    // the app's executor instead of acting from inside the callback.
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        tx.send_blocking(event).ok();
    }));
    cx.spawn(async move |cx| {
        while let Ok(event) = rx.recv().await {
            let id = event.id.as_ref();
            let handled = if id == SHOW_ID {
                cx.update(crate::show_window)
            } else if id == REFRESH_ID {
                cx.update(crate::refresh)
            } else if id == RESTART_ID {
                cx.update(|cx| cx.restart())
            } else if id == CHECK_UPDATES_ID {
                cx.update(crate::check_for_updates)
            } else if id == INSTALL_ID {
                cx.update(crate::install_update)
            } else if id == QUIT_ID {
                cx.update(|cx| cx.quit())
            } else if let Some(url) = id.strip_prefix(OPEN_PREFIX) {
                cx.update(|cx| cx.open_url(url))
            } else {
                Ok(())
            };
            if handled.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn build_menu(pending: &[PendingReview], update: UpdateItem) -> Result<Menu> {
    let menu = Menu::new();
    if pending.is_empty() {
        menu.append(&MenuItem::new(
            "Nothing waiting on your review.",
            false,
            None,
        ))?;
    }
    for pr in pending {
        let mut label = format!("{}#{}: {}", pr.repo_label(), pr.number, truncate(&pr.title));
        if pr.rereview {
            label.push_str("  (re-review)");
        }
        if pr.is_draft {
            label.push_str("  (draft)");
        }
        menu.append(&MenuItem::with_id(
            format!("{OPEN_PREFIX}{}", pr.url),
            label,
            true,
            None,
        ))?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(REFRESH_ID, "Refresh Now", true, None))?;
    let item = match update {
        UpdateItem::Check => MenuItem::with_id(CHECK_UPDATES_ID, "Check for Updates…", true, None),
        UpdateItem::Checking => MenuItem::new("Checking for Updates…", false, None),
        UpdateItem::Available(version) => {
            MenuItem::with_id(INSTALL_ID, format!("Update to {version}"), true, None)
        }
        UpdateItem::Installing(version) => {
            MenuItem::new(format!("Installing {version}…"), false, None)
        }
        UpdateItem::Ready(version) => MenuItem::with_id(
            RESTART_ID,
            format!("Restart to Update to {version}"),
            true,
            None,
        ),
    };
    menu.append(&item)?;
    menu.append(&MenuItem::with_id(SHOW_ID, "Open Octowatcher", true, None))?;
    menu.append(&MenuItem::with_id(QUIT_ID, "Quit Octowatcher", true, None))?;
    Ok(menu)
}

fn truncate(title: &str) -> String {
    if title.chars().count() <= MAX_TITLE_CHARS {
        return title.to_string();
    }
    let cut: String = title.chars().take(MAX_TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The pull request glyph with eyes for commits, rendered from
/// `assets/tray.svg`. Black on transparent, used as a template so
/// macOS tints it for light and dark bars. Linux gets a white outline, since
/// the panel's theme can differ from both the desktop and the app preference.
fn tray_icon() -> Icon {
    let (rgba, width, height) = tray_pixels();
    #[cfg(not(target_os = "macos"))]
    let rgba = outlined_glyph(&rgba, width as usize, height as usize);
    Icon::from_rgba(rgba, width, height).expect("icon buffer matches its size")
}

fn tray_pixels() -> (Vec<u8>, u32, u32) {
    const PNG: &[u8] = include_bytes!("../assets/tray.png");
    let mut decoder = png::Decoder::new(std::io::Cursor::new(PNG));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .expect("bundled tray icon is a valid PNG");
    let mut rgba = vec![
        0;
        reader
            .output_buffer_size()
            .expect("tray icon fits in memory")
    ];
    let info = reader
        .next_frame(&mut rgba)
        .expect("bundled tray icon decodes");
    assert_eq!(
        info.color_type,
        png::ColorType::Rgba,
        "tray icon must be RGBA"
    );
    rgba.truncate(info.buffer_size());
    (rgba, info.width, info.height)
}

/// Composite the black glyph over a two-pixel white halo (about one pixel at
/// usual panel sizes). Preserve antialiasing and transparency outside the halo.
#[cfg(any(test, not(target_os = "macos")))]
fn outlined_glyph(source: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut result = source.to_vec();
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let mut halo = 0u8;
            for ny in y.saturating_sub(2)..=(y + 2).min(height - 1) {
                for nx in x.saturating_sub(2)..=(x + 2).min(width - 1) {
                    halo = halo.max(source[(ny * width + nx) * 4 + 3]);
                }
            }
            let alpha = source[offset + 3] as f64 / 255.;
            let white = halo as f64 / 255. * (1. - alpha);
            let output_alpha = alpha + white;
            if output_alpha > 0. {
                for channel in 0..3 {
                    result[offset + channel] =
                        ((source[offset + channel] as f64 * alpha + 255. * white) / output_alpha)
                            .round() as u8;
                }
                result[offset + 3] = (output_alpha * 255.).round() as u8;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_glyph_has_contrasting_strokes_for_light_and_dark_panels() {
        let (source, width, height) = tray_pixels();
        let outlined = outlined_glyph(&source, width as usize, height as usize);
        assert_eq!(outlined.len(), source.len());
        let mut black = 0;
        let mut white = 0;
        let mut transparent = 0;
        for (original, pixel) in source
            .as_chunks::<4>()
            .0
            .iter()
            .zip(outlined.as_chunks::<4>().0.iter())
        {
            if original[3] == 255 {
                assert_eq!(pixel, original, "opaque glyph details are preserved");
            }
            match pixel {
                [0, 0, 0, 255] => black += 1,
                [255, 255, 255, 255] => white += 1,
                [_, _, _, 0] => transparent += 1,
                _ => {}
            }
        }
        assert!(black > 100, "black stroke contrasts against light panels");
        assert!(white > 100, "white outline contrasts against dark panels");
        assert!(transparent > 100, "no opaque background box");
    }

    #[test]
    fn outline_preserves_antialiasing_at_image_edges() {
        let source = [0, 0, 0, 128, 0, 0, 0, 0];
        let outlined = outlined_glyph(&source, 2, 1);
        assert_eq!(&outlined[4..], &[255, 255, 255, 128]);
        assert!(outlined[3] > 128 && outlined[3] < 255);
        assert!(outlined[0] > 0 && outlined[0] < 255);
    }
}
