use anyhow::Result;
use gpui::App;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::store::PendingReview;

const SHOW_ID: &str = "show";
const QUIT_ID: &str = "quit";
/// Menu ids for pull requests are their URL behind this prefix, so a click
/// carries everything needed to open it.
const OPEN_PREFIX: &str = "open:";
const MAX_TITLE_CHARS: usize = 60;

/// The menu bar icon: its menu lists the pull requests waiting on a review,
/// and its title shows how many there are.
pub struct Tray {
    icon: TrayIcon,
}

impl Tray {
    pub fn new(pending: &[PendingReview]) -> Result<Self> {
        let icon = TrayIconBuilder::new()
            .with_icon_templated(eye_icon())
            .with_tooltip("Octowatcher")
            .with_menu(Box::new(build_menu(pending)?))
            .build()?;
        let tray = Self { icon };
        tray.set_count(pending.len());
        Ok(tray)
    }

    pub fn update(&self, pending: &[PendingReview]) -> Result<()> {
        self.icon.set_menu(Some(Box::new(build_menu(pending)?)));
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

fn build_menu(pending: &[PendingReview]) -> Result<Menu> {
    let menu = Menu::new();
    if pending.is_empty() {
        menu.append(&MenuItem::new("Nothing waiting on your review.", false, None))?;
    }
    for pr in pending {
        let mut label = format!("{}#{}: {}", pr.repo, pr.number, truncate(&pr.title));
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

/// An eye drawn in code: an almond outline around a filled pupil. Black on
/// transparent, used as a template so macOS tints it for light and dark bars.
fn eye_icon() -> Icon {
    const SIZE: u32 = 36;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = SIZE as f32 / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = (x as f32 + 0.5 - center) / center;
            let dy = (y as f32 + 0.5 - center) / center;
            // The almond is the overlap of two discs offset vertically.
            let lid = |offset: f32| (dx * dx + (dy - offset).powi(2)).sqrt() - 1.25;
            let almond = lid(0.75).max(lid(-0.75));
            let outline = almond.abs() < 0.09 && dx.abs() < 0.95;
            let pupil = (dx * dx + dy * dy).sqrt() < 0.3;
            if outline || pupil {
                let ix = ((y * SIZE + x) * 4) as usize;
                rgba[ix + 3] = 0xff;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer matches its size")
}
