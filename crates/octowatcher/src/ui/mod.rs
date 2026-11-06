//! The window: a toolbar with the three tabs, the tab's content, and a status bar.

pub mod icons;
mod repositories;
mod reviews;
mod settings;
pub mod widgets;

use chrono::{DateTime, Local};
use octowatcher_core::{engine::Prompt, health::ReviewsStatus, updater};
use rune::{Cx, El, Key, KeyEvent, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{ButtonVariant, IconButtonVariant, Spinner};

use crate::app::{Octowatcher, Tab};
use crate::theme::{Theme, blend, is_dark, theme};
use widgets::{BADGE, BODY_MEDIUM, META, META_MEDIUM};

/// Height of the toolbar; on macOS the traffic lights sit centred in it.
pub const TOOLBAR: f32 = 52.0;
/// Where the traffic lights' close button sits on macOS, in points from the window's top-left.
pub const TRAFFIC_LIGHTS: (f32, f32) = (18.0, 19.0);

pub fn view(s: &Octowatcher, cx: &mut Cx<Octowatcher>) -> El {
    let dark = is_dark(s.engine.store().appearance, cx.system_dark());
    let t = theme(dark, cx.env().scale);
    let mut layer = Vec::new();
    let content = match s.tab {
        Tab::Reviews => reviews::view(s, &t, &mut layer),
        Tab::Repositories => repositories::view(s, &t, &mut layer),
        Tab::Settings => settings::view(s, &t, &mut layer),
    };
    prompt(s, &t, &mut layer);
    let page = col()
        .key("page")
        .w_full()
        .px(20.0)
        .pt(16.0)
        .pb(24.0)
        .gap(18.0)
        .child(content);
    let body = col()
        .key("window")
        .role("window")
        .w_full()
        .h_full()
        .fill(t.bg)
        .child(toolbar(s, &t, cx.titlebar_inset()))
        .child(rule(&t, "toolbar-rule"))
        .child(
            t.k.scroll_bar(
                col()
                    .key(format!("scroll-{:?}", s.tab).to_lowercase())
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .scroll(ScrollSpec {
                        stick_bottom: false,
                    })
                    .scroll_edge_fade(4.0)
                    .child(page),
            )
            .grow(1.0)
            .min_h(0.0),
        )
        .child(rule(&t, "status-rule"))
        .child(status_bar(s, &t));
    rune_kit::root(body, layer)
}

fn rule(t: &Theme, key: &str) -> El {
    el().key(key)
        .w_full()
        .h(t.hair())
        .fill(t.separator)
        .a11y_hidden()
}

fn toolbar(s: &Octowatcher, t: &Theme, titlebar_inset: f32) -> El {
    let fetching = s.engine.fetching() || s.engine.scanning();
    let tabs = row()
        .key("tabs")
        .role("tablist")
        .label("Sections")
        .gap(2.0)
        .focus_group()
        .children(Tab::ALL.into_iter().map(|tab| tab_button(s, t, tab)));
    let refresh = if fetching {
        el().key("refresh")
            .size(28.0, 28.0)
            .center()
            .label("Checking GitHub")
            .child(Spinner::new().build(&t.k))
    } else {
        widgets::icon_button(&icons::REFRESH)
            .variant(IconButtonVariant::Ghost)
            .label("Check GitHub now")
            .help("Check GitHub now (⌘R)")
            .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| {
                s.command(octowatcher_core::engine::Command::ForceRefresh, tx)
            })
            .build(&t.k)
            .key("refresh")
    };
    row()
        .key("toolbar")
        .w_full()
        .h(TOOLBAR)
        .pl(16.0 + titlebar_inset)
        .pr(14.0)
        .gap(12.0)
        .center_y()
        .fill(t.bg)
        .child(tabs)
        // The empty stretch of the toolbar moves the window; the tabs stay pressable.
        .child(el().key("toolbar-gap").grow(1.0).h_full().window_drag())
        .child(refresh)
}

