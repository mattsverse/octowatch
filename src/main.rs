mod discovery;
mod github;
mod notifications;
mod review_filter;
mod search_input;
mod store;
mod tray;
mod updater;

use std::{collections::HashSet, path::PathBuf, time::Duration};

use chrono::{DateTime, Local};
use gpui::{
    App, Application, AsyncApp, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable,
    FontWeight, Global, KeyBinding, ListAlignment, ListOffset, ListState, PathPromptOptions,
    PromptButton, PromptLevel, SharedString, Subscription, Task, Window, WindowBounds,
    WindowOptions, actions, div, list, prelude::*, px, rgb, size,
};

use discovery::LocalRepo;
use notifications::Response;
use review_filter::{DraftFilter, ReviewFilter, ReviewFilters, SnoozeFilter};
use search_input::SearchInput;
use store::{PendingReview, Store};
use tray::{Tray, UpdateItem};
use updater::Release;

/// Choices offered in Settings for minutes between GitHub checks.
const POLL_CHOICES: [u64; 7] = [1, 2, 5, 10, 15, 30, 60];
/// Choices offered in Settings and for an individual review's snooze.
const SNOOZE_CHOICES: [u64; 6] = [5, 10, 15, 30, 60, 120];
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

actions!(
    octowatcher,
    [Quit, Refresh, FocusReviewSearch, ResetReviewFilters]
);

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

#[derive(PartialEq, Eq)]
struct ReviewListItem {
    key: (String, u64),
    picker_open: bool,
}

struct Octowatcher {
    store: Store,
    /// `None` until the first scan of the roots finishes.
    repos: Option<Vec<LocalRepo>>,
    tab: Tab,
    review_filters: ReviewFilters,
    review_search: Entity<SearchInput>,
    repository_picker_open: bool,
    review_scroll: ListState,
    review_list_items: Vec<ReviewListItem>,
    /// The one PR whose duration picker is open; never persisted as a default.
    snooze_picker: Option<(String, u64)>,
    focus_handle: FocusHandle,
    _search_subscription: Subscription,
    last_checked: Option<DateTime<Local>>,
    /// Each source keeps its own error, so a successful GitHub check doesn't
    /// hide a tray, save, permission or delivery error.
    fetch_error: Option<String>,
    tray_error: Option<String>,
    save_error: Option<String>,
    notification_error: Option<String>,
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
    /// Requests notification permission, starts polling, then schedules updates.
    _startup_and_updates: Task<()>,
}

