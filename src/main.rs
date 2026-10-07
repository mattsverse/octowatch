mod discovery;
mod github;
mod health;
mod notifications;
mod store;
mod tray;
mod updater;

use std::{collections::HashSet, path::PathBuf, time::Duration};

use chrono::{DateTime, Local};
use gpui::{
    App, Application, AsyncApp, Bounds, ClickEvent, Context, Entity, FontWeight, Global,
    KeyBinding, PathPromptOptions, PromptButton, PromptLevel, SharedString, Task, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

use discovery::LocalRepo;
use health::{ReviewsStatus, SyncHealth};
use notifications::Response;
use store::{PendingReview, Store};
use tray::{Tray, UpdateItem};
use updater::Release;

/// Choices offered in Settings for minutes between GitHub checks.
const POLL_CHOICES: [u64; 7] = [1, 2, 5, 10, 15, 30, 60];
/// Choices offered in Settings and for an individual review's snooze.
const SNOOZE_CHOICES: [u64; 6] = [5, 10, 15, 30, 60, 120];
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

actions!(octowatcher, [Quit, Refresh]);

mod theme {
    pub const BASE: u32 = 0x1e1e2e;
    pub const SURFACE: u32 = 0x313244;
    pub const SURFACE_HOVER: u32 = 0x45475a;
    pub const TEXT: u32 = 0xcdd6f4;
    pub const SUBTEXT: u32 = 0xa6adc8;
    pub const MUTED: u32 = 0x6c7086;
    pub const ACCENT: u32 = 0x89b4fa;
    pub const GREEN: u32 = 0xa6e3a1;
    pub const PEACH: u32 = 0xfab387;
    pub const RED: u32 = 0xf38ba8;
}

#[derive(Clone)]
enum Update {
    /// Newer than the running build, and installable in place.
    Available(Release),
    /// Being swapped in for the running copy.
    Installing(semver::Version),
    /// Installed over the running copy; a restart switches to it.
    Ready(semver::Version),
    /// Couldn't be installed in place; the release page has it.
    Manual(Release),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Reviews,
    Repositories,
    Settings,
}

struct Octowatcher {
    store: Store,
    /// `None` until the first scan of the roots finishes.
    repos: Option<Vec<LocalRepo>>,
    tab: Tab,
    health: SyncHealth,
    folder_issues: Vec<String>,
    load_warning: Option<String>,
    permission: notifications::Permission,
    /// The one PR whose duration picker is open; never persisted as a default.
    snooze_picker: Option<(String, u64)>,
    /// Each source keeps its own error, so a successful GitHub check doesn't
    /// hide a tray, save, permission or delivery error.
    tray_error: Option<String>,
    save_error: Option<String>,
    notification_error: Option<String>,
    notifications_ready: bool,
    /// Whether the reviews waiting at launch were announced yet.
    announced_launch: bool,
    tray: Option<Tray>,
    update: Option<Update>,
    scan_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    /// Fires when the earliest snooze runs out.
    wake_task: Option<Task<()>>,
    update_check: Option<Task<()>>,
    permission_task: Option<Task<()>>,
    _health_clock: Task<()>,
    /// Requests notification permission, starts polling, then schedules updates.
    _startup_and_updates: Task<()>,
}

impl Octowatcher {
    fn new(cx: &mut Context<Self>) -> Self {
        let startup_and_updates = cx.spawn(async move |this, cx| {
            // Discovery and sync do not depend on answering the OS prompt.
            if this
                .update(cx, |this, cx| {
                    this.schedule_poll(cx);
                    this.rescan(cx);
                    this.check_permission(cx);
                })
                .is_err()
            {
                return;
            }
            // Ask without blocking the UI, before any background notification
            // can be sent. macOS only prompts when permission is undecided.
            #[cfg(target_os = "macos")]
            let _authorization = futures_lite::future::race(notifications::request_auth(), async {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                Err(
                    "Notification permission request timed out; check System Settings and Refresh"
                        .into(),
                )
            })
            .await;
            if this
                .update(cx, |this, cx| {
                    this.notifications_ready = true;
                    if this.health.verified && !this.announced_launch {
                        this.announced_launch = true;
                        this.announce_waiting(cx);
                    }
                    this.schedule_wake(cx);
                    this.check_permission(cx);
                })
                .is_err()
            {
                return;
            }

            // A dev build would overwrite its own target dir with a release.
            if cfg!(debug_assertions) {
                return;
            }
            loop {
                if this
                    .update(cx, |this, cx| this.check_for_updates(false, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(UPDATE_INTERVAL).await;
            }
        });
        let (store, load_warning) = Store::load();
        // Saved reviews are unverified for the active account at launch.
        let (tray, tray_error) = match Tray::new(&[], ReviewsStatus::Loading.tray_message()) {
            Ok(tray) => (Some(tray), None),
            Err(err) => (None, Some(format!("could not create tray icon: {err:#}"))),
        };
        let health_clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.sync_tray();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            store,
            repos: None,
            tab: Tab::Reviews,
            health: SyncHealth::default(),
            folder_issues: Vec::new(),
            load_warning,
            permission: notifications::Permission::default(),
            snooze_picker: None,
            tray_error,
            save_error: None,
            notification_error: None,
            notifications_ready: !cfg!(target_os = "macos"),
            announced_launch: false,
            tray,
            update: None,
            scan_task: None,
            fetch_task: None,
            poll_task: None,
            wake_task: None,
            update_check: None,
            permission_task: None,
            _health_clock: health_clock,
            _startup_and_updates: startup_and_updates,
        }
    }

    /// Restarts the countdown to the next check, so a new interval applies now.
    /// The scan at startup does the first check.
    fn schedule_poll(&mut self, cx: &mut Context<Self>) {
        let interval = Duration::from_secs(self.store.poll_minutes.max(1) * 60);
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn set_poll_minutes(&mut self, minutes: u64, cx: &mut Context<Self>) {
        if self.store.poll_minutes == minutes {
            return;
        }
        self.store.poll_minutes = minutes;
        self.save();
        self.schedule_poll(cx);
        cx.notify();
    }

    fn set_snooze_minutes(&mut self, minutes: u64, cx: &mut Context<Self>) {
        if self.store.snooze_minutes == minutes {
            return;
        }
        self.store.snooze_minutes = minutes;
        self.save();
        cx.notify();
    }

    /// Notification actions keep using the global default.
    fn snooze(&mut self, key: (String, u64), cx: &mut Context<Self>) {
        self.snooze_for_minutes(key, self.store.snooze_minutes, cx);
    }

    fn snooze_for_minutes(&mut self, key: (String, u64), minutes: u64, cx: &mut Context<Self>) {
        self.snooze_picker = None;
        if self.store.snooze(&key, minutes, Local::now().timestamp()) {
            self.snoozes_changed(cx);
        } else {
            cx.notify();
        }
    }

    fn unsnooze(&mut self, key: (String, u64), cx: &mut Context<Self>) {
        self.store.unsnooze(&key);
        self.snoozes_changed(cx);
    }

    fn snoozes_changed(&mut self, cx: &mut Context<Self>) {
        self.dismiss_stale_snooze_picker();
        self.save();
        self.sync_tray();
        self.schedule_wake(cx);
        cx.notify();
    }

    /// Sets a timer for the earliest snooze to run out.
    fn schedule_wake(&mut self, cx: &mut Context<Self>) {
        let Some(until) = self.store.next_snooze_until() else {
            self.wake_task = None;
            return;
        };
        let wait = (until - Local::now().timestamp()).max(0) as u64;
        self.wake_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(wait))
                .await;
            this.update(cx, |this, cx| this.wake(cx)).ok();
        }));
    }

    /// Brings back every review whose snooze ran out, and notifies again.
    fn wake(&mut self, cx: &mut Context<Self>) {
        let woken = self.store.take_expired(Local::now().timestamp());
        self.snoozes_changed(cx);
        if self.health.verified && self.notifications_ready {
            self.notify(woken, cx);
        }
    }

    /// Rediscovers local clones, then checks GitHub again.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.health.verified = false;
        let roots = self.store.roots.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let scan = cx
                .background_executor()
                .spawn(async move { discovery::discover(&roots) })
                .await;
            this.update(cx, |this, cx| {
                this.repos = Some(scan.repos);
                this.folder_issues = scan.issues;
                this.scan_task = None;
                // Results fetched against the old repo list are stale.
                this.fetch_task = None;
                this.refresh(cx);
            })
            .ok();
        }));
        self.sync_tray();
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.check_permission(cx);
        // Filtering needs the repo list, and the scan refreshes once it's done.
        if self.repos.is_none() || self.scan_task.is_some() || self.fetch_task.is_some() {
            return;
        }
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { github::check() })
                .await;
            this.update(cx, |this, cx| {
                this.fetch_task = None;
                let (fetched, changed_account) =
                    this.health
                        .apply(result, &mut this.store, Local::now().timestamp());
                if changed_account {
                    this.dismiss_stale_snooze_picker();
                    this.announced_launch = false;
                    this.schedule_wake(cx);
                    this.sync_tray();
                }
                match fetched {
                    Some(fetched) => {
                        this.reconcile(fetched, cx);
                        // Account and success time persist even for an unchanged list.
                        this.save();
                    }
                    None if changed_account => this.save(),
                    None => {}
                }
                this.sync_tray();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn check_permission(&mut self, cx: &mut Context<Self>) {
        if self.permission_task.is_some() {
            return;
        }
        self.permission_task = Some(cx.spawn(async move |this, cx| {
            let permission = futures_lite::future::race(notifications::permission(), async {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                notifications::Permission::Unknown(
                    "Permission check timed out. Refresh to retry.".into(),
                )
            })
            .await;
            this.update(cx, |this, cx| {
                this.permission = permission;
                this.permission_task = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Replaces the pending list with what GitHub reports now, keeping only
    /// enabled local repos. A PR that drops out (reviewed, request removed,
    /// closed) is gone; one seen for the first time raises a notification.
    /// The first check after launch announces everything waiting instead.
    fn reconcile(&mut self, fetched: Vec<PendingReview>, cx: &mut Context<Self>) {
        let watched = self.watched_slugs();
        let fetched: Vec<PendingReview> = fetched
            .into_iter()
            .filter(|pr| watched.contains(&pr.repo.to_lowercase()))
            .collect();
        let reconciled = self.store.reconcile(fetched);
        self.dismiss_stale_snooze_picker();
        // Saving the snoozes also saves the pending list.
        if reconciled.snoozes_changed {
            self.snoozes_changed(cx);
        } else if reconciled.pending_changed {
            self.save();
            self.sync_tray();
        }
        if !self.notifications_ready {
            return;
        }
        if self.announced_launch {
            self.notify(reconciled.fresh, cx);
        } else {
            self.announced_launch = true;
            self.announce_waiting(cx);
        }
    }

    /// Notifies about every review waiting and not snoozed, as a count.
    fn announce_waiting(&mut self, cx: &mut Context<Self>) {
        let awake = self.store.awake();
        let summary = match awake.len() {
            0 => return,
            1 => "You have 1 pending review".to_string(),
            n => format!("You have {n} pending reviews"),
        };
        let body = match awake.as_slice() {
            [pr] => format!("{}#{}: {}", pr.repo, pr.number, pr.title),
            many => pr_list(many),
        };
        self.show_notification(summary, body, None, cx, |_, _, _| {});
    }

    fn send_test_notification(&mut self, cx: &mut Context<Self>) {
        self.show_notification(
            "Notifications work".into(),
            "Octowatcher will tell you here when a review is requested.".into(),
            None,
            cx,
            |_, _, _| {},
        );
    }

    /// Shows a notification, then hands what the user did with it to
    /// `respond`. A failure to show it stays on screen until one succeeds.
    fn show_notification(
        &mut self,
        summary: String,
        body: String,
        action: Option<notifications::Action>,
        cx: &mut Context<Self>,
        respond: impl FnOnce(&mut Self, Response, &mut Context<Self>) + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let shown = notifications::show(&summary, &body, action).await;
            this.update(cx, |this, cx| {
                match shown {
                    Ok(response) => {
                        this.notification_error = None;
                        respond(this, response, cx);
                    }
                    Err(err) => {
                        this.notification_error =
                            Some(format!("could not send notification: {err}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Notifies about reviews to do. A single one gets a button to snooze it.
    fn notify(&mut self, prs: Vec<PendingReview>, cx: &mut Context<Self>) {
        if prs.is_empty() {
            return;
        }
        let key = match prs.as_slice() {
            [pr] => Some(pr.key()),
            _ => None,
        };
        let (summary, body) = notification_text(&prs);
        let action = key.is_some().then_some(("snooze", "Snooze"));
        self.show_notification(summary, body, action, cx, move |this, response, cx| {
            if response == Response::Action("snooze".into())
                && let Some(key) = key
            {
                this.snooze(key, cx);
            }
        });
    }

    fn watched_slugs(&self) -> HashSet<String> {
        self.repos
            .iter()
            .flatten()
            .filter(|repo| self.store.is_enabled(&repo.slug))
            .map(|repo| repo.slug.to_lowercase())
            .collect()
    }

    fn toggle_repo(&mut self, slug: &str, cx: &mut Context<Self>) {
        let key = slug.to_lowercase();
        if self.store.disabled.remove(&key) {
            self.health.verified = false;
            self.save();
            self.refresh(cx);
        } else {
            self.store.disabled.insert(key.clone());
            self.store
                .pending
                .retain(|pr| pr.repo.to_lowercase() != key);
            self.save();
            self.sync_tray();
        }
        cx.notify();
    }

    fn dismiss_stale_snooze_picker(&mut self) {
        if let Some(key) = &self.snooze_picker
            && !self
                .store
                .pending
                .iter()
                .any(|pr| pr.key() == *key && self.store.snooze_for(pr).is_none())
        {
            self.snooze_picker = None;
        }
    }

    fn add_root(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Watch folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| {
                for path in paths {
                    if !this.store.roots.contains(&path) {
                        this.store.roots.push(path);
                    }
                }
                this.save();
                this.rescan(cx);
            })
            .ok();
        })
        .detach();
    }

    fn remove_root(&mut self, root: &PathBuf, cx: &mut Context<Self>) {
        self.store.roots.retain(|r| r != root);
        self.save();
        self.rescan(cx);
    }

    /// Looks for a newer release. Scheduled checks announce a new one in a
    /// notification; `manual` checks come from the tray and answer in a dialog.
    fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.update_check.is_some() || matches!(self.update, Some(Update::Installing(_))) {
            return;
        }
        self.update_check = Some(cx.spawn(async move |this, cx| {
            let checked = cx
                .background_executor()
                .spawn(async { updater::check() })
                .await;
            let Ok(state) = this.update(cx, |this, cx| {
                this.update_check = None;
                if let Ok(Some(release)) = &checked
                    && this.found(release.clone())
                    && !manual
                {
                    this.announce_update(cx);
                }
                this.sync_tray();
                cx.notify();
                this.update.clone()
            }) else {
                return;
            };
            let release = match checked {
                Ok(Some(release)) => release,
                Ok(None) => {
                    if manual {
                        let detail = format!(
                            "Version {} is the latest release.",
                            updater::current_version()
                        );
                        ask(
                            cx,
                            PromptLevel::Info,
                            "Octowatcher is up to date",
                            &detail,
                            &[PromptButton::ok("OK")],
                        )
                        .await;
                    }
                    return;
                }
                Err(err) => {
                    eprintln!("could not check for updates: {err:#}");
                    if manual {
                        ask(
                            cx,
                            PromptLevel::Warning,
                            "Could not check for updates",
                            &format!("{err:#}"),
                            &[PromptButton::ok("OK")],
                        )
                        .await;
                    }
                    return;
                }
            };
            if !manual {
                return;
            }
            match state {
                Some(Update::Ready(version)) => ask_restart(&version, cx).await,
                Some(Update::Available(_)) => {
                    let message = format!("Octowatcher {} is available", release.version);
                    let detail = format!("You have {}.", updater::current_version());
                    let answer = ask(
                        cx,
                        PromptLevel::Info,
                        &message,
                        &detail,
                        &[PromptButton::ok("Update"), PromptButton::cancel("Later")],
                    )
                    .await;
                    if answer == Some(0) {
                        this.update(cx, |this, cx| this.install_update(cx)).ok();
                    }
                }
                Some(Update::Manual(release)) => {
                    let message = format!("Octowatcher {} is available", release.version);
                    let answer = ask(
                        cx,
                        PromptLevel::Info,
                        &message,
                        "Download it from the release page.",
                        &[PromptButton::ok("Download"), PromptButton::cancel("Later")],
                    )
                    .await;
                    if answer == Some(0) {
                        cx.update(|cx| cx.open_url(&release.url)).ok();
                    }
                }
                Some(Update::Installing(_)) | None => {}
            }
        }));
        self.sync_tray();
    }

    /// Records a release newer than the running build. Returns false when
    /// it, or something newer, was already known.
    fn found(&mut self, release: Release) -> bool {
        let known = match &self.update {
            Some(Update::Available(known) | Update::Manual(known)) => Some(&known.version),
            Some(Update::Installing(version) | Update::Ready(version)) => Some(version),
            None => None,
        };
        if known.is_some_and(|version| *version >= release.version) {
            return false;
        }
        self.update = Some(if updater::can_install(&release) {
            Update::Available(release)
        } else {
            Update::Manual(release)
        });
        true
    }

    /// Notifies about the update just found, with a button to act on it.
    fn announce_update(&mut self, cx: &mut Context<Self>) {
        let (release, body, action) = match &self.update {
            Some(Update::Available(release)) => (
                release,
                "Install it now, or later from the tray menu.",
                ("update", "Update"),
            ),
            Some(Update::Manual(release)) => (
                release,
                "Download it from the release page.",
                ("download", "Download"),
            ),
            _ => return,
        };
        let summary = format!("Octowatcher {} is available", release.version);
        let url = release.url.clone();
        self.show_notification(
            summary,
            body.into(),
            Some(action),
            cx,
            move |this, response, cx| match response {
                Response::Action(id) if id == "update" => this.install_update(cx),
                Response::Action(id) if id == "download" => cx.open_url(&url),
                Response::Clicked => show_window(cx),
                _ => {}
            },
        );
    }

    /// Swaps in the available update, then offers to restart into it.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let Some(Update::Available(release)) = self.update.clone() else {
            return;
        };
        self.update = Some(Update::Installing(release.version.clone()));
        self.sync_tray();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let release = release.clone();
                    async move { updater::install(&release) }
                })
                .await;
            let updated = this.update(cx, |this, cx| {
                this.update = Some(match &result {
                    Ok(path) => {
                        // Linux relaunches the executable path, which
                        // reads as deleted once it was replaced.
                        cx.set_restart_path(path.clone());
                        Update::Ready(release.version.clone())
                    }
                    Err(err) => {
                        eprintln!("could not install update: {err:#}");
                        Update::Manual(release.clone())
                    }
                });
                this.sync_tray();
                cx.notify();
            });
            if updated.is_err() {
                return;
            }
            match result {
                Ok(_) => ask_restart(&release.version, cx).await,
                Err(err) => {
                    let answer = ask(
                        cx,
                        PromptLevel::Warning,
                        "Could not install the update",
                        &format!("{err:#}"),
                        &[PromptButton::ok("Download"), PromptButton::cancel("Later")],
                    )
                    .await;
                    if answer == Some(0) {
                        cx.update(|cx| cx.open_url(&release.url)).ok();
                    }
                }
            }
        })
        .detach();
    }

    fn sync_tray(&mut self) {
        let Some(tray) = &self.tray else { return };
        let item = match &self.update {
            _ if self.update_check.is_some() => UpdateItem::Checking,
            Some(Update::Available(release)) => UpdateItem::Available(&release.version),
            Some(Update::Installing(version)) => UpdateItem::Installing(version),
            Some(Update::Ready(version)) => UpdateItem::Ready(version),
            Some(Update::Manual(_)) | None => UpdateItem::Check,
        };
        let awake = if self.health.verified {
            self.store.awake()
        } else {
            Vec::new()
        };
        self.tray_error = tray
            .update(&awake, item, self.review_status().tray_message())
            .err()
            .map(|err| format!("could not update tray menu: {err:#}"));
    }

    fn save(&mut self) {
        self.save_error = self
            .store
            .save()
            .err()
            .map(|err| format!("could not save state: {err:#}"));
    }

    fn errors(&self) -> Vec<&str> {
        [
            &self.load_warning,
            &self.health.error,
            &self.save_error,
            &self.tray_error,
            &self.notification_error,
        ]
        .into_iter()
        .filter_map(Option::as_deref)
        .collect()
    }

    fn review_status(&self) -> ReviewsStatus {
        self.health.status(
            &self.store,
            Local::now().timestamp(),
            self.repos.is_none() || self.scan_task.is_some(),
            self.fetch_task.is_some(),
            self.watched_slugs().len(),
            !self.folder_issues.is_empty(),
        )
    }
}

fn notification_text(prs: &[PendingReview]) -> (String, String) {
    match prs {
        [pr] => (
            format!(
                "{} requested your {}",
                pr.author,
                if pr.rereview { "re-review" } else { "review" }
            ),
            format!("{}#{}: {}", pr.repo, pr.number, pr.title),
        ),
        many => (
            format!("{} pull requests need your review", many.len()),
            pr_list(many),
        ),
    }
}

fn pr_list(prs: &[PendingReview]) -> String {
    prs.iter()
        .map(|pr| format!("{}#{}", pr.repo, pr.number))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Render for Octowatcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.tab {
            Tab::Reviews => self.render_reviews(cx).into_any_element(),
            Tab::Repositories => self.render_repositories(cx).into_any_element(),
            Tab::Settings => self.render_settings(cx).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme::BASE))
            .text_color(rgb(theme::TEXT))
            .text_sm()
            .child(self.render_header(cx))
            .child(
                div()
                    .id("content")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_4()
                    .child(content),
            )
    }
}

impl Octowatcher {
    fn last_sync_text(&self) -> String {
        match self
            .store
            .last_successful_sync
            .and_then(|at| DateTime::from_timestamp(at, 0))
        {
            Some(at) => format!(
                "Last successful sync: {}",
                at.with_timezone(&Local).format("%b %d, %H:%M")
            ),
            None => "Last successful sync: never".into(),
        }
    }

    fn render_health(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let github = match &self.health.readiness {
            github::Readiness::Checking => "Checking GitHub CLI and active github.com account…".into(),
            github::Readiness::Missing => "GitHub CLI not found. Install gh, run gh auth login --hostname github.com in a terminal, then Refresh.".into(),
            github::Readiness::NotRunnable => "GitHub CLI could not start. Check its installation and executable permissions, then Refresh.".into(),
            github::Readiness::SignedOut => "GitHub CLI is available; authentication needs attention. Run gh auth login --hostname github.com in a terminal, then Refresh.".into(),
            github::Readiness::Offline => format!("GitHub CLI is available; GitHub cannot be reached (offline or network error). Check your connection or proxy, then Refresh.{}", self.store.sync_account.as_ref().map(|login| format!(" Last verified account: @{login}.")).unwrap_or_default()),
            github::Readiness::Unavailable => format!("GitHub CLI is available; account readiness could not be verified. Check your connection, proxy, or GitHub availability, then Refresh.{}", self.store.sync_account.as_ref().map(|login| format!(" Last verified account: @{login}.")).unwrap_or_default()),
            github::Readiness::Ready(login) => format!("GitHub CLI ready · active account @{login} on github.com.{}", if self.health.error.is_some() { " Review sync failed; cached reviews may be stale." } else { "" }),
        };
        let folders = if self.repos.is_none() || self.scan_task.is_some() {
            "Scanning watched folders…".into()
        } else if self.store.roots.is_empty() {
            "No watched folders. Add a folder containing GitHub clones.".into()
        } else if !self.folder_issues.is_empty() {
            format!(
                "Folder scan incomplete: {} issue(s). Fix the paths or access permissions, then Rescan.",
                self.folder_issues.len()
            )
        } else if self.repos.as_ref().is_some_and(Vec::is_empty) {
            "No GitHub clones found. Add a project folder or clone a repository there, then Rescan."
                .into()
        } else if self.watched_slugs().is_empty() {
            "Every discovered repository is off. Enable a repository in Repositories.".into()
        } else {
            format!(
                "{} repositories enabled in {} watched folder(s).",
                self.watched_slugs().len(),
                self.store.roots.len()
            )
        };
        let permission = match &self.permission {
            notifications::Permission::Checking => "Checking notification permission…".into(),
            notifications::Permission::Allowed(detail) => detail.clone(),
            notifications::Permission::Denied => "Disabled. Enable Allow Notifications for Octowatcher in System Settings → Notifications, then Refresh.".into(),
            notifications::Permission::Unknown(detail) => format!("Unknown · {detail}"),
        };
        let sync = match self.review_status() {
            ReviewsStatus::Loading => {
                "Checking setup and reviews. Saved results are unverified until sync succeeds."
            }
            ReviewsStatus::Setup => {
                "Folder setup needs attention. Review coverage may be incomplete."
            }
            ReviewsStatus::Unavailable => {
                "Sync failed. Cached reviews may be stale; an empty cache cannot confirm there are no requests. Automatic checks continue at your configured interval."
            }
            ReviewsStatus::Offline => {
                "GitHub cannot be reached. Cached reviews may be stale; check your connection or proxy and Refresh. Automatic checks continue at your configured interval."
            }
            ReviewsStatus::Stale => {
                "Reviews may be stale. Refresh to verify the current list. Automatic checks continue at your configured interval."
            }
            ReviewsStatus::Ready => "Reviews are up to date for your enabled repositories.",
        };
        div().flex().flex_col().gap_3().p_3().rounded_lg().bg(rgb(theme::SURFACE))
            .child(section_title("Setup & health"))
            .child(health_row("GitHub", github))
            .when(matches!(self.health.readiness, github::Readiness::Missing), |s| s.child(button("install-gh", "Install GitHub CLI").on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.open_url("https://cli.github.com/")))))
            .when(matches!(self.health.readiness, github::Readiness::Missing | github::Readiness::SignedOut), |s| s.child(button("copy-login", "Copy login command").on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("gh auth login --hostname github.com".into()))))))
            .child(health_row("Watched folders", folders))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("health-folders", "Manage folders").on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.tab = Tab::Repositories; this.snooze_picker = None; cx.notify(); })))
                .child(button("health-add-folder", "Add folder…").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_root(cx))))
                .child(button("health-rescan", "Rescan").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.rescan(cx)))))
            .child(health_row("Notification permission", permission))
            .child(div().flex().flex_wrap().gap_2()
                .when(cfg!(target_os = "macos"), |s| s.child(button("health-notification-settings", "Notification settings").on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.open_url("x-apple.systempreferences:com.apple.Notifications-Settings.extension")))))
                .child(button("health-test-notification", "Send test notification").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.send_test_notification(cx)))))
            .child(health_row("Review sync", format!("{} · {sync}", self.last_sync_text())))
            .child(button("health-refresh", "Refresh").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx))))
            .children(self.errors().into_iter().chain(self.folder_issues.iter().map(String::as_str)).map(|error| div().text_xs().text_color(rgb(theme::RED)).child(error.to_string())))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status: SharedString = if self.scan_task.is_some() {
            "Scanning folders…".into()
        } else if self.fetch_task.is_some() {
            "Checking GitHub…".into()
        } else {
            match self.review_status() {
                ReviewsStatus::Loading => "Checking setup…".into(),
                ReviewsStatus::Setup => "Setup needs attention".into(),
                ReviewsStatus::Unavailable => "Sync failed · reviews unverified".into(),
                ReviewsStatus::Offline => "GitHub unreachable · reviews stale".into(),
                ReviewsStatus::Stale => "Reviews may be stale".into(),
                ReviewsStatus::Ready => "Reviews up to date".into(),
            }
        };
        let repo_count = self.repos.as_ref().map_or(0, Vec::len);

        div()
            .flex()
            .flex_col()
            .gap_3()
            .px_4()
            .pt_4()
            .pb_3()
            .border_b_1()
            .border_color(rgb(theme::SURFACE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Octowatcher"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(status))
                            .child(button("refresh", "Refresh").on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx)),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::SUBTEXT))
                            .child(self.last_sync_text()),
                    )
                    .child(
                        button("health-details", "Setup & health")
                            .debug_selector(|| "health-details".to_string())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.tab = Tab::Settings;
                                this.snooze_picker = None;
                                cx.notify();
                            })),
                    ),
            )
            .when(!self.errors().is_empty(), |s| {
                s.child(div().text_xs().text_color(rgb(theme::RED)).child(format!(
                    "{} issue(s) need attention — see Setup & health",
                    self.errors().len()
                )))
            })
            .children(self.render_update(cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.render_tab(
                        Tab::Reviews,
                        format!("Reviews ({})", self.store.pending.len()),
                        cx,
                    ))
                    .child(self.render_tab(
                        Tab::Repositories,
                        format!("Repositories ({repo_count})"),
                        cx,
                    ))
                    .child(self.render_tab(Tab::Settings, "Settings".into(), cx)),
            )
    }

    fn render_update(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (message, action) =
            match self.update.as_ref()? {
                Update::Available(release) => (
                    format!("Octowatcher {} is available.", release.version),
                    Some(button("install-update", "Update").on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.install_update(cx)),
                    )),
                ),
                Update::Installing(version) => (format!("Installing Octowatcher {version}…"), None),
                Update::Ready(version) => (
                    format!("Octowatcher {version} is installed."),
                    Some(
                        button("restart", "Restart")
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.restart())),
                    ),
                ),
                Update::Manual(release) => {
                    let url = release.url.clone();
                    (
                        format!("Octowatcher {} is available.", release.version),
                        Some(button("download-update", "Download").on_click(
                            cx.listener(move |_, _: &ClickEvent, _, cx| cx.open_url(&url)),
                        )),
                    )
                }
            };
        Some(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(theme::SURFACE))
                .text_xs()
                .child(message)
                .children(action),
        )
    }

    fn render_tab(&self, tab: Tab, label: String, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.tab == tab;
        let id = match tab {
            Tab::Reviews => "tab-reviews",
            Tab::Repositories => "tab-repositories",
            Tab::Settings => "tab-settings",
        };
        div()
            .id(id)
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(active, |s| {
                s.bg(rgb(theme::SURFACE)).text_color(rgb(theme::TEXT))
            })
            .when(!active, |s| {
                s.text_color(rgb(theme::SUBTEXT))
                    .hover(|s| s.bg(rgb(theme::SURFACE)))
            })
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.tab = tab;
                this.snooze_picker = None;
                cx.notify();
            }))
    }

    fn render_reviews(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.review_status();
        let needs_health = status != ReviewsStatus::Ready
            || !self.errors().is_empty()
            || matches!(self.permission, notifications::Permission::Denied)
            || (cfg!(target_os = "macos")
                && matches!(self.permission, notifications::Permission::Unknown(_)));
        if self.store.pending.is_empty() {
            return div()
                .flex()
                .flex_col()
                .gap_6()
                .when(needs_health, |s| s.child(self.render_health(cx)))
                .child(
                    div()
                        .flex()
                        .justify_center()
                        .pt_6()
                        .text_color(rgb(theme::SUBTEXT))
                        .child(status.empty_message()),
                );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .when(needs_health, |s| s.child(self.render_health(cx)))
            .when(status != ReviewsStatus::Ready, |s| {
                s.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::PEACH))
                        .child("Last known reviews · may be stale until a successful sync."),
                )
            })
            .children(self.store.pending.iter().enumerate().map(|(ix, pr)| {
                let url = pr.url.clone();
                let key = pr.key();
                let picker_open = self.snooze_picker.as_ref() == Some(&key);
                let snoozed_until = self.store.snooze_for(pr).map(|snooze| {
                    DateTime::from_timestamp(snooze.until, 0)
                        .map(|at| at.with_timezone(&Local).format("%H:%M").to_string())
                        .unwrap_or_default()
                });
                let action = if snoozed_until.is_some() {
                    button(("unsnooze", ix), "Unsnooze").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            // The card behind the button opens the PR.
                            cx.stop_propagation();
                            this.unsnooze(key.clone(), cx);
                        },
                    ))
                } else {
                    button(("snooze", ix), "Snooze…")
                        .debug_selector(move || format!("snooze-{ix}"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.snooze_picker = if picker_open { None } else { Some(key.clone()) };
                            cx.notify();
                        }))
                };
                let badge = if pr.rereview {
                    Some(("re-review", theme::PEACH))
                } else {
                    None
                };
                div()
                    .id(("review", ix))
                    .debug_selector(move || format!("review-{ix}"))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(theme::SURFACE))
                    .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
                    .cursor_pointer()
                    .when(snoozed_until.is_some(), |s| s.opacity(0.6))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(rgb(theme::SUBTEXT))
                                    .child(format!("{}#{}", pr.repo, pr.number))
                                    .children(badge.map(|(label, color)| pill(label, color)))
                                    .when(pr.is_draft, |s| s.child(pill("draft", theme::MUTED))),
                            )
                            .child(action),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(pr.title.clone()),
                    )
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                        match &snoozed_until {
                            Some(at) => format!("by {} · snoozed until {at}", pr.author),
                            None => format!("by {}", pr.author),
                        },
                    ))
                    .when(picker_open && snoozed_until.is_none(), |card| {
                        let key = pr.key();
                        card.child(snooze_picker(
                            ix,
                            self.store.snooze_minutes,
                            cx,
                            move |this, minutes, cx| {
                                if let Some(minutes) = minutes {
                                    this.snooze_for_minutes(key.clone(), minutes, cx);
                                } else {
                                    this.snooze_picker = None;
                                    cx.notify();
                                }
                            },
                        ))
                    })
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.open_url(&url)))
            }))
    }

    fn render_repositories(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let roots = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Watched folders"))
            .children(self.store.roots.iter().enumerate().map(|(ix, root)| {
                let root_for_click = root.clone();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE))
                    .child(display_path(root))
                    .child(button(("remove-root", ix), "Remove").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| this.remove_root(&root_for_click, cx),
                    )))
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("add-root", "Add folder…")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_root(cx))),
                    )
                    .child(
                        button("rescan", "Rescan")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.rescan(cx))),
                    ),
            );

        let repos = self.repos.as_deref().unwrap_or_default();
        let list = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("GitHub repositories found"))
            .when(repos.is_empty() && self.scan_task.is_none(), |s| {
                s.child(
                    div()
                        .text_color(rgb(theme::MUTED))
                        .child("No GitHub clones in these folders."),
                )
            })
            .children(repos.iter().enumerate().map(|(ix, repo)| {
                let enabled = self.store.is_enabled(&repo.slug);
                let slug = repo.slug.clone();
                let paths = repo
                    .paths
                    .iter()
                    .map(|p| display_path(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                div()
                    .id(("repo", ix))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE))
                    .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .when(!enabled, |s| s.text_color(rgb(theme::MUTED)))
                            .child(repo.slug.clone())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::MUTED))
                                    .truncate()
                                    .child(paths),
                            ),
                    )
                    .child(if enabled {
                        pill("watching", theme::GREEN)
                    } else {
                        pill("off", theme::MUTED)
                    })
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_repo(&slug, cx)),
                    )
            }));

        div().flex().flex_col().gap_6().child(roots).child(list)
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.render_health(cx))
            .child(self.render_choices(
                "Check GitHub for review requests every",
                "poll",
                &POLL_CHOICES,
                self.store.poll_minutes,
                Self::set_poll_minutes,
                cx,
            ))
            .child(self.render_choices(
                "Default snooze length",
                "snooze-minutes",
                &SNOOZE_CHOICES,
                self.store.snooze_minutes,
                Self::set_snooze_minutes,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_title("Notifications"))
                    .child(div().flex().child(
                        button("test-notification", "Send test notification").on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.send_test_notification(cx)
                            }),
                        ),
                    )),
            )
    }

    /// A row of minute lengths to pick one from.
    fn render_choices(
        &self,
        title: &'static str,
        id: &'static str,
        choices: &[u64],
        current: u64,
        pick: fn(&mut Self, u64, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title(title))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(choices.iter().map(|&minutes| {
                        let active = minutes == current;
                        let label = minutes_label(minutes);
                        div()
                            .id((id, minutes as usize))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .text_xs()
                            .cursor_pointer()
                            .when(active, |s| {
                                s.bg(rgb(theme::ACCENT)).text_color(rgb(theme::BASE))
                            })
                            .when(!active, |s| {
                                s.bg(rgb(theme::SURFACE))
                                    .text_color(rgb(theme::SUBTEXT))
                                    .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
                            })
                            .child(label)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                pick(this, minutes, cx)
                            }))
                    })),
            )
    }
}

