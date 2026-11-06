//! The Reviews tab: search and filters over the queue, and a row per pull request.

use chrono::{DateTime, Local};
use octowatcher_core::{
    engine::{Command, Update},
    health::ReviewsStatus,
    repository::PUBLIC_HOST,
    review_filter::{DraftFilter, ReviewFilter, SnoozeFilter},
    store::PendingReview,
};
use rune::{El, Tx, col, row, text};
use rune_kit::{
    ButtonVariant, Menu, MenuItem, Placement, Select, SelectAppearance, State, TextInput,
};

/// Every filter control shares one width, so they line up in a column.
const FILTER_WIDTH: f32 = 372.0;

use super::icons;
use super::widgets::{self, META, TITLE, badge, banner, monogram};
use crate::app::{Octowatcher, Popup, Tab, minutes_label, open_url, snooze_duration_choices};
use crate::theme::{Theme, blend};

pub fn view(s: &Octowatcher, t: &Theme, layer: &mut Vec<El>) -> El {
    let total = s.total_reviews();
    let indices = s.visible_indices();
    let active = s.filters.active_count();
    let mut page = col().key("reviews").w_full().gap(14.0);
    // With nothing in the queue there's nothing to search.
    if total > 0 || active > 0 || s.filters_open {
        page = page.child(search_bar(s, t, active));
    }
    if s.filters_open {
        page = page.child(filter_panel(s, t, layer));
    }
    if let Some(update) = update_banner(s, t) {
        page = page.child(update);
    }
    if s.needs_health() && total > 0 {
        let message = if s.review_status() != ReviewsStatus::Ready {
            "Showing the last known reviews, which may be stale."
        } else {
            "Setup needs attention; some reviews may be missing."
        };
        page = page.child(banner(
            t,
            "review-health-warning",
            &icons::ALERT,
            t.warning,
            message,
            Some(
                super::widgets::button("View Setup")
                    .on_press(|s: &mut Octowatcher| s.select_tab(Tab::Settings))
                    .build(&t.k)
                    .key("open-settings"),
            ),
        ));
    }
    if indices.is_empty() {
        return page.child(empty(s, t, total, active));
    }
    let store = s.engine.store();
    let accounts = store.known_accounts.len();
    let mut list = col()
        .key("review-list")
        .role("list")
        .label("Reviews")
        .w_full()
        .fill(t.card)
        .radius(10.0)
        .border(t.hair(), t.card_border)
        .shadow(1.0, 2.0, t.shadow)
        .clip_rounded(10.0)
        .focus_group();
    for (position, pr) in indices
        .iter()
        .filter_map(|&ix| store.pending.get(ix))
        .enumerate()
    {
        if position > 0 {
            list = list.child(widgets::separator(t, position));
        }
        list = list.child(review_row(
            s,
            t,
            pr,
            accounts > 1,
            position,
            indices.len(),
            layer,
        ));
    }
    let caption = if active > 0 {
        format!("{} of {total} reviews", indices.len())
    } else {
        // Drafts and snoozed reviews aren't waiting on you yet; say how many of the total are.
        let drafts = s.visible_reviews().filter(|pr| pr.is_draft).count();
        let snoozed = s
            .visible_reviews()
            .filter(|pr| store.snooze_for(pr).is_some())
            .count();
        let mut caption = if total == 1 {
            "1 review".to_string()
        } else {
            format!("{total} reviews")
        };
        if drafts > 0 {
            caption.push_str(&format!(
                " · {drafts} draft{}",
                if drafts == 1 { "" } else { "s" }
            ));
        }
        if snoozed > 0 {
            caption.push_str(&format!(" · {snoozed} snoozed"));
        }
        caption
    };
    page.child(
        col()
            .key("results")
            .w_full()
            .gap(7.0)
            .child(
                row()
                    .key("count-line")
                    .w_full()
                    .center_y()
                    .justify_between()
                    .child(
                        text(caption)
                            .key("count")
                            .font(widgets::SECTION)
                            .color(t.muted)
                            .ml(widgets::INSET),
                    )
                    .children((active > 0).then(|| {
                        let searching = !s.filters.query.trim().is_empty();
                        reset_link(
                            t,
                            match (searching, active > usize::from(searching)) {
                                (true, false) => "Clear Search",
                                (false, true) => "Clear Filters",
                                _ => "Clear All",
                            },
                        )
                    })),
            )
            .child(list),
    )
}