fn tab_button(s: &Octowatcher, t: &Theme, tab: Tab) -> El {
    let active = s.tab == tab;
    let (label, count) = match tab {
        Tab::Reviews => ("Reviews", Some(s.total_reviews())),
        // The number watched, matching "5 of 6 watched" and the health summary.
        Tab::Repositories => (
            "Repositories",
            s.engine
                .repos()
                .map(|_| s.engine.watched_repositories().len()),
        ),
        Tab::Settings => ("Settings", None),
    };
    let fg = if active { t.text } else { t.muted };
    let name = match count {
        Some(n) => format!("{label}, {n}"),
        None => label.to_string(),
    };
    row()
        .key(format!("tab-{}", label.to_lowercase()))
        .h(26.0)
        .px(10.0)
        .gap(6.0)
        .radius(7.0)
        .center()
        .fill(if active { t.card } else { t.bg })
        .border(t.hair(), if active { t.card_border } else { t.bg })
        .shadow(
            if active { 1.0 } else { 0.0 },
            if active { 2.0 } else { 0.0 },
            if active {
                t.shadow
            } else {
                t.shadow.alpha(0.0)
            },
        )
        .hover(if active { t.card } else { t.hover })
        .press(t.pressed)
        .role("tab")
        .label(name)
        .checked(active)
        .focusable(true)
        .focus_selected(active)
        .focus_ring(2.0, 1.0, t.k.c.border_focus)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut Octowatcher| s.select_tab(tab))
        .help(match tab {
            Tab::Reviews
                if !matches!(
                    s.review_status(),
                    ReviewsStatus::Ready | ReviewsStatus::Loading
                ) =>
            {
                "Reviews (last known; may be stale)"
            }
            Tab::Reviews => "Reviews (⌘1)",
            Tab::Repositories => "Repositories (⌘2)",
            Tab::Settings => "Settings (⌘,)",
        })
        // One weight for every tab, so selecting one never nudges its neighbours.
        .child(text(label).font(BODY_MEDIUM).color(fg).a11y_hidden())
        // An empty queue shows a quiet check in the count's place, so the tab keeps its width.
        .children(
            (tab == Tab::Reviews && count == Some(0) && s.review_status() == ReviewsStatus::Ready)
                .then(|| {
                    row()
                        .key("count")
                        .h(17.0)
                        .w(17.0)
                        .radius(8.5)
                        .fill(blend(t.bg, t.text, 0.08))
                        .center()
                        .a11y_hidden()
                        .child(icon(t.k.glyph.check, 11.0).color(t.muted))
                }),
        )
        .children(count.filter(|n| *n > 0).map(|n| {
            // Waiting reviews stay accented on every tab: that's when they should call you back.
            let stale = s.review_status() != ReviewsStatus::Ready
                && s.review_status() != ReviewsStatus::Loading;
            let (fg, bg) = if tab == Tab::Reviews && n > 0 && stale {
                // Cached, unverified reviews: still a count, but not a confident one.
                // A bright amber with a dark numeral reads as "attention" in both appearances.
                (
                    rune::color::hex("#2b1c00").unwrap(),
                    rune::color::hex(if t.dark { "#f2b35b" } else { "#f0a93b" }).unwrap(),
                )
            } else if tab == Tab::Reviews && n > 0 {
                (t.on_accent, t.fill)
            } else {
                (t.muted, blend(t.bg, t.text, 0.08))
            };
            row()
                .key("count")
                .h(17.0)
                .min_w(17.0)
                .px(5.0)
                .radius(8.5)
                .fill(bg)
                .center()
                .a11y_hidden()
                .child(text(n.to_string()).font(BADGE).color(fg))
        }))
}