/// Inline choices stay inside the card, but none of their clicks open its URL.
fn snooze_picker<T: 'static>(
    ix: usize,
    default_minutes: u64,
    cx: &mut Context<T>,
    pick: impl Fn(&mut T, Option<u64>, &mut Context<T>) + Clone + 'static,
) -> gpui::Stateful<gpui::Div> {
    // Keep a default saved by an older build available even if it isn't a preset.
    let choices = SNOOZE_CHOICES
        .into_iter()
        .chain((!SNOOZE_CHOICES.contains(&default_minutes)).then_some(default_minutes));
    div()
        .id(("snooze-picker", ix))
        .debug_selector(move || format!("snooze-picker-{ix}"))
        .flex()
        .flex_col()
        .gap_2()
        .pt_2()
        .cursor_default()
        .child(section_title("Snooze for"))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .children(choices.map(|minutes| {
                    let pick = pick.clone();
                    let is_default = minutes == default_minutes;
                    let label = if is_default {
                        format!("{} (default)", minutes_label(minutes))
                    } else {
                        minutes_label(minutes)
                    };
                    button(("snooze-duration", minutes as usize), label)
                        .debug_selector(move || format!("snooze-duration-{ix}-{minutes}"))
                        .when(is_default, |s| {
                            s.border_1().border_color(rgb(theme::ACCENT))
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            pick(this, Some(minutes), cx);
                        }))
                }))
                .child(
                    button("cancel-snooze", "Cancel")
                        .debug_selector(move || format!("cancel-snooze-{ix}"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            pick(this, None, cx);
                        })),
                ),
        )
        .on_click(|_: &ClickEvent, _, cx| cx.stop_propagation())
}