fn search_bar(s: &Octowatcher, t: &Theme, active: usize) -> El {
    let mut search = TextInput::new(&s.search)
        .placeholder("Search reviews")
        .label("Search reviews")
        .leading_icon(t.k.glyph.search);
    if !s.search.text.is_empty() {
        search = search.clear_button();
    }
    let search = search
        .height(widgets::CONTROL)
        .on_edit(|s: &mut Octowatcher| &mut s.search)
        .on_change(|s: &mut Octowatcher, _: &mut Tx| s.search_edited())
        .build(&t.k)
        .key("review-search")
        .grow(1.0)
        .min_w(0.0);
    let filter_count = active - usize::from(!s.filters.query.trim().is_empty());
    let pill = (t.on_accent, t.fill);
    let filters = super::widgets::button("Filters")
        .icon(&icons::FILTER)
        .label_with(move |font, ink| {
            // An active filter shows as an accent count; its slot is always there, so the
            // button never changes width.
            row()
                .gap(6.0)
                .center_y()
                .child(text("Filters").font(font).color(ink))
                .children((filter_count > 0).then(|| {
                    row()
                        .key("filter-count")
                        .h(16.0)
                        .w(16.0)
                        .radius(8.0)
                        .fill(pill.1)
                        .center()
                        .a11y_hidden()
                        .child(
                            text(filter_count.min(9).to_string())
                                .font(widgets::BADGE)
                                .color(pill.0),
                        )
                }))
        })
        .variant(ButtonVariant::Neutral)
        .state(if s.filters_open {
            State::Pressed
        } else {
            State::Rest
        })
        .expanded(s.filters_open)
        .help("Filters (⌘F searches)")
        .on_press(|s: &mut Octowatcher| {
            s.filters_open = !s.filters_open;
            if s.popup == Some(Popup::Repository) {
                s.popup = None;
            }
        })
        .build(&t.k)
        .key("toggle-review-filters")
        // A steady width, so the search field doesn't jump when the count appears.
        .min_w(108.0)
        .justify_center();
    // Open, the button takes the segmented track's grey, so it reads pressed in both appearances.
    let filters = if s.filters_open {
        filters.fill(if t.dark {
            blend(t.card, t.text, 0.18)
        } else {
            blend(t.bg, t.text, 0.08)
        })
    } else {
        filters
    };
    row()
        .key("search-bar")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(search)
        .child(filters)
}