impl Octowatcher {
    fn new(cx: &mut Context<Self>) -> Self {
        let startup_and_updates = cx.spawn(async move |this, cx| {
            // Ask without blocking the UI, before any background notification
            // can be sent. macOS only prompts when permission is undecided.
            #[cfg(target_os = "macos")]
            let notification_error = match notifications::request_auth().await {
                Ok(true) => None,
                Ok(false) => Some(
                    "Notifications are disabled. Enable Allow Notifications for Octowatcher in System Settings → Notifications."
                        .into(),
                ),
                Err(err) => Some(format!("could not request notification permission: {err}")),
            };
            if this
                .update(cx, |this, cx| {
                    #[cfg(target_os = "macos")]
                    {
                        this.notification_error = notification_error;
                    }
                    this.schedule_poll(cx);
                    this.schedule_wake(cx);
                    this.rescan(cx);
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
        let store = Store::load();
        let review_search = cx.new(SearchInput::new);
        let search_subscription = cx.subscribe(
            &review_search,
            |this, _, event: &search_input::Changed, cx| {
                if this.review_filters.query != event.0 {
                    this.review_filters.query = event.0.clone();
                    this.review_filters_changed(cx);
                }
            },
        );
        let (tray, tray_error) = match Tray::new(&store.awake()) {
            Ok(tray) => (Some(tray), None),
            Err(err) => (None, Some(format!("could not create tray icon: {err:#}"))),
        };
        Self {
            store,
            repos: None,
            tab: Tab::Reviews,
            review_filters: ReviewFilters::default(),
            review_search,
            repository_picker_open: false,
            review_scroll: ListState::new(0, ListAlignment::Top, px(0.)),
            review_list_items: Vec::new(),
            snooze_picker: None,
            focus_handle: cx.focus_handle(),
            _search_subscription: search_subscription,
            last_checked: None,
            fetch_error: None,
            tray_error,
            save_error: None,
            notification_error: None,
            announced_launch: false,
            tray,
            update: None,
            scan_task: None,
            fetch_task: None,
            poll_task: None,
            wake_task: None,
            update_check: None,
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
        self.notify(woken, cx);
    }

    /// Rediscovers local clones, then checks GitHub again.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        let roots = self.store.roots.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let repos = cx
                .background_executor()
                .spawn(async move { discovery::discover(&roots) })
                .await;
            this.update(cx, |this, cx| {
                this.repos = Some(repos);
                this.scan_task = None;
                // Results fetched against the old repo list are stale.
                this.fetch_task = None;
                this.refresh(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Filtering needs the repo list, and the scan refreshes once it's done.
        if self.repos.is_none() || self.fetch_task.is_some() {
            return;
        }
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { github::fetch_awaiting_reviews() })
                .await;
            this.update(cx, |this, cx| {
                this.fetch_task = None;
                this.last_checked = Some(Local::now());
                match result {
                    Ok(fetched) => {
                        this.fetch_error = None;
                        this.reconcile(fetched, cx);
                    }
                    Err(err) => this.fetch_error = Some(format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
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
        self.show_notification(summary, body.into(), Some(action), cx, move |this, response, cx| {
            match response {
                Response::Action(id) if id == "update" => this.install_update(cx),
                Response::Action(id) if id == "download" => cx.open_url(&url),
                Response::Clicked => show_window(cx),
                _ => {}
            }
        });
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
        self.tray_error = tray
            .update(&self.store.awake(), item)
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

    /// The header has room for one error, so the first set one wins.
    fn displayed_error(&self) -> Option<&str> {
        [
            &self.fetch_error,
            &self.save_error,
            &self.tray_error,
            &self.notification_error,
        ]
        .into_iter()
        .find_map(Option::as_deref)
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
            .id("octowatcher-window")
            .key_context("OctowatcherWindow")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_review_search))
            .on_action(cx.listener(|this, _: &ResetReviewFilters, window, cx| {
                if this.tab == Tab::Reviews {
                    this.reset_review_filters(cx);
                    window.focus(&this.review_search.focus_handle(cx));
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if this.tab == Tab::Reviews
                    && event.keystroke.key == "tab"
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                }
            }))
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
                    .min_h_0()
                    .when(self.tab != Tab::Reviews, |s| s.overflow_y_scroll())
                    .when(self.tab == Tab::Reviews, |s| {
                        s.flex().flex_col().overflow_hidden()
                    })
                    .p_4()
                    .child(content),
            )
    }
}

impl Octowatcher {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status: SharedString = if self.scan_task.is_some() {
            "Scanning folders…".into()
        } else if self.fetch_task.is_some() {
            "Checking GitHub…".into()
        } else if let Some(at) = self.last_checked {
            format!("Checked at {}", at.format("%H:%M")).into()
        } else {
            "".into()
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
                            .child(button("refresh", "Refresh").on_click(cx.listener(
                                |this, _: &ClickEvent, _, cx| this.refresh(cx),
                            ))),
                    ),
            )
            .children(
                self.displayed_error()
                    .map(str::to_owned)
                    .map(|err| {
                        div()
                            .text_xs()
                            .text_color(rgb(theme::RED))
                            .child(err)
                    }),
            )
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
        let (message, action) = match self.update.as_ref()? {
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
                    Some(
                        button("download-update", "Download")
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.open_url(&url))),
                    ),
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

    fn focus_review_search(
        &mut self,
        _: &FocusReviewSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab = Tab::Reviews;
        window.focus(&self.review_search.focus_handle(cx));
        cx.notify();
    }

    fn review_filters_changed(&mut self, cx: &mut Context<Self>) {
        self.review_list_items.clear();
        self.review_scroll.reset(0);
        // A hidden card must not keep an invisible duration picker open.
        self.snooze_picker = None;
        cx.notify();
    }

    fn reset_review_filters(&mut self, cx: &mut Context<Self>) {
        self.review_filters = ReviewFilters::default();
        self.repository_picker_open = false;
        self.review_search.update(cx, |input, cx| input.reset(cx));
        self.review_filters_changed(cx);
    }

    fn reset_review_filters_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_review_filters(cx);
        window.focus(&self.review_search.focus_handle(cx));
    }

    /// Filter controls alone join the focus order; this doesn't add app-wide
    /// keyboard navigation to the existing repository/settings controls.
    fn review_control(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        active: bool,
        pick: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let click_pick = pick.clone();
        div()
            .id(id)
            .focusable()
            .tab_stop(true)
            .px_2()
            .py_1()
            .rounded_md()
            .max_w_full()
            .min_w_0()
            .text_xs()
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if active {
                theme::ACCENT
            } else {
                theme::SURFACE
            }))
            .bg(rgb(if active {
                theme::ACCENT
            } else {
                theme::SURFACE
            }))
            .text_color(rgb(if active { theme::BASE } else { theme::SUBTEXT }))
            .hover(|s| s.border_color(rgb(theme::ACCENT)))
            .focus(|s| s.border_color(rgb(theme::TEXT)))
            .child(div().truncate().child(label.into()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let picker_open = this.repository_picker_open;
                click_pick(this, window, cx);
                if picker_open && !this.repository_picker_open {
                    window.focus(&this.review_search.focus_handle(cx));
                }
            }))
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space")
                        && !event.keystroke.modifiers.modified()
                    {
                        let picker_open = this.repository_picker_open;
                        pick(this, window, cx);
                        if picker_open && !this.repository_picker_open {
                            window.focus(&this.review_search.focus_handle(cx));
                        }
                        cx.stop_propagation();
                    }
                }),
            )
    }

    fn render_review_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let repo_label = format!(
            "Repository: {} ▾",
            self.review_filters.repository.as_deref().unwrap_or("All")
        );
        div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_shrink_0()
            .child(self.review_search.clone())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.review_control(
                        "review-repository",
                        repo_label,
                        self.review_filters.repository.is_some(),
                        |this, _, cx| {
                            this.repository_picker_open = !this.repository_picker_open;
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(self.review_control(
                        "reset-review-filters",
                        "Reset (Esc)",
                        false,
                        Self::reset_review_filters_and_focus,
                        cx,
                    )),
            )
            .when(self.repository_picker_open, |s| {
                s.child(
                    div()
                        .id("review-repository-choices")
                        .max_h(px(120.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(self.review_control(
                            "review-repository-all",
                            "All repositories",
                            self.review_filters.repository.is_none(),
                            |this, _, cx| {
                                this.review_filters.repository = None;
                                this.repository_picker_open = false;
                                this.review_filters_changed(cx);
                            },
                            cx,
                        ))
                        .children(
                            self.review_filters
                                .repositories(&self.store)
                                .into_iter()
                                .map(|repo| {
                                    let active = self
                                        .review_filters
                                        .repository
                                        .as_ref()
                                        .is_some_and(|r| r.eq_ignore_ascii_case(&repo));
                                    self.review_control(
                                        SharedString::from(format!(
                                            "repo-filter:{}",
                                            repo.to_lowercase()
                                        )),
                                        repo.clone(),
                                        active,
                                        move |this, _, cx| {
                                            this.review_filters.repository = Some(repo.clone());
                                            this.repository_picker_open = false;
                                            this.review_filters_changed(cx);
                                        },
                                        cx,
                                    )
                                }),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(62.))
                            .text_xs()
                            .text_color(rgb(theme::SUBTEXT))
                            .child("Status"),
                    )
                    .children(
                        [
                            (DraftFilter::All, "All"),
                            (DraftFilter::Ready, "Ready"),
                            (DraftFilter::Draft, "Draft"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, (value, label))| {
                            self.review_control(
                                ("draft-filter", ix),
                                label,
                                self.review_filters.draft == value,
                                move |this, _, cx| {
                                    this.review_filters.draft = value;
                                    this.review_filters_changed(cx);
                                },
                                cx,
                            )
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(62.))
                            .text_xs()
                            .text_color(rgb(theme::SUBTEXT))
                            .child("Request"),
                    )
                    .children(
                        [
                            (ReviewFilter::All, "All"),
                            (ReviewFilter::First, "First review"),
                            (ReviewFilter::Rereview, "Re-review"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, (value, label))| {
                            self.review_control(
                                ("request-filter", ix),
                                label,
                                self.review_filters.review == value,
                                move |this, _, cx| {
                                    this.review_filters.review = value;
                                    this.review_filters_changed(cx);
                                },
                                cx,
                            )
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(62.))
                            .text_xs()
                            .text_color(rgb(theme::SUBTEXT))
                            .child("Snooze"),
                    )
                    .children(
                        [
                            (SnoozeFilter::All, "All"),
                            (SnoozeFilter::Awake, "Awake"),
                            (SnoozeFilter::Snoozed, "Snoozed"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, (value, label))| {
                            self.review_control(
                                ("snooze-filter", ix),
                                label,
                                self.review_filters.snooze == value,
                                move |this, _, cx| {
                                    this.review_filters.snooze = value;
                                    this.review_filters_changed(cx);
                                },
                                cx,
                            )
                        }),
                    ),
            )
    }

    fn render_reviews(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let indices = self.review_filters.visible_indices(&self.store);
        let items: Vec<_> = indices
            .iter()
            .map(|&ix| {
                let key = self.store.pending[ix].key();
                let picker_open = self.snooze_picker.as_ref() == Some(&key);
                ReviewListItem { key, picker_open }
            })
            .collect();
        if items != self.review_list_items {
            let old_offset = self.review_scroll.logical_scroll_top();
            let old_anchor = self.review_list_items.get(old_offset.item_ix);
            let anchor =
                old_anchor.and_then(|old| items.iter().position(|item| item.key == old.key));
            self.review_scroll.reset(items.len());
            if !items.is_empty() {
                self.review_scroll.scroll_to(ListOffset {
                    item_ix: anchor.unwrap_or(old_offset.item_ix).min(items.len() - 1),
                    offset_in_item: old_offset.offset_in_item,
                });
            }
            self.review_list_items = items;
        }
        let count = indices.len();
        let view = cx.entity();
        div()
            .flex()
            .flex_col()
            .h_full()
            .min_h_0()
            .gap_3()
            .child(self.render_review_filters(cx))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::SUBTEXT))
                    .child(format!(
                        "{count} of {} reviews · filters only affect this list",
                        self.store.pending.len()
                    )),
            )
            .when(count == 0, |s| {
                s.child(
                    div()
                        .id("review-no-results")
                        .debug_selector(|| "review-no-results".into())
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_3()
                        .pt_8()
                        .text_color(rgb(theme::MUTED))
                        .child(if self.store.pending.is_empty() {
                            "Nothing waiting on your review."
                        } else {
                            "No reviews match your search and filters."
                        })
                        .child(
                            self.review_control(
                                "no-results-reset",
                                "Reset search and filters",
                                false,
                                Self::reset_review_filters_and_focus,
                                cx,
                            )
                            .debug_selector(|| "no-results-reset".into()),
                        ),
                )
            })
            .when(count > 0, |s| {
                s.child(
                    list(self.review_scroll.clone(), move |ix, _, cx| {
                        view.update(cx, |this, cx| {
                            let pending_ix = indices[ix];
                            div()
                                .min_h(px(120.))
                                .pb_2()
                                .debug_selector(move || format!("review-{pending_ix}"))
                                .child(this.render_review(pending_ix, cx))
                                .into_any_element()
                        })
                    })
                    .flex_1()
                    .min_h_0(),
                )
            })
    }

    fn render_review(&self, ix: usize, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let pr = &self.store.pending[ix];
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
            .id(SharedString::from(format!(
                "review:{}#{}",
                pr.repo.to_lowercase(),
                pr.number
            )))
            .debug_selector(|| format!("review:{}#{}", pr.repo.to_lowercase(), pr.number))
            .flex()
            .flex_col()
            .min_h(px(112.))
            .when(!picker_open, |s| s.h(px(112.)))
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
                            .min_w_0()
                            .child(div().truncate().child(format!("{}#{}", pr.repo, pr.number)))
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
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(match &snoozed_until {
                        Some(at) => format!("by {} · snoozed until {at}", pr.author),
                        None => format!("by {}", pr.author),
                    }),
            )
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
                    .child(button("add-root", "Add folder…").on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| this.add_root(cx),
                    )))
                    .child(button("rescan", "Rescan").on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| this.rescan(cx),
                    ))),
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
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_repo(&slug, cx)
                    }))
            }));

        div().flex().flex_col().gap_6().child(roots).child(list)
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
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
                    .child(
                        div().flex().child(
                            button("test-notification", "Send test notification").on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.send_test_notification(cx)
                                }),
                            ),
                        ),
                    ),
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

