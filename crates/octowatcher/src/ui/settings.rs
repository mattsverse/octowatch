//! The Settings tab: setup and health first, then accounts, general preferences and alerts.

use octowatcher_core::{
    appearance::Appearance, engine::Command, github::Readiness, health::ReviewsStatus, login,
    notifications::Permission,
};
use rune::{El, Tx, col, row, text};
use rune_kit::{ButtonVariant, Select, SelectAppearance};

use super::icons;
use super::widgets::{META, Row, glyph_tile, section};
use crate::app::{Octowatcher, POLL_CHOICES, Popup, SNOOZE_CHOICES, Tab, minutes_label, open_url};
use crate::theme::Theme;

/// Pop-ups and segmented controls in the preference cards share one width.
const TRAILING: f32 = 186.0;

pub fn view(s: &Octowatcher, t: &Theme, layer: &mut Vec<El>) -> El {
    col()
        .key("settings")
        .w_full()
        .gap(18.0)
        .child(health(s, t))
        .child(accounts(s, t))
        .child(general(s, t, layer))
        .child(notifications(s, t))
}

fn tone(t: &Theme, ok: Option<bool>) -> rune::Color {
    match ok {
        Some(true) => t.success,
        Some(false) => t.danger,
        None => t.warning,
    }
}

/// A health check's tile: its topic glyph while it passes, an alert or a cross when it
/// doesn't, so status never rests on colour alone.
fn status_tile(t: &Theme, topic: &'static rune::Icon, ok: Option<bool>) -> El {
    let glyph = match ok {
        Some(true) => topic,
        Some(false) => &icons::CROSS_CIRCLE,
        None => &icons::ALERT,
    };
    glyph_tile(t, glyph, tone(t, ok))
}

/// A health check's action: quiet while the check passes, a real button when it needs you.
/// A health check's action: an accent link on the trailing edge while the check passes, a
/// real button when it needs you.
fn health_action(
    t: &Theme,
    key: &str,
    label: &str,
    ok: Option<bool>,
    press: impl Fn(&mut Octowatcher, &mut Tx) + 'static,
) -> El {
    if ok == Some(true) {
        super::widgets::link(t, key, label, press)
    } else {
        super::widgets::button(label)
            .on_press_tx(press)
            .build(&t.k)
            .key(key)
    }
}

fn readiness_text(s: &Octowatcher, host: &str, readiness: &Readiness) -> String {
    let store = s.engine.store();
    let cached = store.sync_accounts.get(host).or_else(|| {
        (host == "github.com")
            .then_some(store.sync_account.as_ref())
            .flatten()
    });
    let previous = cached
        .map(|login| format!(" Last verified as @{login}."))
        .unwrap_or_default();
    match readiness {
        Readiness::Checking => format!("{host}: checking the GitHub CLI and active account…"),
        Readiness::Missing => {
            format!("{host}: GitHub CLI not found. Install gh 2.81 or newer, sign in, then Rescan.")
        }
        Readiness::NotRunnable => format!(
            "{host}: the GitHub CLI could not start. Check its installation and permissions, then Rescan."
        ),
        Readiness::SignedOut => {
            format!("You’re signed out of {host}. Run this in Terminal, then refresh:")
        }
        Readiness::Offline => {
            format!("{host}: unreachable. Check your connection or proxy, then Refresh.{previous}")
        }
        Readiness::Unavailable => format!(
            "{host}: readiness couldn’t be verified. Check the connection, CLI version and server, then Refresh or Rescan.{previous}"
        ),
        Readiness::Ready(login) => format!(
            "{host}: ready as @{login}.{}",
            if s.engine.health().error.is_some() {
                " Some review checks failed; cached results may be stale."
            } else {
                ""
            }
        ),
    }
}

fn readiness_ok(readiness: &Readiness) -> Option<bool> {
    match readiness {
        Readiness::Ready(_) => Some(true),
        Readiness::Checking | Readiness::Offline | Readiness::Unavailable => None,
        Readiness::Missing | Readiness::NotRunnable | Readiness::SignedOut => Some(false),
    }
}