fn filter_panel(s: &Octowatcher, t: &Theme, layer: &mut Vec<El>) -> El {
    let repos = s.filter_repositories();
    let mut options = vec!["All repositories".to_string()];
    options.extend(repos.iter().map(|(_, label)| label.clone()));
    let current = s
        .filters
        .repository
        .as_ref()
        .and_then(|repo| repos.iter().find(|(id, _)| id == repo))
        .map(|(_, label)| label.clone())
        .unwrap_or_else(|| options[0].clone());
    let repository = Select::new("review-repository", &options)
        .label("Repository")
        .current(Some(&current))
        .open(s.popup == Some(Popup::Repository))
        .appearance(SelectAppearance::Field)
        .width(FILTER_WIDTH)
        .on_toggle(|s: &mut Octowatcher| s.toggle_popup(Popup::Repository))
        .on_close(|s: &mut Octowatcher| {
            if s.popup == Some(Popup::Repository) {
                s.popup = None
            }
        })
        .on_choose(move |s: &mut Octowatcher, label: &str, _: &mut Tx| {
            s.filters.repository = s
                .filter_repositories()
                .into_iter()
                .find(|(_, l)| l == label)
                .map(|(id, _)| id);
            s.popup = None;
            s.filters_changed();
        })
        .build(&t.k, layer)
        .h(widgets::CONTROL);
    let segment =
        |key: &str, labels: &[&str], selected: usize, pick: fn(&mut Octowatcher, usize)| {
            widgets::segmented(
                t,
                &format!("{}-filter", key.to_lowercase()),
                key,
                labels,
                selected,
                FILTER_WIDTH,
                move |s: &mut Octowatcher, ix, _: &mut Tx| {
                    pick(s, ix);
                    s.filters_changed();
                },
            )
        };
    let draft = segment(
        "Status",
        &["All", "Ready", "Draft"],
        [DraftFilter::All, DraftFilter::Ready, DraftFilter::Draft]
            .iter()
            .position(|f| *f == s.filters.draft)
            .unwrap_or(0),
        |s, ix| s.filters.draft = [DraftFilter::All, DraftFilter::Ready, DraftFilter::Draft][ix],
    );
    let request = segment(
        "Request",
        &["All", "First review", "Re-review"],
        [
            ReviewFilter::All,
            ReviewFilter::First,
            ReviewFilter::Rereview,
        ]
        .iter()
        .position(|f| *f == s.filters.review)
        .unwrap_or(0),
        |s, ix| {
            s.filters.review = [
                ReviewFilter::All,
                ReviewFilter::First,
                ReviewFilter::Rereview,
            ][ix]
        },
    );
    let snooze = segment(
        "Snooze",
        &["All", "Awake", "Snoozed"],
        [
            SnoozeFilter::All,
            SnoozeFilter::Awake,
            SnoozeFilter::Snoozed,
        ]
        .iter()
        .position(|f| *f == s.filters.snooze)
        .unwrap_or(0),
        |s, ix| {
            s.filters.snooze = [
                SnoozeFilter::All,
                SnoozeFilter::Awake,
                SnoozeFilter::Snoozed,
            ][ix]
        },
    );
    let field = |key: &str, label: &str, control: El| {
        row()
            .key(key)
            .w_full()
            .min_h(40.0)
            .px(widgets::INSET)
            .gap(12.0)
            .center_y()
            .justify_between()
            .child(text(label).font(widgets::BODY).color(t.text).a11y_hidden())
            .child(control)
    };
    let can_reset = s.filters.active_count() > 0;
    let reset = text("Reset All")
        .key("reset-review-filters")
        .font(widgets::META_MEDIUM)
        .color(if can_reset { t.accent } else { t.faint })
        .role("button")
        .label("Reset all filters")
        .help("Reset search and filters (Esc)")
        .disabled(!can_reset)
        .focusable(can_reset)
        .cursor(if can_reset {
            rune::Cursor::Pointer
        } else {
            rune::Cursor::Default
        })
        .mr(widgets::INSET)
        .on_press(|s: &mut Octowatcher| s.reset_filters());
    let card = widgets::section_inset(
        t,
        "filter-card",
        None,
        vec![
            field("filter-repository", "Repository", repository),
            field("filter-status", "Status", draft),
            field("filter-request", "Request", request),
            field("filter-snooze", "Snooze", snooze),
        ],
        widgets::INSET,
    );
    let _ = reset;
    card.key("review-filter-controls")
}

/// Resets search and filters; the app's one quiet text action, in the accent.
/// Minutes from now until 9:00 tomorrow, local time.
fn minutes_until_tomorrow_morning() -> u64 {
    let now = Local::now();
    let tomorrow = now
        .date_naive()
        .succ_opt()
        .and_then(|d| d.and_hms_opt(9, 0, 0));
    tomorrow
        .and_then(|at| at.and_local_timezone(Local).single())
        .map(|at| (at - now).num_minutes().max(1) as u64)
        .unwrap_or(16 * 60)
}

/// "2h ago": how long a review request has been waiting, from its RFC 3339 time.
fn waiting_for(requested_at: &str) -> Option<String> {
    let at = DateTime::parse_from_rfc3339(requested_at).ok()?;
    let minutes = (Local::now().signed_duration_since(at).num_minutes()).max(0);
    Some(match minutes {
        0..=1 => "just now".into(),
        2..=59 => format!("{minutes}m ago"),
        60..=1439 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    })
}