fn display_path(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

fn bind_review_keys(cx: &mut App) {
    SearchInput::bind_keys(cx);
    let search_key = if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    };
    cx.bind_keys([
        KeyBinding::new(search_key, FocusReviewSearch, Some("OctowatcherWindow")),
        KeyBinding::new("escape", ResetReviewFilters, Some("OctowatcherWindow")),
    ]);
}

fn main() {
    let app = Application::new();
    // Clicking the dock icon with the window closed brings it back.
    app.on_reopen(show_window);
    app.run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &Refresh, cx| refresh(cx));
        bind_review_keys(cx);
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
        |window, cx| {
            window.focus(&view.read(cx).focus_handle);
            view
        },
    )
    .unwrap();
}

#[cfg(test)]
mod review_view_tests {
    use super::*;

    /// Render the real view with no tray, GitHub fetch, timers, notifications,
    /// or disk writes. Tests only interact with the session view controls.
    pub(super) fn fixture(cx: &mut Context<Octowatcher>, count: usize) -> Octowatcher {
        let review_search = cx.new(SearchInput::new);
        let subscription = cx.subscribe(
            &review_search,
            |this, _, event: &search_input::Changed, cx| {
                this.review_filters.query = event.0.clone();
                this.review_filters_changed(cx);
            },
        );
        Octowatcher {
            store: Store {
                pending: (1..=count)
                    .rev()
                    .map(|number| PendingReview {
                        repo: "Acme/API".into(),
                        number: number as u64,
                        title: "Café login".into(),
                        author: "Alice".into(),
                        url: format!("https://github.com/acme/api/pull/{number}"),
                        is_draft: number % 2 == 0,
                        rereview: false,
                        requested_at: None,
                    })
                    .collect(),
                ..Default::default()
            },
            repos: None,
            tab: Tab::Reviews,
            review_filters: ReviewFilters::default(),
            review_search,
            repository_picker_open: false,
            review_scroll: ListState::new(0, ListAlignment::Top, px(0.)),
            review_list_items: Vec::new(),
            snooze_picker: None,
            focus_handle: cx.focus_handle(),
            _search_subscription: subscription,
            last_checked: None,
            fetch_error: None,
            tray_error: None,
            save_error: None,
            notification_error: None,
            announced_launch: false,
            tray: None,
            update: None,
            scan_task: None,
            fetch_task: None,
            poll_task: None,
            wake_task: None,
            update_check: None,
            _startup_and_updates: cx.spawn(async |_, _| {}),
        }
    }