fn health(s: &Octowatcher, t: &Theme) -> El {
    let health = s.engine.health();
    let (github, github_ok) = if health.hosts.is_empty() {
        (
            readiness_text(s, "github.com", &health.readiness),
            readiness_ok(&health.readiness),
        )
    } else {
        let text = health
            .hosts
            .iter()
            .map(|(host, readiness)| readiness_text(s, host, readiness))
            .collect::<Vec<_>>()
            .join("\n");
        // The worst host decides: any failure, then any unknown, else ready.
        let checks: Vec<_> = health.hosts.values().map(readiness_ok).collect();
        let ok = if checks.contains(&Some(false)) {
            Some(false)
        } else if checks.contains(&None) {
            None
        } else {
            Some(true)
        };
        (text, ok)
    };
    // The CLI's active account only means something while it can sign in.
    // The CLI's active account matters only while it can sign in, and when it isn't the
    // account already named as ready.
    let github = match (&health.active_public, &health.readiness) {
        (Some(active), Readiness::Ready(login))
            if !health.accounts.is_empty() && active != login =>
        {
            format!("{github}\nActive gh account: @{active}")
        }
        _ => github,
    };
    let mut github_row = Row::new("health-github", t, "GitHub CLI")
        .leading(status_tile(t, &icons::TERMINAL, github_ok))
        .detail(t, github);
    if s.needs_login() && !matches!(health.readiness, Readiness::Missing) {
        let command = format!("gh auth login --hostname {}", s.login_host());
        let copy = command.clone();
        github_row = github_row.below(
            row()
                .key("login-command")
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    row()
                        .key("command-well")
                        .grow(1.0)
                        .min_w(0.0)
                        .h(super::widgets::CONTROL)
                        .px(8.0)
                        .center_y()
                        .radius(6.0)
                        .fill(crate::theme::blend(t.card, t.text, 0.06))
                        .child(
                            text(command)
                                .key("command")
                                // Tracking keeps the two hyphens of `--hostname` apart.
                                .font(rune::TextStyle::new(11.5, 450.0).mono().tracking(0.4))
                                .color(t.text)
                                .selectable(true)
                                .truncate(),
                        ),
                )
                .child(
                    super::widgets::button(if s.copied_login { "Copied" } else { "Copy" })
                        .icon(&icons::COPY)
                        .variant(ButtonVariant::Contrast)
                        .on_press_tx(move |s: &mut Octowatcher, tx: &mut Tx| {
                            tx.copy(copy.clone());
                            s.copied_login = true;
                        })
                        .build(&t.k)
                        .key("copy-login"),
                )
                .child(
                    super::widgets::button("Refresh")
                        .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| {
                            s.command(Command::Refresh, tx)
                        })
                        .build(&t.k)
                        .key("login-refresh"),
                ),
        );
    }
    if matches!(health.readiness, Readiness::Missing) {
        github_row = github_row.trailing(
            super::widgets::button("Install…")
                .on_press_tx(|_: &mut Octowatcher, tx: &mut Tx| {
                    open_url("https://cli.github.com/", tx)
                })
                .build(&t.k)
                .key("install-gh"),
        );
    }

    let (folders, folders_ok) = if s.engine.scan_error().is_some() {
        (
            "Host discovery failed. Fix the GitHub CLI setup, then Rescan.".to_string(),
            Some(false),
        )
    } else if s.engine.repos().is_none() || s.engine.scanning() {
        ("Scanning watched folders…".into(), None)
    } else if s.engine.store().roots.is_empty() {
        (
            "No watched folders. Add a folder containing GitHub clones.".into(),
            Some(false),
        )
    } else if !s.engine.scan_issues().is_empty() {
        (
            format!(
                "Folder scan incomplete: {} issue(s). Fix the paths or permissions, then Rescan.",
                s.engine.scan_issues().len()
            ),
            None,
        )
    } else if s.engine.repos().is_some_and(<[_]>::is_empty) {
        ("No GitHub clones found. Add a project folder or clone a repository there, then Rescan.".into(), Some(false))
    } else if s.engine.watched_repositories().is_empty() {
        (
            "Every discovered repository is off. Turn one on in Repositories.".into(),
            None,
        )
    } else {
        let repos = s.engine.watched_repositories().len();
        let roots = s.engine.store().roots.len();
        (
            format!(
                "{repos} {} watched in {roots} {}.",
                if repos == 1 {
                    "repository"
                } else {
                    "repositories"
                },
                if roots == 1 { "folder" } else { "folders" }
            ),
            Some(true),
        )
    };
    let folders_row = Row::new("health-folders", t, "Watched folders")
        .leading(status_tile(t, &icons::FOLDER, folders_ok))
        .detail(t, folders)
        .trailing(health_action(
            t,
            "manage-folders",
            "Manage",
            folders_ok,
            |s: &mut Octowatcher, _: &mut Tx| s.select_tab(Tab::Repositories),
        ));

    let (permission, permission_ok) = match s.engine.permission() {
        Permission::Checking => ("Checking notification permission…".to_string(), None),
        Permission::Allowed(detail) => (detail.clone(), Some(true)),
        Permission::Denied => (
            "Off. Turn on Allow Notifications for Octowatcher in System Settings → Notifications, then Refresh.".into(),
            Some(false),
        ),
        Permission::Unknown(detail) => (format!("Unknown · {detail}"), None),
    };
    let mut permission_row = Row::new("health-notifications", t, "Notification permission")
        .leading(status_tile(t, &icons::BELL, permission_ok))
        .detail(t, permission);
    if cfg!(target_os = "macos") {
        permission_row = permission_row.trailing(health_action(
            t,
            "notification-settings",
            "System Settings…",
            permission_ok,
            |_: &mut Octowatcher, tx: &mut Tx| {
                open_url(
                    "x-apple.systempreferences:com.apple.Notifications-Settings.extension",
                    tx,
                )
            },
        ));
    }

    let status = s.review_status();
    let signed_out = matches!(health.readiness, Readiness::SignedOut)
        || health
            .hosts
            .values()
            .any(|r| matches!(r, Readiness::SignedOut));
    let sync = match status {
        ReviewsStatus::Unavailable if signed_out => "Waiting for sign-in",
        ReviewsStatus::Loading => {
            "Checking setup and reviews. Saved results are unverified until a sync succeeds."
        }
        ReviewsStatus::Setup => "Folder setup needs attention. Review coverage may be incomplete.",
        ReviewsStatus::Unavailable => "Sync failed; cached reviews may be stale.",
        ReviewsStatus::Offline => {
            "GitHub can’t be reached. Cached reviews may be stale; check your connection or proxy."
        }
        ReviewsStatus::Stale => "Reviews may be stale. Refresh to verify the current list.",
        ReviewsStatus::Ready => "Reviews are up to date for your watched repositories.",
    };
    let sync_ok = match status {
        ReviewsStatus::Ready => Some(true),
        // The CLI row carries the error; the sync row is neutral rather than a second warning.
        ReviewsStatus::Unavailable if signed_out => None,
        ReviewsStatus::Unavailable | ReviewsStatus::Offline => Some(false),
        _ => None,
    };
    let sync_row = Row::new("health-sync", t, "Review sync")
        // Blocked on sign-in reads neutral: neither healthy green nor a second warning.
        .leading(if status == ReviewsStatus::Unavailable && signed_out {
            glyph_tile(t, &icons::REFRESH, t.muted)
        } else {
            status_tile(t, &icons::REFRESH, sync_ok)
        })
        .detail(t, {
            let at = super::last_sync(s.engine.store().last_successful_sync);
            if status == ReviewsStatus::Ready {
                format!("{at}. {sync}")
            } else {
                format!(
                    "{} · {}",
                    sync.trim_end_matches('.'),
                    super::mid_sentence(&at)
                )
            }
        })
        // While signed out, refreshing can't succeed; the CLI row offers it after sign-in.
        .trailing_if(!signed_out, || {
            health_action(
                t,
                "health-refresh",
                "Refresh",
                sync_ok,
                |s: &mut Octowatcher, tx: &mut Tx| s.command(Command::Refresh, tx),
            )
        });

    let healthy = [github_ok, folders_ok, permission_ok, sync_ok]
        .iter()
        .all(|ok| *ok == Some(true));
    let mut problems_seen = false;
    let mut rows = vec![
        github_row.build(),
        folders_row.build(),
        permission_row.build(),
        sync_row.build(),
    ];
    let problems: Vec<String> = s
        .errors()
        .into_iter()
        .map(str::to_string)
        .chain(s.engine.scan_issues().iter().map(|issue| {
            format!(
                "{}: {}",
                crate::app::display_path(&issue.path),
                issue.message
            )
        }))
        .collect();
    if !problems.is_empty() {
        problems_seen = true;
        rows.push(
            Row::new("health-errors", t, "Issues")
                .leading(glyph_tile(t, &icons::ALERT, t.danger))
                .detail_el(
                    col().key("errors").gap(4.0).children(
                        problems
                            .into_iter()
                            .map(|error| text(error).font(META).color(t.danger).max_lines(4)),
                    ),
                )
                .build(),
        );
    }
    if healthy && !problems_seen && !s.health_expanded {
        // Everything works: one line, so preferences stay in view. Details expand on request.
        let ready_as = match &health.readiness {
            Readiness::Ready(login) => format!("gh ready as @{login}"),
            _ => "gh ready".into(),
        };
        let repos = s.engine.watched_repositories().len();
        let summary = format!(
            "{ready_as} · {repos} {} watched · {}",
            if repos == 1 {
                "repository"
            } else {
                "repositories"
            },
            super::mid_sentence(&super::last_sync(s.engine.store().last_successful_sync))
        );
        rows = vec![
            Row::new("health-summary", t, "Everything is working")
                .leading(glyph_tile(t, &icons::CHECK_CIRCLE, t.success))
                .detail(t, summary)
                .trailing(super::widgets::link(
                    t,
                    "health-details",
                    "Details",
                    |s: &mut Octowatcher, _: &mut Tx| s.health_expanded = true,
                ))
                .build(),
        ];
        return section(t, "setup-health-panel", Some("Setup & health"), rows);
    }
    if healthy && !problems_seen {
        rows.push(
            row()
                .key("health-collapse")
                .w_full()
                .h(40.0)
                .px(super::widgets::INSET - 4.0)
                .center_y()
                .child(
                    super::widgets::button("Hide Details")
                        .variant(ButtonVariant::GhostMuted)
                        .on_press(|s: &mut Octowatcher| s.health_expanded = false)
                        .build(&t.k)
                        .key("health-hide"),
                ),
        );
    }
    section(t, "setup-health-panel", Some("Setup & health"), rows)
}