fn status_bar(s: &Octowatcher, t: &Theme) -> El {
    let status = s.review_status();
    let (color, label) = if s.engine.scanning() {
        (t.accent, "Scanning folders…".to_string())
    } else if s.engine.fetching() {
        (t.accent, "Checking GitHub…".to_string())
    } else {
        let color = match status {
            ReviewsStatus::Ready => t.success,
            ReviewsStatus::Loading => t.accent,
            ReviewsStatus::Stale | ReviewsStatus::Setup => t.warning,
            ReviewsStatus::Unavailable if s.needs_login() => t.warning,
            ReviewsStatus::Offline | ReviewsStatus::Unavailable => t.danger,
        };
        let label = match status {
            ReviewsStatus::Loading => "Checking setup…",
            ReviewsStatus::Setup => "Setup needs attention",
            ReviewsStatus::Unavailable if s.needs_login() => "Sign-in required",
            ReviewsStatus::Unavailable => "Sync failed",
            ReviewsStatus::Offline => "GitHub unreachable",
            ReviewsStatus::Stale => "Reviews may be stale",
            ReviewsStatus::Ready => "Reviews up to date",
        };
        (color, label.to_string())
    };
    let synced = match (status, s.engine.store().last_successful_sync) {
        (_, None) => "never synced".to_string(),
        (_, at) => mid_sentence(&last_sync(at)),
    };
    let issues = s.errors().len();
    row()
        .key("status-bar")
        .w_full()
        .h(30.0)
        // Dot and gap sit in the margin; the text starts on the content column.
        .pl(20.0)
        .pr(18.0)
        .gap(7.0)
        .center_y()
        .fill(t.bg)
        .role("status")
        .child(widgets::dot(color))
        .child(
            rune::rich(
                format!("{label} · {synced}"),
                // Routine success reads quietly; anything needing attention is emphasised.
                if s.review_status() == ReviewsStatus::Ready
                    && !s.engine.fetching()
                    && !s.engine.scanning()
                {
                    vec![rune::Span {
                        start: 0,
                        end: label.len() as u32,
                        color: Some(t.muted),
                        ..Default::default()
                    }]
                } else {
                    vec![rune::Span {
                        start: 0,
                        end: label.len() as u32,
                        color: Some(t.text),
                        weight: Some(560.0),
                        ..Default::default()
                    }]
                },
            )
            .key("status")
            .font(META)
            .color(t.muted)
            .truncate()
            .shrink()
            .min_w(0.0),
        )
        .child(el().key("status-gap").grow(1.0).h(1.0))
        .children((issues > 0).then(|| {
            row()
                .key("issues")
                .gap(5.0)
                .center_y()
                .role("link")
                .label(format!("{issues} issue(s) need attention, open Settings"))
                .focusable(true)
                .cursor(rune::Cursor::Pointer)
                .on_press(|s: &mut Octowatcher| s.select_tab(Tab::Settings))
                .child(icon(&icons::ALERT, 13.0).color(t.danger).a11y_hidden())
                .child(
                    text(if issues == 1 {
                        "1 issue".to_string()
                    } else {
                        format!("{issues} issues")
                    })
                    .font(META_MEDIUM)
                    .color(t.danger)
                    .a11y_hidden(),
                )
        }))
        .child(match s.engine.update() {
            // A quiet reminder, only where the Reviews banner isn't showing it.
            Some(octowatcher_core::engine::Update::Ready(version))
                if s.update_banner_dismissed || s.tab != Tab::Reviews =>
            {
                row()
                    .key("restart-to-update")
                    .role("button")
                    .label(format!("Restart to update to {version}"))
                    .focusable(true)
                    .cursor(rune::Cursor::Pointer)
                    .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| s.restart(tx))
                    .child(
                        text(format!("Restart to update to {version}"))
                            .font(META)
                            .color(t.accent)
                            .a11y_hidden(),
                    )
            }
            _ => text(format!("v{}", updater::current_version()))
                .key("version")
                .font(META)
                .color(t.faint),
        })
}

/// "Last synced Oct 8, 08:27" as it reads after a "·": first letter lowered, month kept.
pub fn mid_sentence(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_lowercase().chain(chars).collect())
        .unwrap_or_default()
}

pub fn last_sync(at: Option<i64>) -> String {
    match at.and_then(|at| DateTime::from_timestamp(at, 0)) {
        Some(at) => {
            let at = at.with_timezone(&Local);
            if at.date_naive() == Local::now().date_naive() {
                format!("Last synced {}", at.format("%H:%M"))
            } else {
                format!("Last synced {}", at.format("%b %-d, %H:%M"))
            }
        }
        None => "Never synced".into(),
    }
}