    fn find_key() -> &'static str {
        if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-f"
        }
    }
    fn editing_modifier() -> &'static str {
        if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        }
    }

    #[gpui::test]
    fn keyboard_search_filters_paste_and_reset(cx: &mut gpui::TestAppContext) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        view.update(cx, |view, cx| {
            view.tab = Tab::Settings;
            cx.notify();
        });
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("alice");
        view.read_with(cx, |view, _| {
            assert!(view.tab == Tab::Reviews);
            assert_eq!(view.review_filters.query, "alice");
            assert_eq!(view.review_filters.visible_indices(&view.store).len(), 10);
        });
        // Search -> Repository -> Reset -> Status All -> Ready.
        cx.simulate_keystrokes("tab tab tab tab enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.draft, DraftFilter::Ready)
        });
        cx.simulate_keystrokes("tab space");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.draft, DraftFilter::Draft)
        });
        cx.simulate_keystrokes("shift-tab enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.visible_indices(&view.store).len(), 5)
        });
        cx.simulate_keystrokes(find_key());
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("CAFÉ\n#3".into()));
        cx.simulate_keystrokes(&format!(
            "{}-a {}-v",
            editing_modifier(),
            editing_modifier()
        ));
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.query, "CAFÉ #3");
            assert_eq!(view.review_filters.visible_indices(&view.store), vec![7]);
        });
        cx.simulate_keystrokes("escape");
        cx.simulate_input("login");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.query, "login");
            assert_eq!(view.review_filters.draft, DraftFilter::All);
            assert_eq!(view.review_filters.visible_indices(&view.store).len(), 10);
        });
    }

    #[gpui::test]
    fn repository_picker_and_no_results_reset_keep_keyboard_focus(cx: &mut gpui::TestAppContext) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_keystrokes(find_key());
        cx.simulate_keystrokes("tab enter");
        view.read_with(cx, |view, _| assert!(view.repository_picker_open));
        // Repository -> Reset -> All repositories -> Acme/API.
        cx.simulate_keystrokes("tab tab tab enter");
        view.read_with(cx, |view, _| {
            assert!(!view.repository_picker_open);
            assert_eq!(view.review_filters.repository.as_deref(), Some("Acme/API"));
        });
        cx.simulate_input("no-match");
        let reset = cx.debug_bounds("no-results-reset").unwrap();
        cx.simulate_click(reset.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters, ReviewFilters::default())
        });
        cx.simulate_input("#3");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.visible_indices(&view.store), vec![7])
        });
    }

    #[gpui::test]
    fn refresh_tab_changes_and_window_reopening_keep_the_query(cx: &mut gpui::TestAppContext) {
        cx.update(bind_review_keys);
        let (view, visual) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10);
            window.focus(&view.focus_handle);
            view
        });
        visual.simulate_keystrokes(find_key());
        visual.simulate_input("#3");
        view.update(visual, |view, cx| {
            let mut fetched = view.store.pending.clone();
            fetched[7].title = "Updated after refresh".into();
            view.store.reconcile(fetched);
            view.tab = Tab::Repositories;
            cx.notify();
        });
        visual.simulate_keystrokes(find_key());
        view.read_with(visual, |view, _| {
            assert_eq!(view.review_filters.query, "#3")
        });
        visual.update(|window, _| window.remove_window());
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                window.focus(&view.read(cx).focus_handle);
                view.clone()
            })
            .unwrap()
        });
        cx.simulate_keystrokes(*window, find_key());
        cx.simulate_keystrokes(*window, "backspace 4");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.query, "#4");
            assert_eq!(view.review_filters.visible_indices(&view.store), vec![6]);
        });
    }

    #[gpui::test]
    fn filtered_card_snooze_picker_expands_and_disappears_with_the_filter(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("#3");
        let closed = cx.debug_bounds("review:acme/api#3").unwrap().size.height;
        let snooze = cx.debug_bounds("snooze-7").unwrap();
        cx.simulate_click(snooze.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.snooze_picker, Some(("acme/api".into(), 3)))
        });
        assert!(cx.debug_bounds("review:acme/api#3").unwrap().size.height > closed);
        assert!(cx.debug_bounds("snooze-duration-7-120").is_some());
        let cancel = cx.debug_bounds("cancel-snooze-7").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.debug_bounds("review:acme/api#3").unwrap().size.height,
            closed
        );
        cx.simulate_click(snooze.center(), gpui::Modifiers::none());
        cx.simulate_keystrokes(find_key());
        cx.simulate_keystrokes(&format!("{}-a", editing_modifier()));
        cx.simulate_input("no-match");
        view.read_with(cx, |view, _| {
            assert!(view.snooze_picker.is_none());
            assert!(view.review_filters.visible_indices(&view.store).is_empty());
            assert_eq!(view.store.awake().len(), 10);
        });
        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    fn refresh_keeps_the_scrolled_pr_visible_when_new_reviews_arrive(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 100);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        view.update(cx, |view, cx| {
            view.review_scroll.scroll_to(ListOffset {
                item_ix: 10,
                offset_in_item: px(0.),
            });
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            let mut new = view.store.pending[0].clone();
            new.number = 101;
            view.store.pending.insert(0, new);
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_scroll.logical_scroll_top().item_ix, 11);
            assert_eq!(view.store.pending[11].number, 90);
        });
        assert!(cx.debug_bounds("review:acme/api#90").is_some());
    }

    #[gpui::test]
    fn large_queue_can_render_and_search_the_last_review(cx: &mut gpui::TestAppContext) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10_000);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("review:acme/api#10000").is_some());
        assert!(
            cx.debug_bounds("review:acme/api#1").is_none(),
            "offscreen cards should not be rendered"
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_scroll.item_count(), 10_000);
            assert_eq!(
                view.review_scroll.bounds_for_item(0).unwrap().size.height,
                px(120.)
            );
        });
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("#1");
        assert!(cx.debug_bounds("review:acme/api#1").is_some());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.review_filters.visible_indices(&view.store),
                vec![9_999]
            )
        });
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.review_filters.visible_indices(&view.store).len(),
                10_000
            )
        });
        cx.simulate_input("no-such-review");
        assert!(cx.debug_bounds("review-no-results").is_some());
        // Search -> Repository -> Reset; reset restores the list and focus.
        cx.simulate_keystrokes("tab tab enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.query, "");
            assert_eq!(
                view.review_filters.visible_indices(&view.store).len(),
                10_000
            );
        });
        cx.simulate_input("#2");
        view.read_with(cx, |view, _| assert_eq!(view.review_filters.query, "#2"));
    }
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
    fn app_for_picker_test(cx: &mut Context<Octowatcher>) -> Octowatcher {
        let mut app = review_view_tests::fixture(cx, 2);
        app.store.pending = vec![review(1), review(2)];
        app.repos = Some(Vec::new());
        app.announced_launch = true;
        app
    }

    #[gpui::test]
    fn picker_tracks_the_pr_key_and_closes_when_the_review_disappears_or_is_snoozed(
        cx: &mut TestAppContext,
    ) {
        let app = cx.new(app_for_picker_test);
        app.update(cx, |app, _| {
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
        });
    }

    #[gpui::test]
    fn opening_switching_and_canceling_pickers_never_opens_the_pr(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| app_for_picker_test(cx));
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
