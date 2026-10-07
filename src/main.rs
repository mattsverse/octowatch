mod discovery;
mod github;
mod notifications;
mod repository;
mod review_notifications;
mod store;
mod theme;
mod tray;
mod updater;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    time::Duration,
};

use chrono::{DateTime, Local};
use gpui::{
    App, Application, AsyncApp, Bounds, ClickEvent, Context, Entity, FontWeight, Global,
    KeyBinding, PathPromptOptions, PromptButton, PromptLevel, SharedString, Subscription, Task,
    Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

use discovery::LocalRepo;
use notifications::Response;
use repository::{PUBLIC_HOST, RepositoryId};
use review_notifications::{Batch, Delivery, ReviewAction, Target};
use store::{PendingReview, ReviewKey, Store};
use theme::{Appearance, Palette};
use tray::{Tray, UpdateItem};
use updater::Release;

/// Choices offered in Settings for minutes between GitHub checks.
const POLL_CHOICES: [u64; 7] = [1, 2, 5, 10, 15, 30, 60];
/// Choices offered in Settings and for an individual review's snooze.
const SNOOZE_CHOICES: [u64; 6] = [5, 10, 15, 30, 60, 120];
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

actions!(octowatcher, [Quit, Refresh]);

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
    /// The one PR whose duration picker is open; never persisted as a default.
    snooze_picker: Option<ReviewKey>,
    last_checked: Option<DateTime<Local>>,
    /// Each source keeps its own error, so a successful GitHub check doesn't
    /// hide a tray, save, permission or delivery error.
    fetch_error: Option<String>,
    scan_error: Option<String>,
    announced_hosts: BTreeSet<String>,
    tray_error: Option<String>,
    save_error: Option<String>,
    notification_error: Option<String>,
    /// Whether the reviews waiting at launch were announced yet.
    announced_accounts: BTreeSet<u64>,
    account_errors: BTreeMap<String, String>,
    repo_errors: BTreeMap<(String, String), String>,
    access_cache: github::AccessCache,
    /// Whether the first successful poll has validated the cached review list.
    announced_launch: bool,
    /// Retained until the launch batch is accepted, including after failures.
    launch_summary: bool,
    review_delivery: Delivery,
    notification_tasks: HashMap<usize, Task<()>>,
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
    /// Reattached when a closed window is opened again.
    appearance_subscription: Option<Subscription>,
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
        let (tray, tray_error) = match Tray::new(&store.awake(), store.notifications_muted) {
            Ok(tray) => (Some(tray), None),
            Err(err) => (None, Some(format!("could not create tray icon: {err:#}"))),
        };
        Self {
            store,
            repos: None,
            tab: Tab::Reviews,
            snooze_picker: None,
            last_checked: None,
            fetch_error: None,
            scan_error: None,
            announced_hosts: BTreeSet::new(),
            tray_error,
            save_error: None,
            notification_error: None,
            announced_accounts: BTreeSet::new(),
            account_errors: BTreeMap::new(),
            repo_errors: BTreeMap::new(),
            access_cache: github::AccessCache::default(),
            announced_launch: false,
            launch_summary: true,
            review_delivery: Delivery::default(),
            notification_tasks: HashMap::new(),
            tray,
            update: None,
            scan_task: None,
            fetch_task: None,
            poll_task: None,
            wake_task: None,
            update_check: None,
            _startup_and_updates: startup_and_updates,
            appearance_subscription: None,
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

    fn toggle_notifications_muted(&mut self, cx: &mut Context<Self>) {
        self.store.notifications_muted = !self.store.notifications_muted;
        self.save();
        self.sync_tray();
        if !self.store.notifications_muted {
            self.deliver_reviews(cx);
        }
        cx.notify();
    }

    fn toggle_notify_drafts(&mut self, cx: &mut Context<Self>) {
        self.store.notify_drafts = !self.store.notify_drafts;
        self.save();
        self.deliver_reviews(cx);
        cx.notify();
    }

    fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        if self.store.appearance == appearance {
            return;
        }
        self.store.appearance = appearance;
        self.save();
        cx.notify();
    }

    /// Notification actions keep using the global default.
    fn snooze(&mut self, key: ReviewKey, cx: &mut Context<Self>) {
        self.snooze_for_minutes(key, self.store.snooze_minutes, cx);
    }

    fn snooze_for_minutes(&mut self, key: ReviewKey, minutes: u64, cx: &mut Context<Self>) {
        self.snooze_picker = None;
        if self.store.snooze(&key, minutes, Local::now().timestamp()) {
            self.snoozes_changed(cx);
        } else {
            cx.notify();
        }
    }

    fn unsnooze(&mut self, key: ReviewKey, cx: &mut Context<Self>) {
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
        self.store.take_expired(Local::now().timestamp());
        self.snoozes_changed(cx);
        self.deliver_reviews(cx);
    }

    /// Rediscovers local clones, then checks GitHub again.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        let roots = self.store.roots.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let repos = cx
                .background_executor()
                .spawn(async move {
                    github::known_hosts().map(|hosts| discovery::discover(&roots, &hosts))
                })
                .await;
            this.update(cx, |this, cx| {
                this.scan_task = None;
                match repos {
                    Ok(repos) => {
                        this.scan_error = None;
                        this.store.local_repos =
                            repos.iter().map(|repo| repo.id.store_key()).collect();
                        this.repos = Some(repos);
                        this.dismiss_stale_snooze_picker();
                        this.sync_tray();
                        this.force_refresh(cx);
                    }
                    Err(err) => this.scan_error = Some(format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn force_refresh(&mut self, cx: &mut Context<Self>) {
        self.fetch_task = None;
        self.access_cache = github::AccessCache::default();
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Filtering needs the repo list, and the scan refreshes once it's done.
        if self.repos.is_none() || self.fetch_task.is_some() {
            return;
        }
        let watched = self.watched_repositories();
        let repos: Vec<_> = watched
            .iter()
            .filter(|repo| repo.host == PUBLIC_HOST)
            .map(|repo| repo.slug.clone())
            .collect();
        let enterprise_hosts = watched
            .iter()
            .filter(|repo| repo.host != PUBLIC_HOST)
            .map(|repo| repo.host.clone())
            .collect();
        let preferences = self.store.clone();
        let cache = self.access_cache.clone();
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let public = github::poll(&repos, preferences, cache);
                    let enterprise = github::fetch_awaiting_reviews(&enterprise_hosts);
                    (public, enterprise)
                })
                .await;
            this.update(cx, |this, cx| {
                this.fetch_task = None;
                this.last_checked = Some(Local::now());
                let (public, enterprise) = result;
                let (poll, error) = match public {
                    Ok(poll) => (poll, None),
                    Err(err) => (
                        github::Poll {
                            accounts: Vec::new(),
                            checks: Vec::new(),
                            access_cache: github::AccessCache::default(),
                        },
                        Some(format!("{err:#}")),
                    ),
                };
                this.reconcile(poll, enterprise, error, cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Apply each successful account independently. Failed or signed-out
    /// accounts keep their partitioned cache but cannot appear or notify.
    fn reconcile(
        &mut self,
        poll: github::Poll,
        enterprise: github::FetchResults,
        public_error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut errors = enterprise.errors.clone();
        if let Some(error) = &public_error {
            errors.push(error.clone());
        }
        self.fetch_error = (!errors.is_empty()).then(|| errors.join("\n"));
        self.account_errors.clear();
        self.repo_errors.clear();
        self.store.available_accounts.clear();
        self.store.unavailable_repos.clear();
        self.access_cache = poll.access_cache;
        for account in &poll.accounts {
            if let Some(id) = account.account_id {
                self.store.record_account(&account.login, id);
            }
        }
        let found: BTreeSet<_> = poll
            .accounts
            .iter()
            .map(|a| a.login.to_lowercase())
            .collect();
        self.store.known_accounts.extend(found.iter().cloned());
        for account in self
            .store
            .known_accounts
            .difference(&found)
            .filter(|account| self.store.account_enabled(account))
        {
            self.account_errors.insert(
                account.clone(),
                "Not signed in. Run gh auth login --hostname github.com, then Refresh.".into(),
            );
        }
        if self.store.known_accounts.is_empty()
            && public_error.is_none()
            && self
                .watched_repositories()
                .iter()
                .any(|repo| repo.host == PUBLIC_HOST)
        {
            self.fetch_error = Some("No saved github.com accounts. Run gh auth login --hostname github.com, then Refresh.".into());
        }
        let first_hosts: BTreeSet<_> = enterprise
            .successful_hosts
            .difference(&self.announced_hosts)
            .cloned()
            .collect();
        self.store
            .available_hosts
            .extend(enterprise.successful_hosts.iter().cloned());
        self.announced_hosts
            .extend(enterprise.successful_hosts.iter().cloned());
        let mut fetched = enterprise.reviews;
        let mut checked_accounts = BTreeMap::new();
        for check in poll.checks {
            let account = check.account.to_lowercase();
            for (repo, error) in check.access_errors {
                self.repo_errors
                    .insert((account.clone(), repo.to_lowercase()), error);
            }
            match check.reviews {
                Ok(reviews) => {
                    self.store.unavailable_repos.extend(
                        reviews
                            .unavailable_repos
                            .into_iter()
                            .map(|repo| (reviews.account_id, repo)),
                    );
                    checked_accounts.insert(account.clone(), reviews.account_id);
                    self.store.available_accounts.insert(account);
                    fetched.extend(reviews.pending);
                }
                Err(err) => {
                    self.account_errors
                        .insert(account, format!("@{}: {err:#}", check.account));
                }
            }
        }
        let watched = self
            .watched_repositories()
            .iter()
            .map(RepositoryId::store_key)
            .collect();
        self.store.reconcile_scopes(
            fetched,
            &checked_accounts,
            &enterprise.successful_hosts,
            &watched,
        );
        if public_error.is_some() {
            for error in self.account_errors.values_mut() {
                *error = "Account check unavailable; see the error above.".into();
            }
        }
        self.dismiss_stale_snooze_picker();
        let first_accounts: BTreeSet<_> = checked_accounts
            .values()
            .copied()
            .filter(|id| !self.announced_accounts.contains(id))
            .collect();
        self.announced_accounts
            .extend(first_accounts.iter().copied());
        if !checked_accounts.is_empty() || !enterprise.successful_hosts.is_empty() {
            self.announced_launch = true;
        }
        self.store.queue_startup_notifications(&first_accounts);
        self.store.queue_startup_host_notifications(&first_hosts);
        if self.store.notification_queue.is_empty() {
            self.launch_summary = false;
        }
        // Discovery and availability changes matter even when the cached list is unchanged.
        self.wake(cx);
    }

    /// Sends eligible, undelivered review events in one batch. A failed send
    /// stays persisted and is attempted at the next successful GitHub poll.
    fn deliver_reviews(&mut self, cx: &mut Context<Self>) {
        // Validate cached requests against GitHub before any launch delivery.
        if !self.announced_launch {
            return;
        }
        let Some(batch) = self.review_delivery.begin(&self.store) else {
            return;
        };
        self.send_review_batch(batch, cx);
    }

    fn send_review_batch(&mut self, batch: Batch, cx: &mut Context<Self>) {
        let target = Target::for_reviews(&batch.reviews);
        let (summary, body) = if self.launch_summary {
            waiting_text(&batch.reviews)
        } else {
            notification_text(&batch.reviews)
        };
        let action = target.action();
        let started = self.start_notification(
            summary,
            body,
            Some(action),
            cx,
            move |this, delivered, cx| {
                let next = this
                    .review_delivery
                    .complete(&mut this.store, &batch, delivered);
                if delivered {
                    this.launch_summary = false;
                    this.save();
                }
                if let Some(next) = next {
                    this.send_review_batch(next, cx);
                }
                cx.notify();
            },
            move |this, response, cx| match target.respond(response, &this.store) {
                Some(ReviewAction::OpenPr(url)) => cx.open_url(&url),
                Some(ReviewAction::OpenReviews) => {
                    this.tab = Tab::Reviews;
                    show_window(cx);
                }
                Some(ReviewAction::Snooze(pr)) => this.snooze(pr.key(), cx),
                None => {}
            },
        );
        if !started {
            self.review_delivery.deferred();
        }
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

    /// Generic notifications (test/update) use the same bounded sender.
    fn show_notification(
        &mut self,
        summary: String,
        body: String,
        action: Option<notifications::Action>,
        cx: &mut Context<Self>,
        respond: impl FnOnce(&mut Self, Response, &mut Context<Self>) + 'static,
    ) {
        if !self.start_notification(summary, body, action, cx, |_, _, _| {}, respond) {
            self.notification_error =
                Some("Too many active notifications. Try again after dismissing one.".into());
            cx.notify();
        }
    }

    /// Reports OS acceptance before waiting for any action. Fixed slots and
    /// a finite observer lifetime bound tasks and platform response registries.
    fn start_notification(
        &mut self,
        summary: String,
        body: String,
        action: Option<notifications::Action>,
        cx: &mut Context<Self>,
        delivered: impl FnOnce(&mut Self, bool, &mut Context<Self>) + 'static,
        respond: impl FnOnce(&mut Self, Response, &mut Context<Self>) + 'static,
    ) -> bool {
        let Some(slot) =
            notifications::free_slot(|slot| self.notification_tasks.contains_key(&slot))
        else {
            return false;
        };
        let task = cx.spawn(async move |this, cx| {
            let shown = futures_lite::future::or(
                notifications::send(slot, &summary, &body, action),
                async {
                    cx.background_executor()
                        .timer(Duration::from_secs(15))
                        .await;
                    Err("notification service did not answer within 15 seconds".into())
                },
            )
            .await;
            let accepted = shown.is_ok();
            if this
                .update(cx, |this, cx| {
                    this.notification_error = shown
                        .as_ref()
                        .err()
                        .map(|err| format!("could not send notification: {err}"));
                    delivered(this, accepted, cx);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            let response = match shown {
                Ok(handle) => {
                    handle
                        .response(|duration| cx.background_executor().timer(duration))
                        .await
                }
                Err(_) => Response::Dismissed,
            };
            this.update(cx, |this, cx| {
                if accepted {
                    respond(this, response, cx);
                }
                this.notification_tasks.remove(&slot);
                // Successful observers also free bounded sender capacity.
                // A failed send waits for a later poll or explicit resume.
                if accepted {
                    this.deliver_reviews(cx);
                }
                cx.notify();
            })
            .ok();
        });
        self.notification_tasks.insert(slot, task);
        true
    }

    fn watched_repositories(&self) -> BTreeSet<RepositoryId> {
        self.repos
            .iter()
            .flatten()
            .filter(|repo| self.store.is_enabled(&repo.id.store_key()))
            .map(|repo| repo.id.clone())
            .collect()
    }

    fn scope_changed(&mut self, cx: &mut Context<Self>) {
        // Cancel the callback for a check started under previous settings.
        self.fetch_task = None;
        self.dismiss_stale_snooze_picker();
        self.save();
        self.sync_tray();
        self.schedule_wake(cx);
        self.force_refresh(cx);
        cx.notify();
    }

    fn toggle_repo(&mut self, slug: &str, cx: &mut Context<Self>) {
        let key = slug.to_lowercase();
        if !self.store.disabled.remove(&key) {
            self.store.disabled.insert(key);
        }
        self.scope_changed(cx);
    }

    fn toggle_account(&mut self, account: &str, cx: &mut Context<Self>) {
        self.store.toggle_account(account);
        self.store
            .available_accounts
            .remove(&account.to_lowercase());
        self.scope_changed(cx);
    }

    fn toggle_repo_account(&mut self, repo: &str, account: &str, cx: &mut Context<Self>) {
        self.store.toggle_repo_account(repo, account);
        self.scope_changed(cx);
    }

    fn all_repo_accounts(&mut self, repo: &str, cx: &mut Context<Self>) {
        self.store.reset_repo_accounts(repo);
        self.scope_changed(cx);
    }

    fn dismiss_stale_snooze_picker(&mut self) {
        if let Some(key) = &self.snooze_picker
            && !self.store.pending.iter().any(|pr| {
                pr.key() == *key && self.store.visible(pr) && self.store.snooze_for(pr).is_none()
            })
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
        self.tray_error = tray
            .update(&self.store.awake(), item, self.store.notifications_muted)
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
            &self.scan_error,
            &self.save_error,
            &self.tray_error,
            &self.notification_error,
        ]
        .into_iter()
        .find_map(Option::as_deref)
    }
}

fn waiting_text(prs: &[PendingReview]) -> (String, String) {
    let summary = match prs.len() {
        1 => "You have 1 pending review".to_string(),
        n => format!("You have {n} pending reviews"),
    };
    let body = match prs {
        [pr] => format!("{}#{}: {}", pr.request_label(), pr.number, pr.title),
        many => pr_list(many),
    };
    (summary, body)
}

fn notification_text(prs: &[PendingReview]) -> (String, String) {
    match prs {
        [pr] => (
            format!(
                "{} requested your {}",
                pr.author,
                if pr.rereview { "re-review" } else { "review" }
            ),
            format!("{}#{}: {}", pr.request_label(), pr.number, pr.title),
        ),
        many => (
            format!("{} pull requests need your review", many.len()),
            pr_list(many),
        ),
    }
}

fn pr_list(prs: &[PendingReview]) -> String {
    prs.iter()
        .map(|pr| format!("{}#{}", pr.request_label(), pr.number))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Render for Octowatcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.store.appearance.palette(window.appearance());
        let content = match self.tab {
            Tab::Reviews => self.render_reviews(theme, cx).into_any_element(),
            Tab::Repositories => self.render_repositories(theme, cx).into_any_element(),
            Tab::Settings => self.render_settings(theme, cx).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme.background))
            .text_color(rgb(theme.text))
            .text_sm()
            .child(self.render_header(theme, cx))
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
    fn render_header(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
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
            .border_color(rgb(theme.border))
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
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme.muted_text))
                                    .child(status),
                            )
                            .child(button("refresh", "Refresh", theme).on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.force_refresh(cx)),
                            )),
                    ),
            )
            .children(
                self.displayed_error()
                    .map(str::to_owned)
                    .map(|err| div().text_xs().text_color(rgb(theme.error)).child(err)),
            )
            .when(!self.account_errors.is_empty() || !self.repo_errors.is_empty(), |s| s.child(
                div().text_xs().text_color(rgb(theme.error))
                    .child("Some accounts or repositories could not be checked. See Settings and Repositories.")))
            .children(self.render_update(theme, cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.render_tab(
                        Tab::Reviews,
                        format!("Reviews ({})", self.store.visible_pending().len()),
                        theme,
                        cx,
                    ))
                    .child(self.render_tab(
                        Tab::Repositories,
                        format!("Repositories ({repo_count})"),
                        theme,
                        cx,
                    ))
                    .child(self.render_tab(Tab::Settings, "Settings".into(), theme, cx)),
            )
    }

    fn render_update(&self, theme: Palette, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (message, action) =
            match self.update.as_ref()? {
                Update::Available(release) => (
                    format!("Octowatcher {} is available.", release.version),
                    Some(button("install-update", "Update", theme).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.install_update(cx)),
                    )),
                ),
                Update::Installing(version) => (format!("Installing Octowatcher {version}…"), None),
                Update::Ready(version) => (
                    format!("Octowatcher {version} is installed."),
                    Some(
                        button("restart", "Restart", theme)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.restart())),
                    ),
                ),
                Update::Manual(release) => {
                    let url = release.url.clone();
                    (
                        format!("Octowatcher {} is available.", release.version),
                        Some(button("download-update", "Download", theme).on_click(
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
                .bg(rgb(theme.surface))
                .text_xs()
                .child(message)
                .children(action),
        )
    }

    fn render_tab(
        &self,
        tab: Tab,
        label: String,
        theme: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                s.bg(rgb(theme.surface)).text_color(rgb(theme.text))
            })
            .when(!active, |s| {
                s.text_color(rgb(theme.secondary_text))
                    .hover(|s| s.bg(rgb(theme.surface)))
            })
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.tab = tab;
                this.snooze_picker = None;
                cx.notify();
            }))
    }

    fn render_reviews(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let pending = self.store.visible_pending();
        if pending.is_empty() {
            return div()
                .flex()
                .justify_center()
                .pt_16()
                .text_color(rgb(theme.muted_text))
                .child("Nothing waiting on your review.");
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(pending.iter().enumerate().map(|(ix, pr)| {
                let url = pr.url.clone();
                let key = pr.key();
                let picker_open = self.snooze_picker.as_ref() == Some(&key);
                let snoozed_until = self.store.snooze_for(pr).map(|snooze| {
                    DateTime::from_timestamp(snooze.until, 0)
                        .map(|at| at.with_timezone(&Local).format("%H:%M").to_string())
                        .unwrap_or_default()
                });
                let action = if snoozed_until.is_some() {
                    button(("unsnooze", ix), "Unsnooze", theme).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            // The card behind the button opens the PR.
                            cx.stop_propagation();
                            this.unsnooze(key.clone(), cx);
                        },
                    ))
                } else {
                    button(("snooze", ix), "Snooze…", theme)
                        .debug_selector(move || format!("snooze-{ix}"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.snooze_picker = if picker_open { None } else { Some(key.clone()) };
                            cx.notify();
                        }))
                };
                let badge = if pr.rereview {
                    Some(("re-review", theme.warning))
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
                    .bg(rgb(theme.surface))
                    .hover(|s| s.bg(rgb(theme.surface_hover)))
                    .cursor_pointer()
                    .when(snoozed_until.is_some(), |s| {
                        s.text_color(rgb(theme.snoozed_text))
                    })
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
                                    .text_color(rgb(theme.secondary_text))
                                    .child(format!("{}#{}", pr.request_label(), pr.number))
                                    .children(badge.map(|(label, color)| pill(label, color)))
                                    .when(pr.is_draft, |s| {
                                        s.child(pill("draft", theme.muted_text))
                                    }),
                            )
                            .child(action),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(pr.title.clone()),
                    )
                    .child(div().text_xs().text_color(rgb(theme.muted_text)).child(
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
                            theme,
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

    fn render_repositories(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let roots = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Watched folders", theme))
            .children(self.store.roots.iter().enumerate().map(|(ix, root)| {
                let root_for_click = root.clone();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme.surface))
                    .child(display_path(root))
                    .child(
                        button(("remove-root", ix), "Remove", theme).on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                this.remove_root(&root_for_click, cx)
                            },
                        )),
                    )
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("add-root", "Add folder…", theme)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_root(cx))),
                    )
                    .child(
                        button("rescan", "Rescan", theme)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.rescan(cx))),
                    ),
            );

        let repos = self.repos.as_deref().unwrap_or_default();
        let list = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("GitHub repositories found", theme))
            .when(repos.is_empty() && self.scan_task.is_none(), |s| {
                s.child(
                    div()
                        .text_color(rgb(theme.muted_text))
                        .child("No GitHub clones in these folders."),
                )
            })
            .children(repos.iter().enumerate().map(|(ix, repo)| {
                let enabled = self.store.is_enabled(&repo.id.store_key());
                let slug = repo.id.store_key();
                let paths = repo
                    .paths
                    .iter()
                    .map(|p| display_path(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(theme.surface))
                    .child(
                        div()
                            .id(("repo", ix))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .hover(|s| s.bg(rgb(theme.surface_hover)))
                            .cursor_pointer()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .when(!enabled, |s| s.text_color(rgb(theme.muted_text)))
                                    .child(repo.id.to_string())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme.muted_text))
                                            .truncate()
                                            .child(paths),
                                    ),
                            )
                            .child(if enabled {
                                pill("watching", theme.success)
                            } else {
                                pill("off", theme.muted_text)
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.toggle_repo(&slug, cx)
                            })),
                    )
                    .when(repo.id.host == PUBLIC_HOST, |view| {
                        view.child(self.render_repo_accounts(&repo.id.slug, ix, theme, cx))
                    })
                    .children(
                        self.repo_errors
                            .iter()
                            .filter(|((_, slug), _)| slug == &repo.id.store_key())
                            .map(|(_, error)| {
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme.error))
                                    .child(error.clone())
                            }),
                    )
            }));

        div().flex().flex_col().gap_6().child(roots).child(list)
    }

    fn render_repo_accounts(
        &self,
        repo: &str,
        ix: usize,
        theme: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let all = self.store.all_repo_accounts(repo);
        let slug = repo.to_string();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme.secondary_text))
                    .child("Monitor with"),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(("all-accounts", ix), "All enabled accounts", theme)
                            .when(all, |s| {
                                s.bg(rgb(theme.accent)).text_color(rgb(theme.on_accent))
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.all_repo_accounts(&slug, cx)
                            })),
                    )
                    .children(
                        self.store
                            .known_accounts
                            .iter()
                            .enumerate()
                            .map(|(ai, account)| {
                                let enabled = self.store.repo_account_selected(repo, account);
                                let account = account.clone();
                                let slug = repo.to_string();
                                div()
                                    .id(("repo-account", ix * self.store.known_accounts.len() + ai))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .cursor_pointer()
                                    .when(enabled, |s| {
                                        s.bg(rgb(theme.accent)).text_color(rgb(theme.on_accent))
                                    })
                                    .when(!enabled, |s| {
                                        s.bg(rgb(theme.surface_hover))
                                            .text_color(rgb(theme.secondary_text))
                                    })
                                    .child(format!("@{account}"))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.toggle_repo_account(&slug, &account, cx)
                                    }))
                            }),
                    ),
            )
            .when(
                !all && !self
                    .store
                    .known_accounts
                    .iter()
                    .any(|account| self.store.repo_account_selected(repo, account)),
                |s| {
                    s.child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.muted_text))
                            .child("No accounts selected."),
                    )
                },
            )
    }

    fn render_accounts(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().gap_2()
            .child(section_title("GitHub accounts", theme))
            .child(div().text_xs().text_color(rgb(theme.secondary_text))
                .child("Sign in with gh auth login --hostname github.com, then Refresh. New saved accounts are enabled automatically."))
            .when(self.store.known_accounts.is_empty(), |s| s.child(
                div().text_xs().text_color(rgb(theme.muted_text)).child("No saved accounts discovered yet.")))
            .children(self.store.known_accounts.iter().enumerate().map(|(ix, account)| {
                let enabled = self.store.account_enabled(account);
                let login = account.clone();
                let status = if !enabled {
                    "Monitoring off".into()
                } else if let Some(error) = self.account_errors.get(account) {
                    error.clone()
                } else if self.store.available_accounts.contains(account) {
                    "Monitoring github.com".into()
                } else {
                    "Waiting for a successful check".into()
                };
                div().flex().flex_col().gap_1().p_3().rounded_md().bg(rgb(theme.surface))
                    .child(div().flex().items_center().justify_between()
                        .child(format!("@{account}"))
                        .child(button(("account-toggle", ix), if enabled { "Disable" } else { "Enable" }, theme)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_account(&login, cx)))))
                    .child(div().text_xs().text_color(rgb(if enabled && self.account_errors.contains_key(account) { theme.error } else { theme.secondary_text })).child(status))
            }))
    }

    fn render_settings(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.render_accounts(theme, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_title("Appearance", theme))
                    .child(
                        div().flex().gap_2().children(
                            Appearance::CHOICES
                                .into_iter()
                                .enumerate()
                                .map(|(ix, appearance)| {
                                    let active = self.store.appearance == appearance;
                                    choice(("appearance", ix), appearance.label(), active, theme)
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, _, cx| {
                                                this.set_appearance(appearance, cx);
                                            },
                                        ))
                                }),
                        ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.muted_text))
                            .child("System follows your desktop’s light or dark appearance."),
                    ),
            )
            .child(Self::render_choices(
                theme,
                "Check GitHub for review requests every",
                "poll",
                &POLL_CHOICES,
                self.store.poll_minutes,
                Self::set_poll_minutes,
                cx,
            ))
            .child(Self::render_choices(
                theme,
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
                    .child(section_title("Notifications", theme))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                button(
                                    "mute-notifications",
                                    if self.store.notifications_muted {
                                        "Resume review notifications"
                                    } else {
                                        "Mute review notifications"
                                    },
                                    theme,
                                )
                                .debug_selector(|| "mute-notifications".to_string())
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.toggle_notifications_muted(cx)
                                })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme.secondary_text))
                                    .child(if self.store.notifications_muted {
                                        "Muted · polling continues"
                                    } else {
                                        "On"
                                    }),
                            ),
                    )
                    .child(
                        button(
                            "notify-drafts",
                            if self.store.notify_drafts {
                                "Notify about drafts: On"
                            } else {
                                "Notify about drafts: Off"
                            },
                            theme,
                        )
                        .debug_selector(|| "notify-drafts".to_string())
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.toggle_notify_drafts(cx)
                        })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.muted_text))
                            .child("Drafts stay in the queue. Resume sends one catch-up alert for undelivered reviews."),
                    )
                    .child(
                        div().flex().child(
                            button("test-notification", "Send test notification", theme)
                                .debug_selector(|| "test-notification".to_string())
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.send_test_notification(cx)
                                })),
                        ),
                    ),
            )
    }

    /// A row of minute lengths to pick one from.
    fn render_choices(
        theme: Palette,
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
            .child(section_title(title, theme))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(choices.iter().map(|&minutes| {
                        let active = minutes == current;
                        let label = minutes_label(minutes);
                        choice((id, minutes as usize), label, active, theme).on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| pick(this, minutes, cx)),
                        )
                    })),
            )
    }
}