fn minutes_label(minutes: u64) -> String {
    if minutes < 60 || !minutes.is_multiple_of(60) {
        format!("{minutes} min")
    } else {
        format!("{} h", minutes / 60)
    }
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgb(theme::SURFACE))
        .text_color(rgb(theme::ACCENT))
        .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
        .cursor_pointer()
        .child(label.into())
}

fn pill(label: &'static str, color: u32) -> gpui::Div {
    div()
        .px_2()
        .rounded_full()
        .text_xs()
        .border_1()
        .border_color(rgb(color))
        .text_color(rgb(color))
        .child(label)
}

fn section_title(label: &'static str) -> gpui::Div {
    div()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(theme::SUBTEXT))
        .child(label)
}

fn health_row(label: &'static str, detail: String) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_title(label))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::SUBTEXT))
                .child(detail),
        )
}

fn display_path(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

fn main() {
    let app = Application::new();
    // Clicking the dock icon with the window closed brings it back.
    app.on_reopen(show_window);
    app.run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &Refresh, cx| refresh(cx));
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-r", Refresh, None),
        ]);
        cx.set_menus(vec![gpui::Menu {
            name: "Octowatcher".into(),
            items: vec![
                gpui::MenuItem::action("Refresh", Refresh),
                gpui::MenuItem::separator(),
                gpui::MenuItem::action("Quit", Quit),
            ],
        }]);
        tray::listen(cx);

        // The app owns the state rather than the window, so closing the
        // window keeps polling and the tray icon alive until Quit.
        let octowatcher = cx.new(Octowatcher::new);
        cx.set_global(MainView(octowatcher));
        show_window(cx);
    });
}