/// The dialog the engine asked for (update checks and restarts), over the window.
fn prompt(s: &Octowatcher, t: &Theme, layer: &mut Vec<El>) {
    let Some(prompt) = &s.prompt else { return };
    let current = updater::current_version();
    let (title, detail, accept, cancel) = match prompt {
        Prompt::UpToDate => (
            "Octowatcher is up to date".to_string(),
            format!("Version {current} is the latest release."),
            "OK",
            None,
        ),
        Prompt::CheckFailed(error) => (
            "Could not check for updates".into(),
            error.clone(),
            "OK",
            None,
        ),
        Prompt::Available(release) => (
            format!("Octowatcher {} is available", release.version),
            format!("You have {current}."),
            "Update",
            Some("Later"),
        ),
        Prompt::Download(release) => (
            format!("Octowatcher {} is available", release.version),
            "Download it from the release page.".into(),
            "Download",
            Some("Later"),
        ),
        Prompt::Restart(version) => (
            format!("Octowatcher {version} is ready"),
            "Restart to finish updating.".into(),
            "Restart Now",
            Some("Later"),
        ),
        Prompt::InstallFailed { error, .. } => (
            "Could not install the update".into(),
            error.clone(),
            "Download",
            Some("Later"),
        ),
    };
    let buttons = row()
        .key("prompt-buttons")
        .w_full()
        .gap(8.0)
        .justify_end()
        .mt(18.0)
        .children(cancel.map(|label| {
            widgets::button(label)
                .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| s.answer_prompt(false, tx))
                .build(&t.k)
                .key("prompt-cancel")
                .min_w(76.0)
                .justify_center()
        }))
        .child(
            widgets::button(accept)
                .variant(ButtonVariant::Contrast)
                .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| s.answer_prompt(true, tx))
                .build(&t.k)
                .key("prompt-accept")
                .min_w(76.0)
                .justify_center()
                .default_button(),
        );
    let glyph = match prompt {
        Prompt::CheckFailed(_) | Prompt::InstallFailed { .. } => {
            widgets::glyph_tile(t, &icons::ALERT, t.warning)
        }
        Prompt::UpToDate => widgets::glyph_tile(t, &icons::CHECK_CIRCLE, t.success),
        // Update alerts lead with the app's own icon, as a Mac alert does.
        Prompt::Restart(_) | Prompt::Available(_) | Prompt::Download(_) => el()
            .key("app-icon")
            .size(40.0, 40.0)
            .pixels(&s.app_icon)
            .a11y_hidden(),
    };
    let panel = col()
        .key("prompt-dialog")
        .role("dialog")
        .label(title.clone())
        .description(detail.clone())
        // Sized to its two lines and buttons, so no dead band runs beside the message.
        .w(320.0)
        .padding(20.0)
        // Square where it meets the toolbar, rounded below: it hangs from the window's edge.
        .corners([0.0, 0.0, 12.0, 12.0])
        // In dark the panel sits a step below its buttons, so "Later" reads raised.
        .fill(if t.dark { t.card } else { t.raised })
        .border(
            t.hair(),
            if t.dark {
                t.text.alpha(0.12)
            } else {
                t.text.alpha(0.08)
            },
        )
        .shadow(
            18.0,
            48.0,
            rune::Color::BLACK.alpha(if t.dark { 0.6 } else { 0.18 }),
        )
        .shadow(
            1.0,
            3.0,
            rune::Color::BLACK.alpha(if t.dark { 0.4 } else { 0.08 }),
        )
        .modal(true, |s: &mut Octowatcher| s.prompt = None)
        .child(
            row()
                .key("prompt-body")
                .w_full()
                .gap(14.0)
                .items_start()
                .child(glyph)
                .child(
                    col()
                        .key("prompt-text")
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(4.0)
                        .child(
                            text(title)
                                .font(widgets::HEADING)
                                .color(t.text)
                                .max_lines(2),
                        )
                        .child(text(detail).font(widgets::BODY).color(t.muted).max_lines(6)),
                ),
        )
        .child(buttons);
    // A sheet: attached under the toolbar (which stays sharp), over a light dim of the content.
    let wash = rune::Color::BLACK.alpha(if t.dark { 0.32 } else { 0.10 });
    layer.push(
        el().key("prompt-scrim")
            .absolute(0.0, TOOLBAR)
            .w_full()
            .h_full()
            .fill(wash)
            .a11y_hidden(),
    );
    layer.push(
        col()
            .key("prompt-layer")
            .absolute(0.0, TOOLBAR)
            .w_full()
            .px(24.0)
            .items_center()
            .child(panel),
    );
}

/// Keys no control took: shortcuts and Escape.
pub fn on_key(s: &mut Octowatcher, e: &KeyEvent, tx: &mut Tx) -> bool {
    if e.is_command('f') {
        s.focus_search();
        return true;
    }
    if e.is_command('r') {
        s.command(octowatcher_core::engine::Command::ForceRefresh, tx);
        return true;
    }
    if e.is_command('q') {
        s.quit(tx);
        return true;
    }
    if e.is_command(',') {
        s.select_tab(Tab::Settings);
        return true;
    }
    for (ix, tab) in Tab::ALL.into_iter().enumerate() {
        if e.is_command(char::from(b'1' + ix as u8)) {
            s.select_tab(tab);
            return true;
        }
    }
    if matches!(e.key, Key::Escape) {
        if s.popup.take().is_some() {
            return true;
        }
        if s.tab == Tab::Reviews && (s.filters.active_count() > 0 || s.filters_open) {
            if s.filters.active_count() > 0 {
                s.reset_filters();
            } else {
                s.filters_open = false;
            }
            return true;
        }
    }
    false
}
