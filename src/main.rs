mod discovery;
mod github;
mod keyboard;
#[cfg(test)]
mod keyboard_tests;
mod notifications;
mod store;
mod tray;
mod updater;

use std::{collections::HashSet, path::PathBuf, time::Duration};

use chrono::{DateTime, Local};
use gpui::{
    App, Application, AsyncApp, Bounds, ClickEvent, Context, Entity, FontWeight, Global,
    KeyBinding, KeyDownEvent, PathPromptOptions, PromptButton, PromptLevel, SharedString, Task,
    Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

use discovery::LocalRepo;
use keyboard::{Control, Keyboard};
use notifications::Response;
use store::{PendingReview, Snooze, Store};
use tray::{Tray, UpdateItem};
use updater::Release;

/// Choices offered in Settings for minutes between GitHub checks.
const POLL_CHOICES: [u64; 7] = [1, 2, 5, 10, 15, 30, 60];
/// Choices offered in Settings for minutes a review stays snoozed.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tab {
    Reviews,
    Repositories,
    Settings,
}

struct Octowatcher {
    keyboard: Keyboard,
    #[cfg(test)]
    persist: bool,
    store: Store,
    /// `None` until the first scan of the roots finishes.
    repos: Option<Vec<LocalRepo>>,
    tab: Tab,
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
    /// Matches visual order. Header controls stay available in empty/loading states.
    fn controls(&self) -> Vec<Control> {
        let mut controls = vec![Control::Refresh];
        if matches!(
            self.update,
            Some(Update::Available(_) | Update::Ready(_) | Update::Manual(_))
        ) {
            controls.push(Control::Update);
        }
        controls.extend([Tab::Reviews, Tab::Repositories, Tab::Settings].map(Control::Tab));
        match self.tab {
            Tab::Reviews => {
                for pr in &self.store.pending {
                    controls.extend([Control::Review(pr.key()), Control::Snooze(pr.key())]);
                }
            }
            Tab::Repositories => {
                controls.extend(self.store.roots.iter().cloned().map(Control::RemoveRoot));
                controls.extend([Control::AddRoot, Control::Rescan]);
                controls.extend(
                    self.repos
                        .iter()
                        .flatten()
                        .map(|repo| Control::Repository(repo.slug.to_lowercase())),
                );
            }
            Tab::Settings => {
                controls.extend(POLL_CHOICES.map(Control::Poll));
                controls.extend(SNOOZE_CHOICES.map(Control::SnoozeMinutes));
                controls.push(Control::TestNotification);
            }
        }
        controls
    }