struct MainView(Entity<Octowatcher>);

impl Global for MainView {}

/// Checks GitHub for review requests now, from the tray or the app menu.
pub fn refresh(cx: &mut App) {
    let view = cx.global::<MainView>().0.clone();
    view.update(cx, |this, cx| this.refresh(cx));
}

/// Checks for a new release now, from the tray.
pub fn check_for_updates(cx: &mut App) {
    let view = cx.global::<MainView>().0.clone();
    view.update(cx, |this, cx| this.check_for_updates(true, cx));
}

/// Installs the available update, from the tray.
pub fn install_update(cx: &mut App) {
    let view = cx.global::<MainView>().0.clone();
    view.update(cx, |this, cx| this.install_update(cx));
}

/// Shows a dialog over the window, opening it first if it was closed, and
/// resolves to the index of the button clicked. On macOS this is a native
/// alert; Linux has none, so gpui draws one in the window.
async fn ask(
    cx: &mut AsyncApp,
    level: PromptLevel,
    message: &str,
    detail: &str,
    answers: &[PromptButton],
) -> Option<usize> {
    let answer = cx
        .update(|cx| {
            show_window(cx);
            let window = *cx.windows().first()?;
            window
                .update(cx, |_, window, cx| {
                    window.prompt(level, message, Some(detail), answers, cx)
                })
                .ok()
        })
        .ok()
        .flatten()?;
    answer.await.ok()
}