/// A PR title with its `backtick` spans set in the monospace face, backticks removed.
fn code_spans(title: &str, t: &Theme) -> El {
    if title.matches('`').count() < 2 {
        return text(title.to_string());
    }
    let (mut plain, mut spans, mut code) = (String::new(), Vec::new(), false);
    for part in title.split('`') {
        if code && !part.is_empty() {
            // Thin spaces keep the chip's fill off the neighbouring words.
            plain.push('\u{2009}');
        }
        let start = plain.len() as u32;
        plain.push_str(part);
        if code && !part.is_empty() {
            spans.push(rune::Span {
                start,
                end: plain.len() as u32,
                mono: true,
                // The monospace face runs large; a step down matches the title's x-height.
                size: Some(11.5),
                weight: Some(600.0),
                bg: Some(crate::theme::blend(t.card, t.text, 0.07)),
                bg_pad: 2.0,
                ..Default::default()
            });
            plain.push('\u{2009}');
        }
        code = !code;
    }
    rune::rich(plain, spans)
}

fn reset_link(t: &Theme, label: &'static str) -> El {
    text(label)
        .key("reset-review-filters")
        .font(widgets::META_MEDIUM)
        .color(t.accent)
        .role("button")
        .label("Reset search and filters")
        .help("Reset search and filters (Esc)")
        .focusable(true)
        .cursor(rune::Cursor::Pointer)
        .mr(widgets::INSET)
        .on_press(|s: &mut Octowatcher| s.reset_filters())
}

fn update_banner(s: &Octowatcher, t: &Theme) -> Option<El> {
    if s.update_banner_dismissed {
        return None;
    }
    let (message, action): (String, Option<El>) = match s.engine.update()? {
        Update::Available(release) => (
            format!("Octowatcher {} is available.", release.version),
            Some(
                super::widgets::button("Update")
                    .variant(ButtonVariant::Contrast)
                    .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| {
                        s.command(Command::InstallUpdate, tx)
                    })
                    .build(&t.k)
                    .key("update"),
            ),
        ),
        Update::Installing(version) => (format!("Installing Octowatcher {version}…"), None),
        Update::Ready(version) => (
            format!("Octowatcher {version} is ready\nRestart to finish updating."),
            Some(
                row()
                    .key("update-actions")
                    .gap(8.0)
                    .center_y()
                    .child(
                        super::widgets::button("Later")
                            .on_press(|s: &mut Octowatcher| s.update_banner_dismissed = true)
                            .build(&t.k)
                            .key("update-later"),
                    )
                    .child(
                        super::widgets::button("Restart Now")
                            .variant(ButtonVariant::Contrast)
                            .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| s.restart(tx))
                            .build(&t.k)
                            .key("update"),
                    ),
            ),
        ),
        Update::Manual(release) => {
            let url = release.url.clone();
            (
                format!("Octowatcher {} is available.", release.version),
                Some(
                    super::widgets::button("Download")
                        .on_press_tx(move |_: &mut Octowatcher, tx: &mut Tx| open_url(&url, tx))
                        .build(&t.k)
                        .key("update"),
                ),
            )
        }
    };
    // An action waiting, not a success: always the accent download glyph.
    let (glyph, tint) = (&icons::DOWNLOAD, t.accent);
    // A title over a subtitle when the message has two parts.
    let message = match message.split_once('\n') {
        Some((title, detail)) => col()
            .key("message")
            .gap(1.0)
            .child(
                text(title.to_string())
                    .font(widgets::BODY_MEDIUM)
                    .color(t.text),
            )
            .child(text(detail.to_string()).font(META).color(t.muted)),
        None => text(message)
            .key("message")
            .font(widgets::BODY)
            .color(t.text)
            .max_lines(3),
    };
    Some(widgets::banner_with(
        t,
        "update-banner",
        glyph,
        tint,
        message,
        action,
    ))
}