/// Inline choices stay inside the card, but none of their clicks open its URL.
fn snooze_picker<T: 'static>(
    ix: usize,
    default_minutes: u64,
    theme: Palette,
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
        .child(section_title("Snooze for", theme))
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
                    button(("snooze-duration", minutes as usize), label, theme)
                        .debug_selector(move || format!("snooze-duration-{ix}-{minutes}"))
                        .when(is_default, |s| s.border_1().border_color(rgb(theme.accent)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            pick(this, Some(minutes), cx);
                        }))
                }))
                .child(
                    button("cancel-snooze", "Cancel", theme)
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

fn choice(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    theme: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .when(active, |s| {
            s.bg(rgb(theme.accent)).text_color(rgb(theme.on_accent))
        })
        .when(!active, |s| {
            s.bg(rgb(theme.surface))
                .text_color(rgb(theme.secondary_text))
                .hover(|s| s.bg(rgb(theme.surface_hover)))
        })
        .focus(|s| {
            s.border_1()
                .border_color(rgb(if active { theme.on_accent } else { theme.focus }))
        })
        .child(label.into())
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    theme: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgb(theme.surface))
        .text_color(rgb(theme.accent))
        .hover(|s| s.bg(rgb(theme.surface_hover)))
        .cursor_pointer()
        .focus(|s| s.border_1().border_color(rgb(theme.focus)))
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

fn section_title(label: &'static str, theme: Palette) -> gpui::Div {
    div()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(theme.secondary_text))
        .child(label)
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
    view.update(cx, |this, cx| this.force_refresh(cx));
}