fn accounts(s: &Octowatcher, t: &Theme) -> El {
    let store = s.engine.store();
    let mut rows: Vec<El> = store
        .known_accounts
        .iter()
        .map(|account| {
            let enabled = store.account_enabled(account);
            let readiness = s.engine.health().accounts.get(account);
            let signed_out = matches!(readiness, Some(Readiness::SignedOut))
                || matches!(
                    s.engine.health().readiness,
                    Readiness::SignedOut | Readiness::Missing
                );
            let error = s
                .engine
                .account_errors()
                .get(account)
                .cloned()
                .or(None::<String>)
                .filter(|_| enabled);
            let status = if !enabled {
                "Not monitored".to_string()
            } else if let Some(error) = &error {
                error.clone()
            } else if signed_out {
                "Paused until you sign in".into()
            } else if matches!(readiness, Some(Readiness::Offline)) {
                "Unreachable. Cached reviews may be stale.".into()
            } else if store.available_accounts.contains(account) {
                "Monitoring github.com".into()
            } else {
                "Waiting for a successful check".into()
            };
            let login = account.clone();
            let attention = enabled && signed_out;
            Row::new(format!("account-{account}"), t, format!("@{account}"))
                .leading(super::widgets::person(t, account, !enabled || attention))
                .detail_el(
                    text(status)
                        .key("detail")
                        .font(META)
                        .color(if error.is_some() {
                            t.danger
                        } else if attention {
                            t.warning
                        } else {
                            t.muted
                        })
                        .max_lines(3),
                )
                .trailing(super::widgets::switch_paused(
                    t,
                    "monitor",
                    format!("Monitor @{account}"),
                    enabled,
                    !attention,
                    attention,
                    move |s: &mut Octowatcher, tx: &mut Tx| {
                        s.command(Command::ToggleAccount(login.clone()), tx)
                    },
                ))
                .build()
        })
        .collect();
    if rows.is_empty() {
        rows.push(
            Row::new("no-accounts", t, "No saved accounts yet")
                .leading(glyph_tile(t, &icons::USER, t.muted))
                .build(),
        );
    }
    col()
        .key("accounts-section")
        .w_full()
        .gap(7.0)
        .child(section(t, "accounts", Some("GitHub accounts"), rows))
        .child(
            text("Accounts come from the GitHub CLI; new ones are monitored automatically.")
                .key("accounts-help")
                .font(rune::TextStyle::new(11.0, 400.0))
                .color(t.muted)
                .mt(-2.0)
                .max_lines(3)
                .mx(super::widgets::INSET),
        )
}