fn empty(s: &Octowatcher, t: &Theme, total: usize, active: usize) -> El {
    if total > 0 || active > 0 {
        let searching = !s.filters.query.trim().is_empty();
        let filtering = active > usize::from(searching);
        let (body, action) = match (searching, filtering) {
            (true, false) => (
                format!(
                    "Try another word, or clear “{}” to see every review.",
                    s.filters.query.trim()
                ),
                "Clear Search",
            ),
            (false, true) => (
                "Loosen the filters to see more of your queue.".to_string(),
                "Reset Filters",
            ),
            _ => (
                "Try another search, or loosen the filters.".to_string(),
                "Clear Search and Filters",
            ),
        };
        return col()
            .key("review-no-results")
            .w_full()
            .items_center()
            .child(
                row()
                    .key("count-line")
                    .w_full()
                    .center_y()
                    .justify_between()
                    .child(
                        text(format!("0 of {total} reviews"))
                            .key("count")
                            .font(widgets::SECTION)
                            .color(t.muted)
                            .ml(widgets::INSET),
                    ),
            )
            .child(
                widgets::empty_state(
                    t,
                    "no-results",
                    &icons::SEARCH_X,
                    t.muted,
                    "No matching reviews",
                    &body,
                )
                .pt(122.0),
            )
            .child(
                super::widgets::button(action)
                    .on_press(|s: &mut Octowatcher| s.reset_filters())
                    .build(&t.k)
                    .key("no-results-reset"),
            );
    }
    let status = s.review_status();
    let (i, tint, title) = match status {
        ReviewsStatus::Ready => (&icons::CHECK_ALL, t.success, "You’re all caught up"),
        ReviewsStatus::Loading => (&icons::REFRESH, t.accent, "Checking for reviews"),
        _ => (&icons::ALERT, t.warning, "Reviews unavailable"),
    };
    let mut state = col()
        .key("review-no-results")
        .w_full()
        .items_center()
        // The search bar is hidden here; keep the block where the no-results one sits.
        .pt(widgets::CONTROL + 14.0)
        .child(widgets::empty_state(
            t,
            "empty",
            i,
            tint,
            title,
            status.empty_message(),
        ));
    if status != ReviewsStatus::Ready && status != ReviewsStatus::Loading {
        state = state.child(
            super::widgets::button("Open Settings")
                .on_press(|s: &mut Octowatcher| s.select_tab(Tab::Settings))
                .build(&t.k)
                .key("open-settings"),
        );
    }
    state
}