/// Mutes or resumes review notifications, from the tray.
pub fn toggle_notifications_muted(cx: &mut App) {
    let view = cx.global::<MainView>().0.clone();
    view.update(cx, |this, cx| this.toggle_notifications_muted(cx));
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
            view.update(cx, |this, cx| {
                this.appearance_subscription =
                    Some(cx.observe_window_appearance(window, |_, _, cx| cx.notify()));
            });
            view
        },
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_notifications_label_receiving_accounts_for_single_and_grouped_requests() {
        let store = Store::from_json(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap();
        let (_, single) = notification_text(&store.pending[..1]);
        assert!(single.contains("@alice · github.com/Owner/Repo#7"));
        let (summary, grouped) = notification_text(&store.pending);
        assert_eq!(summary, "2 pull requests need your review");
        assert!(grouped.contains("@alice · github.com/Owner/Repo#7"));
        assert!(grouped.contains("@bob · github.com/Owner/Repo#7"));
    }
}

#[cfg(test)]
mod snooze_tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    fn review(number: u64) -> PendingReview {
        PendingReview {
            host: repository::default_host(),
            account: "alice".into(),
            account_id: 1,
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
                available_accounts: BTreeSet::from(["alice".into()]),
                local_repos: BTreeSet::from(["owner/repo".into()]),
                ..Store::default()
            },
            repos: Some(Vec::new()),
            tab: Tab::Reviews,
            snooze_picker: None,
            last_checked: None,
            fetch_error: None,
            scan_error: None,
            announced_hosts: BTreeSet::new(),
            tray_error: None,
            save_error: None,
            notification_error: None,
            announced_accounts: BTreeSet::from([1]),
            account_errors: BTreeMap::new(),
            repo_errors: BTreeMap::new(),
            access_cache: github::AccessCache::default(),
            announced_launch: true,
            launch_summary: false,
            review_delivery: Delivery::default(),
            notification_tasks: HashMap::new(),
            tray: None,
            update: None,
            scan_task: None,
            fetch_task: None,
            poll_task: None,
            wake_task: None,
            update_check: None,
            _startup_and_updates: Task::ready(()),
            appearance_subscription: None,
        }
    }

    #[gpui::test]
    fn notification_controls_render_in_both_palettes_and_preference_states(
        cx: &mut TestAppContext,
    ) {
        for appearance in [Appearance::Light, Appearance::Dark] {
            for muted in [false, true] {
                let (_, window) = cx.add_window_view(|_, _| {
                    let mut app = app_for_picker_test();
                    app.tab = Tab::Settings;
                    app.store.appearance = appearance;
                    app.store.notifications_muted = muted;
                    app.store.notify_drafts = !muted;
                    app
                });
                for selector in ["mute-notifications", "notify-drafts", "test-notification"] {
                    let bounds = window
                        .debug_bounds(selector)
                        .expect("notification control rendered");
                    assert!(bounds.size.width > px(0.));
                    assert!(bounds.size.height > px(0.));
                }
            }
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

    #[gpui::test]
    fn picker_keeps_matching_pr_numbers_on_different_hosts_separate(cx: &mut TestAppContext) {
        let public = review(1);
        let enterprise = PendingReview {
            host: "github.example.com".into(),
            url: "https://github.example.com/owner/repo/pull/1".into(),
            ..public.clone()
        };
        let enterprise_key = enterprise.key();
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut app = app_for_picker_test();
            app.store
                .local_repos
                .insert("github.example.com/owner/repo".into());
            app.store.pending = vec![public.clone(), enterprise];
            app
        });
        click(cx, "snooze-0");
        assert_eq!(
            view.read_with(cx, |v, _| v.snooze_picker.clone()),
            Some(public.key())
        );
        click(cx, "snooze-1");
        assert_eq!(
            view.read_with(cx, |v, _| v.snooze_picker.clone()),
            Some(enterprise_key.clone())
        );
        // Snoozing the public counterpart must leave the Enterprise picker open.
        view.update(cx, |v, _| {
            v.store.snooze(&public.key(), 5, 1_000);
            v.dismiss_stale_snooze_picker();
            assert_eq!(v.snooze_picker, Some(enterprise_key));
        });
        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    fn shared_pr_picker_tracks_the_receiving_account_and_closes_when_unavailable(
        cx: &mut TestAppContext,
    ) {
        let mut app = app_for_picker_test();
        let alice = review(1);
        let mut bob = alice.clone();
        bob.account = "bob".into();
        bob.account_id = 2;
        app.store.pending = vec![alice.clone(), bob.clone()];
        app.store.available_accounts.insert("bob".into());
        let (view, cx) = cx.add_window_view(|_, _| app);
        click(cx, "snooze-0");
        assert_eq!(
            view.read_with(cx, |v, _| v.snooze_picker.clone()),
            Some(alice.key())
        );
        click(cx, "snooze-1");
        assert_eq!(
            view.read_with(cx, |v, _| v.snooze_picker.clone()),
            Some(bob.key())
        );
        view.update(cx, |v, cx| {
            v.store.available_accounts.remove("bob");
            v.dismiss_stale_snooze_picker();
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |v, _| {
            assert!(v.snooze_picker.is_none());
            assert_eq!(v.store.pending.len(), 2);
            assert_eq!(v.store.visible_pending(), vec![alice]);
        });
        assert_eq!(cx.opened_url(), None);
    }

    struct PickerHarness {
        store: Store,
        picked: Vec<Option<u64>>,
    }

    impl Render for PickerHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("card")
                .w(px(360.))
                .p_3()
                .text_sm()
                .child(snooze_picker(
                    0,
                    self.store.snooze_minutes,
                    self.store.appearance.palette(window.appearance()),
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
                available_accounts: BTreeSet::from(["alice".into()]),
                local_repos: BTreeSet::from(["owner/repo".into()]),
                snooze_minutes: 15,
                ..Store::default()
            },
            picked: Vec::new(),
        });
        for appearance in [Appearance::Light, Appearance::Dark] {
            view.update(cx, |view, cx| {
                view.store.appearance = appearance;
                view.picked.clear();
                cx.notify();
            });
            cx.run_until_parked();
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
}