    fn navigate(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        // Leave modified keys to the platform shortcuts and assistive technology.
        if stroke.modifiers.control || stroke.modifiers.platform || stroke.modifiers.alt {
            return;
        }
        if stroke.key == "tab" {
            if stroke.modifiers.shift {
                window.focus_prev();
            } else {
                window.focus_next();
            }
        } else if !stroke.modifiers.shift {
            let Some(current) = self.keyboard.focused(window) else {
                return;
            };
            if let Some(next) = self.keyboard.review_neighbor(&current, &stroke.key) {
                self.keyboard.focus(&next, window);
            } else if let Control::Tab(tab) = current {
                let tabs = [Tab::Reviews, Tab::Repositories, Tab::Settings];
                let index = tabs.iter().position(|item| *item == tab).unwrap();
                let next = match stroke.key.as_str() {
                    "left" => tabs[(index + 2) % 3],
                    "right" => tabs[(index + 1) % 3],
                    _ => return,
                };
                self.tab = next;
                self.keyboard.scroll.set_offset(gpui::point(px(0.), px(0.)));
                self.keyboard.focus(&Control::Tab(next), window);
            } else {
                return;
            }
        } else {
            return;
        }
        self.keyboard.request_reveal();
        cx.stop_propagation();
        cx.notify();
    }

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
        let (tray, tray_error) = match Tray::new(&store.awake()) {
            Ok(tray) => (Some(tray), None),
            Err(err) => (None, Some(format!("could not create tray icon: {err:#}"))),
        };
        Self {
            keyboard: Keyboard::new(cx),
            #[cfg(test)]
            persist: true,
            store,
            repos: None,
            tab: Tab::Reviews,
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

    /// Hides a pending review from the list and the tray for the snooze
    /// length, then notifies about it again.
    fn snooze(&mut self, key: (String, u64), cx: &mut Context<Self>) {
        let Some(pr) = self.store.pending.iter().find(|pr| pr.key() == key) else {
            return;
        };
        let snooze = Snooze {
            repo: key.0.clone(),
            number: key.1,
            until: Local::now().timestamp() + self.store.snooze_minutes as i64 * 60,
            requested_at: pr.requested_at.clone(),
        };
        self.store.snoozed.retain(|s| s.key() != key);
        self.store.snoozed.push(snooze);
        self.snoozes_changed(cx);
    }

    fn unsnooze(&mut self, key: (String, u64), cx: &mut Context<Self>) {
        self.store.snoozed.retain(|s| s.key() != key);
        self.snoozes_changed(cx);
    }

    fn snoozes_changed(&mut self, cx: &mut Context<Self>) {
        self.save();
        self.sync_tray();
        self.schedule_wake(cx);
        cx.notify();
    }

    /// Sets a timer for the earliest snooze to run out.
    fn schedule_wake(&mut self, cx: &mut Context<Self>) {
        let Some(until) = self.store.snoozed.iter().map(|s| s.until).min() else {
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
        self.tray_error = tray
            .update(&self.store.awake(), item)
            .err()
            .map(|err| format!("could not update tray menu: {err:#}"));
    }

    fn save(&mut self) {
        #[cfg(test)]
        if !self.persist {
            return;
        }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.keyboard.reconcile(self.controls(), window, cx);
        cx.defer_in(window, |this, window, _| this.keyboard.reveal(window));
        let content = match self.tab {
            Tab::Reviews => self.render_reviews(cx).into_any_element(),
            Tab::Repositories => self.render_repositories(cx).into_any_element(),
            Tab::Settings => self.render_settings(cx).into_any_element(),
        };
        div()
            .id("octowatcher")
            .track_focus(&self.keyboard.root)
            .tab_group()
            .on_key_down(cx.listener(Self::navigate))
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
                    .overflow_y_scroll()
                    .track_scroll(&self.keyboard.scroll)
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
                            .child(self.button(Control::Refresh, "Refresh").on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx)),
                            )),
                    ),
            )
            .children(
                self.displayed_error()
                    .map(str::to_owned)
                    .map(|err| div().text_xs().text_color(rgb(theme::RED)).child(err)),
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
        let (message, action) =
            match self.update.as_ref()? {
                Update::Available(release) => (
                    format!("Octowatcher {} is available.", release.version),
                    Some(self.button(Control::Update, "Update").on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.install_update(cx)),
                    )),
                ),
                Update::Installing(version) => (format!("Installing Octowatcher {version}…"), None),
                Update::Ready(version) => (
                    format!("Octowatcher {version} is installed."),
                    Some(
                        self.button(Control::Update, "Restart")
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.restart())),
                    ),
                ),
                Update::Manual(release) => {
                    let url = release.url.clone();
                    (
                        format!("Octowatcher {} is available.", release.version),
                        Some(self.button(Control::Update, "Download").on_click(
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
        self.keyboard
            .control(Control::Tab(tab))
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
                this.keyboard.scroll.set_offset(gpui::point(px(0.), px(0.)));
                cx.notify();
            }))
    }

    fn render_reviews(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.store.pending.is_empty() {
            return div()
                .flex()
                .justify_center()
                .pt_16()
                .text_color(rgb(theme::MUTED))
                .child("Nothing waiting on your review.");
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(self.store.pending.iter().map(|pr| {
                let url = pr.url.clone();
                let key = pr.key();
                let snoozed_until = self.store.snooze_for(pr).map(|snooze| {
                    DateTime::from_timestamp(snooze.until, 0)
                        .map(|at| at.with_timezone(&Local).format("%H:%M").to_string())
                        .unwrap_or_default()
                });
                let action = if snoozed_until.is_some() {
                    self.button(Control::Snooze(pr.key()), "Unsnooze")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            // The card behind the button opens the PR.
                            cx.stop_propagation();
                            this.unsnooze(key.clone(), cx);
                        }))
                } else {
                    self.button(Control::Snooze(pr.key()), "Snooze")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.snooze(key.clone(), cx);
                        }))
                };
                let badge = if pr.rereview {
                    Some(("re-review", theme::PEACH))
                } else {
                    None
                };
                self.keyboard
                    .control(Control::Review(pr.key()))
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
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.open_url(&url)))
            }))
    }

    fn render_repositories(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let roots = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Watched folders"))
            .children(self.store.roots.iter().map(|root| {
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
                    .child(
                        self.button(Control::RemoveRoot(root.clone()), "Remove")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.remove_root(&root_for_click, cx)
                            })),
                    )
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        self.button(Control::AddRoot, "Add folder…")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_root(cx))),
                    )
                    .child(
                        self.button(Control::Rescan, "Rescan")
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
            .children(repos.iter().map(|repo| {
                let enabled = self.store.is_enabled(&repo.slug);
                let slug = repo.slug.clone();
                let paths = repo
                    .paths
                    .iter()
                    .map(|p| display_path(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.keyboard
                    .control(Control::Repository(repo.slug.to_lowercase()))
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
            .child(self.render_choices(
                "Check GitHub for review requests every",
                "poll",
                &POLL_CHOICES,
                self.store.poll_minutes,
                Self::set_poll_minutes,
                cx,
            ))
            .child(self.render_choices(
                "Snooze a review for",
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
                            self.button(Control::TestNotification, "Send test notification")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.send_test_notification(cx)
                                })),
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
                        let label = if minutes < 60 {
                            format!("{minutes} min")
                        } else {
                            format!("{} h", minutes / 60)
                        };
                        self.keyboard
                            .control(if id == "poll" {
                                Control::Poll(minutes)
                            } else {
                                Control::SnoozeMinutes(minutes)
                            })
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
                            .child(if active {
                                format!("{label} ✓")
                            } else {
                                label
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                pick(this, minutes, cx)
                            }))
                    })),
            )
    }

    fn button(&self, key: Control, label: &'static str) -> gpui::Stateful<gpui::Div> {
        self.keyboard
            .control(key)
            .px_3()
            .py_1()
            .rounded_md()
            .text_xs()
            .bg(rgb(theme::SURFACE))
            .text_color(rgb(theme::ACCENT))
            .hover(|s| s.bg(rgb(theme::SURFACE_HOVER)))
            .cursor_pointer()
            .child(label)
    }
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

/// Use the host platform's primary shortcut modifier.
fn bind_keys(cx: &mut App) {
    #[cfg(target_os = "macos")]
    let modifier = "cmd";
    #[cfg(not(target_os = "macos"))]
    let modifier = "ctrl";
    cx.bind_keys([
        KeyBinding::new(&format!("{modifier}-q"), Quit, None),
        KeyBinding::new(&format!("{modifier}-r"), Refresh, None),
    ]);
}

fn main() {
    let app = Application::new();
    // Clicking the dock icon with the window closed brings it back.
    app.on_reopen(show_window);
    app.run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &Refresh, cx| refresh(cx));
        bind_keys(cx);
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
            window.focus(&view.read(cx).keyboard.root);
            view
        },
    )
    .unwrap();
}