fn review_row(
    s: &Octowatcher,
    t: &Theme,
    pr: &PendingReview,
    show_account: bool,
    position: usize,
    count: usize,
    layer: &mut Vec<El>,
) -> El {
    let store = s.engine.store();
    let key = pr.key();
    // "10:59" today, "tomorrow 9:00", or a date further out.
    let snoozed_until = store.snooze_for(pr).map(|snooze| {
        DateTime::from_timestamp(snooze.until, 0)
            .map(|at| {
                let at = at.with_timezone(&Local);
                let today = Local::now().date_naive();
                match (at.date_naive() - today).num_days() {
                    0 => at.format("%H:%M").to_string(),
                    1 => at.format("tomorrow %H:%M").to_string(),
                    _ => at.format("%b %-d, %H:%M").to_string(),
                }
            })
            .unwrap_or_default()
    });
    let snoozed = snoozed_until.is_some();
    let owner = pr
        .repo
        .split_once('/')
        .map_or(pr.repo.as_str(), |(owner, _)| owner);
    let repo_label = if pr.host == PUBLIC_HOST {
        pr.repo.clone()
    } else {
        format!("{}/{}", pr.host, pr.repo)
    };

    let mut badges = row().key("badges").gap(5.0).center_y();
    if let Some(until) = &snoozed_until {
        badges = badges.child(widgets::badge_icon(
            "snoozed",
            &icons::MOON,
            format!("Until {until}"),
            t.muted,
            blend(t.card, t.text, 0.08),
        ));
    }
    if pr.rereview {
        badges = badges.child(badge("rereview", "Re-review", t.warning, t.warning_bg));
    }
    if pr.is_draft {
        badges = badges.child(badge(
            "draft",
            "Draft",
            blend(t.text, t.card, 0.25),
            blend(t.card, t.text, 0.08),
        ));
    }

    let title = row()
        .key("title-row")
        .w_full()
        // A fixed line box: badges and code chips centre in it instead of growing the line.
        .h(18.0)
        .gap(8.0)
        .center_y()
        .child(
            code_spans(&pr.title, t)
                .key("title")
                .font(TITLE)
                // Snoozed titles dim, but stay a step above the line under them.
                // A snoozed row recedes as a whole: title secondary, meta tertiary.
                .color(if snoozed { t.muted } else { t.text })
                .truncate()
                .shrink()
                .min_w(0.0),
        )
        .child(badges);
    // One line, one separator: repo and number, author, how long it has waited, any snooze,
    // and the receiving account last, so it's the part that gives way when space runs out.
    let mut parts = vec![format!("{repo_label} #{}", pr.number), pr.author.clone()];
    if let Some(age) = pr.requested_at.as_deref().and_then(waiting_for) {
        parts.push(age);
    }
    if show_account && !pr.account.is_empty() {
        parts.push(format!("for @{}", pr.account));
    }
    let meta = text(parts.join(" · "))
        .key("meta")
        .font(META)
        .color(if snoozed {
            blend(t.muted, t.card, 0.3)
        } else {
            t.muted
        })
        .truncate()
        .w_full();

    let action = if snoozed {
        let key = key.clone();
        // Same slot, size and weight as the moons; the tooltip names it.
        widgets::ring_button(
            t,
            "snooze",
            &icons::ALARM,
            "Wake now",
            false,
            move |s: &mut Octowatcher, tx: &mut Tx| s.command(Command::Unsnooze(key.clone()), tx),
        )
    } else {
        let open = s.popup == Some(Popup::Snooze(key.clone()));
        let default = store.snooze_minutes;
        let toggle = key.clone();
        // A ghost button with a hairline ring: clearly a control, without a column of chrome.
        // A hairline-ringed glyph: clearly a control, without a column of heavy chrome. It
        // brightens with its row's hover and stays pressed while its menu is open.
        let trigger = widgets::ring_button(
            t,
            "snooze",
            &icons::MOON,
            "Snooze…",
            open,
            move |s: &mut Octowatcher, _: &mut Tx| s.toggle_popup(Popup::Snooze(toggle.clone())),
        );
        let mut menu = Menu::new(format!("snooze-menu-{position}"))
            .trigger(trigger)
            .open(open)
            .placement(Placement::BottomEnd)
            // `standard()` resets the width, so it comes first.
            .standard()
            .min_width(172.0)
            .gutter(2.0)
            .group_label("Snooze for")
            .on_close(|s: &mut Octowatcher| {
                if matches!(s.popup, Some(Popup::Snooze(_))) {
                    s.popup = None;
                }
            });
        for minutes in snooze_duration_choices(default) {
            let key = key.clone();
            let mut item = MenuItem::new(minutes_label(minutes)).on_select_tx(
                move |s: &mut Octowatcher, tx: &mut Tx| s.snooze_for(key.clone(), minutes, tx),
            );
            if minutes == default {
                item = item.shortcut("Default");
            }
            menu = menu.item(item);
        }
        let key_tomorrow = key.clone();
        menu = menu
            .separator()
            .item(MenuItem::new("Tomorrow at 9:00").on_select_tx(
                move |s: &mut Octowatcher, tx: &mut Tx| {
                    s.snooze_for(key_tomorrow.clone(), minutes_until_tomorrow_morning(), tx)
                },
            ));
        menu.build(&t.k, layer)
    };

    let url = pr.url.clone();
    let label = format!(
        "{}, {repo_label} #{} by {}{}{}{}",
        pr.title,
        pr.number,
        pr.author,
        if pr.rereview { ", re-review" } else { "" },
        if pr.is_draft { ", draft" } else { "" },
        snoozed_until
            .as_ref()
            .map(|at| format!(", snoozed until {at}"))
            .unwrap_or_default(),
    );
    row()
        .key(format!(
            "review:{}#{}:{}",
            pr.repository(),
            pr.number,
            pr.account_id
        ))
        .w_full()
        .min_h(54.0)
        .px(widgets::INSET)
        .py(8.0)
        .gap(widgets::TILE_GAP)
        .center_y()
        .hover(t.hover)
        .press(t.pressed)
        .hover_group()
        .role("link")
        .label(label)
        .position_in_set(position + 1, count)
        .focusable(true)
        .focus_ring(2.0, -2.0, t.k.c.border_focus)
        .cursor(rune::Cursor::Pointer)
        .on_press_tx(move |_: &mut Octowatcher, tx: &mut Tx| open_url(&url, tx))
        .child(monogram(t, owner, s.owner_slot(owner), snoozed).help(owner.to_string()))
        .child(
            col()
                .key("text")
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(title)
                .child(meta)
                .a11y_hidden(),
        )
        .child(action)
}