fn general(s: &Octowatcher, t: &Theme, layer: &mut Vec<El>) -> El {
    let store = s.engine.store();
    let appearance = super::widgets::segmented(
        t,
        "appearance",
        "Appearance",
        &Appearance::CHOICES.map(Appearance::label),
        Appearance::CHOICES
            .iter()
            .position(|a| *a == store.appearance)
            .unwrap_or(0),
        TRAILING,
        |s: &mut Octowatcher, ix, tx: &mut Tx| s.set_appearance(Appearance::CHOICES[ix], tx),
    );
    let mut rows = vec![
        Row::new("appearance-row", t, "Appearance")
            .leading(glyph_tile(t, &icons::SUN, t.accent))
            .detail(t, "System follows your desktop.")
            .trailing(appearance)
            .build(),
    ];

    match s.engine.login_state() {
        login::State::Unavailable(reason) => rows.push(
            Row::new("login-row", t, "Launch at login")
                .leading(glyph_tile(t, &icons::POWER, t.muted))
                .detail(t, reason.clone())
                .trailing(super::widgets::switch(
                    t,
                    "launch-at-login",
                    "Launch at login",
                    false,
                    false,
                    |_: &mut Octowatcher, _: &mut Tx| {},
                ))
                .build(),
        ),
        state => {
            let detail = match state {
                login::State::NeedsApproval => {
                    "Allow Octowatcher in System Settings → General → Login Items.".to_string()
                }
                login::State::NeedsRepair => {
                    "The startup command changed. Turn this on to start this copy at login.".into()
                }
                _ => "Start quietly in the menu bar when you sign in.".into(),
            };
            let mut r = Row::new("login-row", t, "Launch at login")
                .leading(glyph_tile(t, &icons::POWER, t.accent))
                .detail(t, detail)
                .trailing(super::widgets::switch(
                    t,
                    "launch-at-login",
                    "Launch at login",
                    state.requested(),
                    true,
                    |s: &mut Octowatcher, tx: &mut Tx| s.command(Command::ToggleLogin, tx),
                ));
            if let Some(error) = s.engine.login_error() {
                r = r.below(
                    text(error.to_string())
                        .font(META)
                        .color(t.danger)
                        .max_lines(3),
                );
            }
            rows.push(r.build());
        }
    }

    let minutes = |choices: &[u64], current: u64| {
        let mut labels: Vec<String> = choices.iter().map(|m| minutes_label(*m)).collect();
        if !choices.contains(&current) {
            labels.push(minutes_label(current));
        }
        labels
    };
    let poll_labels = minutes(&POLL_CHOICES, store.poll_minutes);
    let poll = Select::new("poll-interval", &poll_labels)
        .label("Check GitHub every")
        .current(Some(&minutes_label(store.poll_minutes)))
        .open(s.popup == Some(Popup::PollInterval))
        .appearance(SelectAppearance::Field)
        .width(112.0)
        .on_toggle(|s: &mut Octowatcher| s.toggle_popup(Popup::PollInterval))
        .on_close(|s: &mut Octowatcher| s.popup = None)
        .on_choose(|s: &mut Octowatcher, label: &str, tx: &mut Tx| {
            s.popup = None;
            if let Some(m) = POLL_CHOICES
                .into_iter()
                .find(|m| minutes_label(*m) == label)
            {
                s.command(Command::SetPollMinutes(m), tx);
            }
        })
        .build(&t.k, layer)
        .h(super::widgets::CONTROL);
    rows.push(
        Row::new("poll-row", t, "Check GitHub every")
            .leading(glyph_tile(t, &icons::REFRESH, t.accent))
            .detail(t, "Or check now with ⌘R.")
            .trailing(poll)
            .build(),
    );

    let snooze_labels = minutes(&SNOOZE_CHOICES, store.snooze_minutes);
    let snooze = Select::new("snooze-default", &snooze_labels)
        .label("Default snooze")
        .current(Some(&minutes_label(store.snooze_minutes)))
        .open(s.popup == Some(Popup::SnoozeDefault))
        .appearance(SelectAppearance::Field)
        .width(112.0)
        .on_toggle(|s: &mut Octowatcher| s.toggle_popup(Popup::SnoozeDefault))
        .on_close(|s: &mut Octowatcher| s.popup = None)
        .on_choose(|s: &mut Octowatcher, label: &str, tx: &mut Tx| {
            s.popup = None;
            if let Some(m) = SNOOZE_CHOICES
                .into_iter()
                .find(|m| minutes_label(*m) == label)
            {
                s.command(Command::SetSnoozeMinutes(m), tx);
            }
        })
        .build(&t.k, layer)
        .h(super::widgets::CONTROL);
    rows.push(
        Row::new("snooze-row", t, "Default snooze")
            .leading(glyph_tile(t, &icons::MOON, t.accent))
            .detail(
                t,
                "Marked in the snooze menu; used by notification actions.",
            )
            .trailing(snooze)
            .build(),
    );
    section(t, "general", Some("General"), rows)
}