async fn ask_restart(version: &semver::Version, cx: &mut AsyncApp) {
    let message = format!("Octowatcher {version} is installed");
    let answer = ask(
        cx,
        PromptLevel::Info,
        &message,
        "Restart now to start using it?",
        &[
            PromptButton::ok("Restart Now"),
            PromptButton::cancel("Later"),
        ],
    )
    .await;
    if answer == Some(0) {
        cx.update(|cx| cx.restart()).ok();
    }
}

/// Brings the window to the front, opening it again if it was closed.
pub fn show_window(cx: &mut App) {
    cx.activate(true);
    if let Some(window) = cx.windows().first() {
        window
            .update(cx, |_, window, _| window.activate_window())
            .ok();
        return;
    }
    let view = cx.global::<MainView>().0.clone();
    let bounds = Bounds::centered(None, size(px(560.), px(680.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |_, _| view,
    )
    .unwrap();
}

#[cfg(test)]
mod snooze_tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    fn review(number: u64) -> PendingReview {
        PendingReview {
            repo: "owner/repo".into(),
            number,
            title: format!("Review {number}"),
            url: format!("https://github.com/owner/repo/pull/{number}"),
            author: "author".into(),
            is_draft: false,
            rereview: false,
            requested_at: None,
        }
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx.debug_bounds(selector).expect("visible control");
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    // No startup, tray, GitHub, config-file or notification side effects.
    fn app_for_picker_test() -> Octowatcher {
        Octowatcher {
            store: Store {
                pending: vec![review(1), review(2)],
                sync_account: Some("test-viewer".into()),
                last_successful_sync: Some(Local::now().timestamp()),
                ..Store::default()
            },
            repos: Some(vec![LocalRepo {
                slug: "owner/repo".into(),
                paths: Vec::new(),
            }]),
            tab: Tab::Reviews,
            snooze_picker: None,
            health: SyncHealth {
                readiness: github::Readiness::Ready("test-viewer".into()),
                verified: true,
                ..SyncHealth::default()
            },
            folder_issues: Vec::new(),
            load_warning: None,
            permission: notifications::Permission::Allowed("Allowed".into()),
            tray_error: None,
            save_error: None,
            notification_error: None,
            notifications_ready: true,
            announced_launch: true,
            tray: None,
            update: None,
            scan_task: None,
            fetch_task: None,
            poll_task: None,
            wake_task: None,
            update_check: None,
            permission_task: None,
            _health_clock: Task::ready(()),
            _startup_and_updates: Task::ready(()),
        }
    }

    #[test]
    fn picker_tracks_the_pr_key_and_closes_when_the_review_disappears_or_is_snoozed() {
        let mut app = app_for_picker_test();
        let key = review(1).key();
        app.snooze_picker = Some(key.clone());
        app.store.pending.reverse();
        app.dismiss_stale_snooze_picker();
        assert_eq!(app.snooze_picker, Some(key.clone()));
        app.store.snooze(&key, 30, 1_000);
        app.dismiss_stale_snooze_picker();
        assert_eq!(app.snooze_picker, None);
        app.snooze_picker = Some(key);
        app.store.pending.retain(|pr| pr.number != 1);
        app.dismiss_stale_snooze_picker();
        assert_eq!(app.snooze_picker, None);
    }

    #[gpui::test]
    fn health_navigation_dismisses_the_snooze_picker(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| app_for_picker_test());
        click(cx, "snooze-0");
        assert!(view.read_with(cx, |v, _| v.snooze_picker.is_some()));
        click(cx, "health-details");
        assert!(view.read_with(cx, |v, _| v.tab == Tab::Settings
            && v.snooze_picker.is_none()));
        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    fn opening_switching_and_canceling_pickers_never_opens_the_pr(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| app_for_picker_test());
        let closed_height = cx.debug_bounds("review-0").unwrap().size.height;
        click(cx, "snooze-0");
        assert_eq!(
            view.read_with(cx, |v, _| v.snooze_picker.clone()),
            Some(review(1).key())
        );
        assert!(cx.debug_bounds("snooze-picker-0").is_some());
        assert!(cx.debug_bounds("review-0").unwrap().size.height > closed_height);
        click(cx, "snooze-1");
        assert_eq!(
            view.read_with(cx, |v, _| v.snooze_picker.clone()),
            Some(review(2).key())
        );
        // GPUI retains old debug selectors; check the card's current layout.
        assert_eq!(
            cx.debug_bounds("review-0").unwrap().size.height,
            closed_height
        );
        assert!(cx.debug_bounds("snooze-picker-1").is_some());
        click(cx, "cancel-snooze-1");
        assert!(view.read_with(cx, |v, _| v.snooze_picker.is_none()
            && v.store.snoozed.is_empty()));
        assert_eq!(cx.opened_url(), None);
        click(cx, "snooze-0");
        click(cx, "snooze-0");
        assert!(view.read_with(cx, |v, _| v.snooze_picker.is_none()));
        assert_eq!(cx.opened_url(), None);
        click(cx, "review-0");
        assert_eq!(cx.opened_url(), Some(review(1).url));
    }

    struct PickerHarness {
        store: Store,
        picked: Vec<Option<u64>>,
    }

    impl Render for PickerHarness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("card")
                .w(px(360.))
                .p_3()
                .text_sm()
                .child(snooze_picker(
                    0,
                    self.store.snooze_minutes,
                    cx,
                    |this, minutes, _| {
                        this.picked.push(minutes);
                        if let Some(minutes) = minutes {
                            this.store.snooze(&review(1).key(), minutes, 1_000);
                        }
                    },
                ))
                .on_click(|_: &ClickEvent, _, cx| cx.open_url(&review(1).url))
        }
    }

    #[gpui::test]
    fn every_duration_and_picker_background_stop_click_propagation(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| PickerHarness {
            store: Store {
                pending: vec![review(1)],
                snooze_minutes: 15,
                ..Store::default()
            },
            picked: Vec::new(),
        });
        for (minutes, selector) in [
            (5, "snooze-duration-0-5"),
            (10, "snooze-duration-0-10"),
            (15, "snooze-duration-0-15"),
            (30, "snooze-duration-0-30"),
            (60, "snooze-duration-0-60"),
            (120, "snooze-duration-0-120"),
        ] {
            click(cx, selector);
            view.read_with(cx, |v, _| {
                assert_eq!(v.picked.last(), Some(&Some(minutes)));
                assert_eq!(
                    v.store.next_snooze_until(),
                    Some(1_000 + minutes as i64 * 60)
                );
                assert_eq!(v.store.snooze_minutes, 15);
            });
            assert_eq!(cx.opened_url(), None);
        }
        // The panel heading/padding must also consume the card's click.
        let panel = cx.debug_bounds("snooze-picker-0").unwrap();
        cx.simulate_click(
            panel.origin + gpui::point(px(2.), px(2.)),
            Modifiers::none(),
        );
        assert_eq!(cx.opened_url(), None);
        assert_eq!(view.read_with(cx, |v, _| v.picked.len()), 6);
        click(cx, "cancel-snooze-0");
        assert_eq!(
            view.read_with(cx, |v, _| v.picked.last().copied()),
            Some(None)
        );
        assert_eq!(cx.opened_url(), None);
    }
}