fn notifications(s: &Octowatcher, t: &Theme) -> El {
    let store = s.engine.store();
    let muted = store.notifications_muted;
    let rows = vec![
        Row::new("alerts-row", t, "Review alerts")
            .leading(glyph_tile(t, &icons::BELL, t.accent))
            .detail(
                t,
                if muted {
                    "Muted. The queue keeps updating; turning alerts back on sends one catch-up alert."
                } else {
                    "A notification for each new review request."
                },
            )
            .trailing(
                super::widgets::switch(t, "mute-notifications", "Review alerts", !muted, true, |s: &mut Octowatcher, tx: &mut Tx| {
                        s.command(Command::ToggleNotificationsMuted, tx)
                    }),
            )
            .build(),
        Row::new("drafts-row", t, "Draft alerts")
            .leading(glyph_tile(t, &icons::PULL_REQUEST, t.accent))
            .detail(t, if muted { "Turn on review alerts to choose." } else { "Also notify when a draft requests your review." })
            .trailing(
                super::widgets::switch(t, "notify-drafts", "Draft alerts", store.notify_drafts, !muted, |s: &mut Octowatcher, tx: &mut Tx| s.command(Command::ToggleNotifyDrafts, tx)),
            )
            .build(),
        Row::new("test-row", t, "Test notification")
            .leading(glyph_tile(t, &icons::SEND, t.accent))
            .detail(t, "Check that alerts reach you.")
            .trailing(
                super::widgets::button("Send")
                    .on_press_tx(|s: &mut Octowatcher, tx: &mut Tx| s.command(Command::SendTestNotification, tx))
                    .build(&t.k)
                    .key("test-notification"),
            )
            .build(),
    ];
    section(t, "notifications", Some("Notifications"), rows)
}
