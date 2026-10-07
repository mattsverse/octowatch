mod discovery;
mod github;
mod instance;
mod keyboard;
#[cfg(test)]
mod keyboard_tests;
mod login;
mod notifications;
mod refresh_queue;
mod repository;
mod review_filter;
mod review_notifications;
mod search_input;
mod store;
mod theme;
mod tray;
mod updater;

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    future::Future,
    path::PathBuf,
    time::Duration,
};

use chrono::{DateTime, Local};
use gpui::{
    App, Application, AsyncApp, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable,
    FontWeight, Global, KeyBinding, KeyDownEvent, ListAlignment, ListOffset, ListState,
    PathPromptOptions, PromptButton, PromptLevel, ScrollHandle, SharedString, Subscription, Task,
    Window, WindowBounds, WindowOptions, actions, div, list, point, prelude::*, px, relative, rgb,
    size,
};

use discovery::LocalRepo;
use keyboard::{Control, Keyboard};
use notifications::Response;
use repository::RepositoryId;
use review_filter::{DraftFilter, ReviewFilter, ReviewFilterCache, ReviewFilters, SnoozeFilter};
use review_notifications::{Batch, Delivery, ReviewAction, Target};
use search_input::SearchInput;
use store::{PendingReview, Store};
use theme::{Appearance, Palette};
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

#[derive(PartialEq, Eq)]
struct ReviewListItem {
    key: (RepositoryId, u64),
    picker_open: bool,
}

struct Octowatcher {
    keyboard: Keyboard,
    #[cfg(test)]
    persist: bool,
    store: Store,
    /// `None` until the first scan of the roots finishes.
    repos: Option<Vec<LocalRepo>>,
    tab: Tab,
    review_filters: ReviewFilters,
    review_filter_cache: ReviewFilterCache,
    review_filter_scroll: ScrollHandle,
    review_no_results_scroll: ScrollHandle,
    review_control_focus: RefCell<HashMap<gpui::ElementId, (FocusHandle, Option<usize>)>>,
    rendered_snooze_picker: Option<(RepositoryId, u64)>,
    review_search: Entity<SearchInput>,
    repository_picker_open: bool,
    review_scroll: ListState,
    review_list_items: Vec<ReviewListItem>,
    /// The one PR whose duration picker is open; never persisted as a default.
    snooze_picker: Option<(RepositoryId, u64)>,
    focus_handle: FocusHandle,
    _search_subscription: Subscription,
    #[cfg(test)]
    review_filter_passes: usize,
    last_checked: Option<DateTime<Local>>,
    /// Each source keeps its own error, so a successful GitHub check doesn't
    /// hide a tray, save, permission or delivery error.
    fetch_error: Option<String>,
    scan_issues: Vec<discovery::ScanIssue>,
    scan_error: Option<String>,
    tray_error: Option<String>,
    save_error: Option<String>,
    notification_error: Option<String>,
    login_state: login::State,
    login_error: Option<String>,
    /// Hosts with confirmed requests or at least one complete repository check.
    announced_hosts: BTreeSet<String>,
    /// Retained until the launch batch is accepted, including after failures.
    launch_summary: bool,
    review_delivery: Delivery,
    notification_tasks: HashMap<usize, Task<()>>,
    tray: Option<Tray>,
    update: Option<Update>,
    #[cfg(target_os = "linux")]
    restart_path: Option<PathBuf>,
    scan_task: Option<Task<()>>,
    refresh_queue: refresh_queue::RefreshQueue,
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

struct DiscoveryChanges {
    cache_changed: bool,
    reviews: store::Reconciled,
    start_fetch: bool,
}

impl Octowatcher {
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
                controls.push(Control::Search);
                controls.extend(
                    ["review-repository", "reset-review-filters"]
                        .map(|id| Control::Filter(id.into())),
                );
                if self.repository_picker_open {
                    controls.push(Control::Filter("review-repository-all".into()));
                    controls.extend(
                        self.review_filter_cache
                            .repositories(&self.review_filters)
                            .into_iter()
                            .map(|(repo, _)| {
                                Control::Filter(
                                    SharedString::from(format!("repo-filter:{repo}")).into(),
                                )
                            }),
                    );
                }
                for group in ["draft-filter", "request-filter", "snooze-filter"] {
                    controls.extend((0usize..3).map(|ix| Control::Filter((group, ix).into())));
                }
                let indices = self.review_filter_cache.visible_indices();
                if indices.is_empty() {
                    controls.push(Control::Filter("no-results-reset".into()));
                }
                for &ix in indices.iter() {
                    let pr = &self.store.pending[ix];
                    controls.extend([Control::Review(pr.key()), Control::Snooze(pr.key())]);
                    if self.snooze_picker.as_ref() == Some(&pr.key())
                        && self.store.snooze_for(pr).is_none()
                    {
                        controls.extend(
                            snooze_duration_choices(self.store.snooze_minutes)
                                .map(|minutes| Control::SnoozeDuration(pr.key(), minutes)),
                        );
                        controls.push(Control::CancelSnooze(pr.key()));
                    }
                }
            }
            Tab::Repositories => {
                controls.extend(self.store.roots.iter().cloned().map(Control::RemoveRoot));
                controls.extend([Control::AddRoot, Control::Rescan]);
                controls.extend(
                    self.repos
                        .iter()
                        .flatten()
                        .map(|repo| Control::Repository(repo.id.clone())),
                );
            }
            Tab::Settings => {
                controls.extend(Appearance::CHOICES.map(Control::Appearance));
                if !matches!(self.login_state, login::State::Unavailable(_)) {
                    controls.push(Control::LaunchAtLogin);
                }
                controls.extend(POLL_CHOICES.map(Control::Poll));
                controls.extend(SNOOZE_CHOICES.map(Control::SnoozeMinutes));
                controls.extend([
                    Control::MuteNotifications,
                    Control::NotifyDrafts,
                    Control::TestNotification,
                ]);
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
        if stroke.key == "escape" {
            let Some(key) = self.snooze_picker.take() else {
                return;
            };
            self.keyboard.focus(&Control::Snooze(key), window);
        } else if stroke.key == "tab" {
            let current = self.keyboard.focused(window);
            if let Some(next) = self
                .keyboard
                .tab_neighbor(current.as_ref(), stroke.modifiers.shift)
            {
                self.keyboard.focus(&next, window);
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
                self.snooze_picker = None;
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
        let mut store = Store::load();
        let (login_state, login_error) = match login::status() {
            Ok(state) => {
                if !matches!(state, login::State::Unavailable(_)) {
                    store.launch_at_login = state.requested();
                }
                (state, None)
            }
            Err(err) => (
                login::State::Off,
                Some(format!("could not read launch at login: {err:#}")),
            ),
        };
        let review_delivery = Delivery::for_launch(&store);
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
        let (tray, tray_error) = match Tray::new(&store.awake(), store.notifications_muted) {
            Ok(tray) => (Some(tray), None),
            Err(err) => (None, Some(format!("could not create tray icon: {err:#}"))),
        };
        let keyboard = Keyboard::new(cx);
        let focus_handle = keyboard.root.clone();
        Self {
            keyboard,
            #[cfg(test)]
            persist: true,
            store,
            repos: None,
            tab: Tab::Reviews,
            review_filters: ReviewFilters::default(),
            review_filter_cache: ReviewFilterCache::default(),
            review_filter_scroll: ScrollHandle::default(),
            review_no_results_scroll: ScrollHandle::default(),
            review_control_focus: RefCell::default(),
            rendered_snooze_picker: None,
            review_search,
            repository_picker_open: false,
            review_scroll: ListState::new(0, ListAlignment::Top, px(0.)),
            review_list_items: Vec::new(),
            snooze_picker: None,
            focus_handle,
            _search_subscription: search_subscription,
            #[cfg(test)]
            review_filter_passes: 0,
            last_checked: None,
            fetch_error: None,
            scan_issues: Vec::new(),
            scan_error: None,
            tray_error,
            save_error: None,
            notification_error: None,
            login_state,
            login_error,
            announced_hosts: BTreeSet::new(),
            launch_summary: true,
            review_delivery,
            notification_tasks: HashMap::new(),
            tray,
            update: None,
            #[cfg(target_os = "linux")]
            restart_path: None,
            scan_task: None,
            refresh_queue: refresh_queue::RefreshQueue::default(),
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

    fn refresh_login_status(&mut self, cx: &mut Context<Self>) {
        match login::status() {
            Ok(state) => {
                if !matches!(state, login::State::Unavailable(_))
                    && self.store.launch_at_login != state.requested()
                {
                    self.store.launch_at_login = state.requested();
                    self.save();
                }
                self.login_state = state;
                self.login_error = None;
            }
            Err(err) => self.login_error = Some(format!("could not read launch at login: {err:#}")),
        }
        cx.notify();
    }

    fn toggle_login(&mut self, cx: &mut Context<Self>) {
        // Refresh first in case the OS setting changed while our window was
        // closed. A failed operation never records the requested change as done.
        let changed = login::status().and_then(|state| login::set_enabled(!state.requested()));
        match changed {
            Ok(state) => {
                self.store.launch_at_login = state.requested();
                self.login_state = state;
                self.login_error = None;
                self.save();
            }
            Err(err) => {
                self.login_error = Some(format!("could not change launch at login: {err:#}"))
            }
        }
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
    fn snooze(&mut self, key: (RepositoryId, u64), cx: &mut Context<Self>) {
        self.snooze_for_minutes(key, self.store.snooze_minutes, cx);
    }

    fn snooze_for_minutes(
        &mut self,
        key: (RepositoryId, u64),
        minutes: u64,
        cx: &mut Context<Self>,
    ) {
        if self.snooze_picker.as_ref() == Some(&key) {
            self.snooze_picker = None;
        }
        if self.store.snooze(&key, minutes, Local::now().timestamp()) {
            self.snoozes_changed(cx);
        } else {
            cx.notify();
        }
    }

    fn unsnooze(&mut self, key: (RepositoryId, u64), cx: &mut Context<Self>) {
        self.store.unsnooze(&key);
        self.snoozes_changed(cx);
    }

    fn snoozes_changed(&mut self, cx: &mut Context<Self>) {
        self.review_filter_cache.invalidate_snoozes();
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

    /// Rediscovers local checkouts off the UI thread, then checks GitHub.
    /// Requests received during a scan coalesce into one follow-up snapshot.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        if !self.refresh_queue.request_scan(self.scan_task.is_some()) {
            return;
        }
        let roots = self.store.roots.clone();
        let previous = self.store.discovered.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let scan_roots = roots.clone();
            let scan = cx
                .background_executor()
                .spawn(async move {
                    github::known_hosts().map(|hosts| {
                        let mut scan = discovery::discover(&scan_roots, &hosts);
                        let previous = discovery::within_roots(&previous, &scan_roots);
                        scan.retain_unavailable(&previous);
                        scan
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.scan_task = None;
                if this.refresh_queue.scan_finished(roots != this.store.roots) {
                    this.rescan(cx);
                    return;
                }
                if let Some(changes) = this.apply_discovery(scan) {
                    if changes.reviews.snoozes_changed {
                        this.snoozes_changed(cx);
                    } else if changes.cache_changed
                        || changes.reviews.pending_changed
                        || changes.reviews.notifications_changed
                    {
                        this.save();
                        this.sync_tray();
                    }
                    // A later scan queues a fresh check for its current hosts.
                    // Do not cancel and overlap synchronous gh subprocesses.
                    if changes.start_fetch {
                        this.fetch_reviews(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Apply discovery independently of UI/save/fetch side effects. Failed host
    /// lookup keeps the known snapshot, still respecting explicitly removed roots.
    fn apply_discovery(
        &mut self,
        result: anyhow::Result<discovery::ScanResult>,
    ) -> Option<DiscoveryChanges> {
        let repos = match result {
            Ok(scan) => {
                self.scan_error = None;
                self.scan_issues = scan.issues;
                scan.repos
            }
            Err(err) => {
                self.scan_error = Some(format!("{err:#}"));
                self.scan_issues.clear();
                // Legacy state has no discovery snapshot yet. Unknown local
                // membership must not erase its saved reviews and snoozes.
                if self.repos.is_none() && self.store.discovered.is_empty() {
                    return None;
                }
                discovery::within_roots(&self.store.discovered, &self.store.roots)
            }
        };
        let cache_changed = self.store.discovered != repos;
        self.store.discovered = repos.clone();
        self.repos = Some(repos);
        let reviews = self.store.retain_watched(&self.watched_repositories());
        if reviews.pending_changed {
            self.review_filter_cache.invalidate_reviews();
        }
        if reviews.snoozes_changed {
            self.review_filter_cache.invalidate_snoozes();
        }
        self.dismiss_stale_snooze_picker();
        Some(DiscoveryChanges {
            cache_changed,
            reviews,
            // Even a failed host lookup leaves a known, root-filtered snapshot.
            // Replace any result discarded during scanning and keep cached hosts
            // refreshing while discovery is unavailable.
            start_fetch: self.refresh_queue.request_fetch(self.fetch_task.is_some()),
        })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.rescan(cx);
    }

    fn fetch_reviews(&mut self, cx: &mut Context<Self>) {
        self.refresh_with(
            |watched| async move { github::fetch_awaiting_reviews(&watched) },
            cx,
        );
    }

    fn refresh_with<F, Fut>(&mut self, fetch: F, cx: &mut Context<Self>)
    where
        F: Fn(HashSet<RepositoryId>) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = anyhow::Result<github::FetchedReviews>> + Send + 'static,
    {
        // Scope the API request to enabled local repos after discovery finishes.
        if self.repos.is_none() || !self.refresh_queue.request_fetch(self.fetch_task.is_some()) {
            return;
        }
        let watched = self.watched_repositories();
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(fetch(watched)).await;
            this.update(cx, |this, cx| {
                this.fetch_task = None;
                let fetch_again = this.refresh_queue.fetch_finished(this.scan_task.is_some());
                if fetch_again {
                    // Discard the older result, including errors. This requested
                    // check must run after it, without overlapping subprocesses.
                    this.refresh_with(fetch.clone(), cx);
                    return;
                }
                // A scan in progress may invalidate the filtering list. Its
                // completion starts a fresh fetch; do not announce stale repos.
                if this.scan_task.is_some() {
                    cx.notify();
                    return;
                }
                this.last_checked = Some(Local::now());
                match result {
                    Ok(fetched) => {
                        this.fetch_error =
                            (!fetched.errors.is_empty()).then(|| fetched.errors.join("\n"));
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
    /// enabled local repos. Complete snapshots remove reviewed, withdrawn or
    /// closed requests; incomplete checks preserve uncertain saved state.
    /// Confirmed requests enter the queue and raise notifications even when
    /// another team's membership could not be checked.
    /// The first check after launch announces everything waiting instead.
    fn reconcile(&mut self, fetched: github::FetchedReviews, cx: &mut Context<Self>) {
        let watched = self.watched_repositories();
        let can_announce_launch = !fetched.pending.is_empty()
            || !fetched.completed_repos.is_empty()
            || watched.is_empty();
        let confirmed = fetched.pending.clone();
        let reconciled =
            self.store
                .reconcile_repositories(fetched.pending, &fetched.completed_repos, &watched);
        if reconciled.pending_changed {
            self.review_filter_cache.invalidate_reviews();
        }
        self.dismiss_stale_snooze_picker();
        let notification_sequence = self.store.notification_sequence;
        self.review_delivery.confirm(&mut self.store, &confirmed);
        let first_check = self.announced_hosts.is_empty() && can_announce_launch;
        self.announced_hosts
            .extend(confirmed.iter().map(|pr| pr.repository().host));
        self.announced_hosts
            .extend(fetched.completed_repos.iter().map(|repo| repo.host.clone()));
        if first_check && self.store.notification_queue.is_empty() {
            self.launch_summary = false;
        }
        if reconciled.snoozes_changed {
            self.snoozes_changed(cx);
        } else if reconciled.pending_changed
            || reconciled.notifications_changed
            || self.store.notification_sequence != notification_sequence
        {
            self.save();
            self.sync_tray();
        }
        self.deliver_reviews(cx);
    }

    /// Sends eligible, undelivered review events in one batch. A failed send
    /// stays persisted and is attempted at the next successful GitHub poll.
    fn deliver_reviews(&mut self, cx: &mut Context<Self>) {
        // Require a confirmed check before delivery. Delivery also holds each
        // unchecked saved review until GitHub confirms that request.
        if self.announced_hosts.is_empty() {
            return;
        }
        let Some(batch) = self
            .review_delivery
            .begin(&self.store, &self.announced_hosts)
        else {
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
                let next = this.review_delivery.complete(
                    &mut this.store,
                    &batch,
                    delivered,
                    &this.announced_hosts,
                );
                if delivered {
                    this.launch_summary = false;
                    this.save();
                }
                if let Some(next) = next {
                    this.send_review_batch(next, cx);
                }
                cx.notify();
            },
            move |this, response, cx| this.respond_to_review(&target, response, cx),
        );
        if !started {
            self.review_delivery.deferred();
        }
    }

    fn respond_to_review(&mut self, target: &Target, response: Response, cx: &mut Context<Self>) {
        match target.respond(response, &self.store) {
            Some(ReviewAction::OpenPr(url)) => cx.open_url(&url),
            Some(ReviewAction::OpenReviews) => {
                self.tab = Tab::Reviews;
                // The response runs inside an entity update; opening a new
                // window reads this view and must wait for it to be returned.
                cx.defer(show_window);
            }
            Some(ReviewAction::Snooze(pr)) => self.snooze(pr.key(), cx),
            None => {}
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

    fn watched_repositories(&self) -> HashSet<RepositoryId> {
        self.repos
            .iter()
            .flatten()
            .filter(|repo| self.store.is_enabled(&repo.id))
            .map(|repo| repo.id.clone())
            .collect()
    }

    fn toggle_repo(&mut self, repository: &RepositoryId, cx: &mut Context<Self>) {
        let key = repository.clone();
        if self.store.disabled.remove(&key) {
            self.save();
            self.refresh(cx);
        } else {
            self.store.disabled.insert(key.clone());
            self.store.pending.retain(|pr| pr.repository() != key);
            self.review_filter_cache.invalidate_reviews();
            self.store.prune_notifications();
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
            move |this, response, cx| this.respond_to_update(response, &url, cx),
        );
    }

    fn respond_to_update(&mut self, response: Response, url: &str, cx: &mut Context<Self>) {
        match response {
            Response::Action(id) if id == "update" => self.install_update(cx),
            Response::Action(id) if id == "download" => cx.open_url(url),
            Response::Clicked => cx.defer(show_window),
            _ => {}
        }
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
                        #[cfg(target_os = "linux")]
                        {
                            this.restart_path = Some(path.clone());
                        }
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
            &self.scan_error,
            &self.fetch_error,
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
        [pr] => format!("{}#{}: {}", pr.repo_label(), pr.number, pr.title),
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
            format!("{}#{}: {}", pr.repo_label(), pr.number, pr.title),
        ),
        many => (
            format!("{} pull requests need your review", many.len()),
            pr_list(many),
        ),
    }
}

fn pr_list(prs: &[PendingReview]) -> String {
    prs.iter()
        .map(|pr| format!("{}#{}", pr.repo_label(), pr.number))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Render for Octowatcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_changed = if self.tab == Tab::Reviews {
            let changed = self.prepare_review_list();
            self.keyboard
                .register_search(self.review_search.focus_handle(cx));
            changed
        } else {
            false
        };
        self.keyboard.reconcile(self.controls(), window, cx);
        if list_changed {
            let offset = self.review_scroll.logical_scroll_top();
            self.review_scroll.splice_focusable(
                0..self.review_list_items.len(),
                self.review_list_items
                    .iter()
                    .map(|item| Some(self.keyboard.handle(&Control::Review(item.key.clone())))),
            );
            self.review_scroll.scroll_to(offset);
        }
        cx.defer_in(window, |this, window, cx| this.reveal_keyboard(window, cx));
        let theme = self.store.appearance.palette(window.appearance());
        self.keyboard.set_palette(theme);
        self.review_search
            .update(cx, |input, cx| input.set_palette(theme, cx));
        self.review_control_focus.borrow_mut().clear();
        let content = match self.tab {
            Tab::Reviews => self.render_reviews(theme, cx).into_any_element(),
            Tab::Repositories => self.render_repositories(theme, cx).into_any_element(),
            Tab::Settings => self.render_settings(theme, cx).into_any_element(),
        };
        div()
            .id("octowatcher-window")
            .key_context("OctowatcherWindow")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_review_search))
            .on_action(cx.listener(|this, _: &ResetReviewFilters, window, cx| {
                if let Some(key) = this.snooze_picker.take() {
                    this.keyboard.focus(&Control::Snooze(key), window);
                    this.keyboard.request_reveal();
                    cx.notify();
                } else if this.tab == Tab::Reviews {
                    this.reset_review_filters_and_focus(window, cx);
                }
            }))
            .tab_group()
            .on_key_down(cx.listener(Self::navigate))
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
                    .min_h_0()
                    .when(self.tab != Tab::Reviews, |s| {
                        s.overflow_y_scroll().track_scroll(&self.keyboard.scroll)
                    })
                    .when(self.tab == Tab::Reviews, |s| {
                        s.flex().flex_col().overflow_hidden()
                    })
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
                            .child(self.button(Control::Refresh, "Refresh", theme).on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx)),
                            )),
                    ),
            )
            .children(
                self.displayed_error()
                    .map(str::to_owned)
                    .map(|err| div().text_xs().text_color(rgb(theme.error)).child(err)),
            )
            .when(!self.scan_issues.is_empty(), |s| {
                s.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme.warning))
                        .child("Some folders could not be scanned. See Repositories for details."),
                )
            })
            .children(self.render_update(theme, cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.render_tab(
                        Tab::Reviews,
                        format!("Reviews ({})", self.store.pending.len()),
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
                    Some(self.button(Control::Update, "Update", theme).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.install_update(cx)),
                    )),
                ),
                Update::Installing(version) => (format!("Installing Octowatcher {version}…"), None),
                Update::Ready(version) => (
                    format!("Octowatcher {version} is installed."),
                    Some(
                        self.button(Control::Update, "Restart", theme)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| restart(cx))),
                    ),
                ),
                Update::Manual(release) => {
                    let url = release.url.clone();
                    (
                        format!("Octowatcher {} is available.", release.version),
                        Some(self.button(Control::Update, "Download", theme).on_click(
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
        self.keyboard
            .control(Control::Tab(tab))
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
                this.keyboard.scroll.set_offset(gpui::point(px(0.), px(0.)));
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
        self.review_filter_scroll.set_offset(point(px(0.), px(0.)));
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
        self.review_filter_scroll.set_offset(point(px(0.), px(0.)));
        self.review_search.update(cx, |input, cx| input.reset(cx));
        self.review_filters_changed(cx);
    }

    fn reset_review_filters_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_review_filters(cx);
        window.focus(&self.review_search.focus_handle(cx));
    }

    fn reveal_keyboard(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab != Tab::Reviews {
            self.keyboard.reveal(window);
            return;
        }
        if !self.keyboard.take_reveal_request() {
            return;
        }
        match self.keyboard.focused(window) {
            Some(Control::Search | Control::Filter(_)) => {
                self.scroll_focused_review_control(window, cx);
            }
            Some(
                control @ (Control::Review(_)
                | Control::Snooze(_)
                | Control::SnoozeDuration(_, _)
                | Control::CancelSnooze(_)),
            ) => {
                let key = match &control {
                    Control::Review(key)
                    | Control::Snooze(key)
                    | Control::SnoozeDuration(key, _)
                    | Control::CancelSnooze(key) => key,
                    _ => unreachable!(),
                };
                let Some(ix) = self
                    .review_list_items
                    .iter()
                    .position(|item| &item.key == key)
                else {
                    return;
                };
                let Some(viewport) = self.keyboard.review_viewport.get() else {
                    return;
                };
                if viewport.size.height <= px(0.) {
                    return;
                }
                if self.review_scroll.bounds_for_item(ix).is_none_or(|row| {
                    row.bottom() <= viewport.top() || row.top() >= viewport.bottom()
                }) {
                    self.review_scroll.scroll_to(ListOffset {
                        item_ix: ix,
                        offset_in_item: px(0.),
                    });
                    self.keyboard.request_reveal();
                    window.refresh();
                    return;
                }
                let Some(bounds) = self.keyboard.bounds(&control) else {
                    // Virtual rows join the render tree before their focus can
                    // be activated. Retry the reveal after that row is laid out.
                    self.review_scroll.scroll_to(ListOffset {
                        item_ix: ix,
                        offset_in_item: px(0.),
                    });
                    self.keyboard.request_reveal();
                    window.refresh();
                    return;
                };
                let adjustment =
                    if bounds.top() < viewport.top() || bounds.size.height > viewport.size.height {
                        bounds.top() - viewport.top()
                    } else if bounds.bottom() > viewport.bottom() {
                        bounds.bottom() - viewport.bottom()
                    } else {
                        px(0.)
                    };
                if adjustment != px(0.) {
                    self.review_scroll.scroll_by(adjustment);
                    self.keyboard.request_reveal();
                    window.refresh();
                }
            }
            _ => {}
        }
    }

    fn scroll_focused_review_control(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review_search.focus_handle(cx).is_focused(window) {
            self.review_filter_scroll.set_offset(point(px(0.), px(0.)));
        } else if let Some((_, row)) = self
            .review_control_focus
            .borrow()
            .values()
            .find(|(handle, _)| handle.is_focused(window))
        {
            if let Some(row) = row {
                self.review_filter_scroll.scroll_to_item(*row);
            } else {
                self.review_no_results_scroll.scroll_to_item(1);
            }
        }
        cx.notify();
    }

    /// Filter controls share the app focus order while retaining their
    /// key-down activation. Key-up cannot activate the same choice again.
    fn review_control(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        active: bool,
        scroll_row: Option<usize>,
        pick: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = self.review_search.read(cx).palette();
        let click_pick = pick.clone();
        let id = id.into();
        self.review_control_focus.borrow_mut().insert(
            id.clone(),
            (
                self.keyboard.handle(&Control::Filter(id.clone())),
                scroll_row,
            ),
        );
        let label = label.into();
        let selector = format!("review-filter-{label}");
        self.keyboard
            .control(Control::Filter(id))
            .debug_selector(move || selector)
            .px_2()
            .py_1()
            .rounded_md()
            .max_w_full()
            .min_w_0()
            .text_xs()
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if active { theme.accent } else { theme.border }))
            .bg(rgb(if active { theme.accent } else { theme.surface }))
            .text_color(rgb(if active {
                theme.on_accent
            } else {
                theme.secondary_text
            }))
            .hover(|s| s.border_color(rgb(theme.accent)))
            .focus(|s| s.border_color(rgb(if active { theme.on_accent } else { theme.focus })))
            .capture_key_up(|event, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && !event.keystroke.modifiers.modified()
                {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            })
            .child(div().truncate().child(label))
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
        let theme = self.review_search.read(cx).palette();
        let repo_label = format!(
            "Repository: {} ▾",
            self.review_filters
                .repository
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "All".into())
        );
        let repositories = if self.repository_picker_open {
            self.review_filter_cache.repositories(&self.review_filters)
        } else {
            Vec::new()
        };
        let draft_row = 2 + if self.repository_picker_open {
            1 + repositories.len()
        } else {
            0
        };
        div()
            .id("review-filter-controls")
            .max_h(relative(0.5))
            .overflow_y_scroll()
            .track_scroll(&self.review_filter_scroll)
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
                        Some(1),
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
                        Some(1),
                        Self::reset_review_filters_and_focus,
                        cx,
                    )),
            )
            .when(self.repository_picker_open, |s| {
                s.child(self.review_control(
                    "review-repository-all",
                    "All repositories",
                    self.review_filters.repository.is_none(),
                    Some(2),
                    |this, window, cx| {
                        this.review_filters.repository = None;
                        this.repository_picker_open = false;
                        this.review_filters_changed(cx);
                        this.focus_review_search(&FocusReviewSearch, window, cx);
                    },
                    cx,
                ))
                .children(
                    repositories
                        .into_iter()
                        .enumerate()
                        .map(|(ix, (repo, label))| {
                            let active = self
                                .review_filters
                                .repository
                                .as_ref()
                                .is_some_and(|r| r == &repo);
                            self.review_control(
                                SharedString::from(format!("repo-filter:{repo}")),
                                label,
                                active,
                                Some(3 + ix),
                                move |this, window, cx| {
                                    this.review_filters.repository = Some(repo.clone());
                                    this.repository_picker_open = false;
                                    this.review_filters_changed(cx);
                                    this.focus_review_search(&FocusReviewSearch, window, cx);
                                },
                                cx,
                            )
                        }),
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
                            .text_color(rgb(theme.secondary_text))
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
                                Some(draft_row),
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
                            .text_color(rgb(theme.secondary_text))
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
                                Some(draft_row + 1),
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
                            .text_color(rgb(theme.secondary_text))
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
                                Some(draft_row + 2),
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

    fn prepare_review_list(&mut self) -> bool {
        let mut changed = false;
        let recomputed = self
            .review_filter_cache
            .refresh(&self.store, &self.review_filters);
        #[cfg(test)]
        if recomputed {
            self.review_filter_passes += 1;
        }
        let indices = self.review_filter_cache.visible_indices();
        // Picker changes only require row remeasurement; unrelated repaints
        // reuse both matching results and the stable PR/scroll anchors.
        if recomputed
            || self.rendered_snooze_picker != self.snooze_picker
            || self.review_list_items.len() != indices.len()
        {
            let items: Vec<_> = indices
                .iter()
                .map(|&ix| {
                    let key = self.store.pending[ix].key();
                    let picker_open = self.snooze_picker.as_ref() == Some(&key);
                    ReviewListItem { key, picker_open }
                })
                .collect();
            if items != self.review_list_items {
                changed = true;
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
            self.rendered_snooze_picker = self.snooze_picker.clone();
        }
        changed
    }

    fn render_reviews(&mut self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let indices = self.review_filter_cache.visible_indices();
        let count = indices.len();
        let view = cx.entity();
        let viewport = self.keyboard.review_viewport.clone();
        div()
            .flex()
            .flex_col()
            .h_full()
            .min_h_0()
            .gap_3()
            .child(self.render_review_filters(cx))
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(rgb(theme.secondary_text))
                    .child(format!(
                        "{count} of {} reviews · filters only affect this list",
                        self.store.pending.len()
                    )),
            )
            .when(count == 0, |s| {
                s.child(
                    div()
                        .id("review-no-results")
                        .track_scroll(&self.review_no_results_scroll)
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .debug_selector(|| "review-no-results".into())
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_3()
                        .pt_8()
                        .text_color(rgb(theme.muted_text))
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
                                None,
                                Self::reset_review_filters_and_focus,
                                cx,
                            )
                            .debug_selector(|| "no-results-reset".into()),
                        ),
                )
            })
            .when(count > 0, |s| {
                s.child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(
                            gpui::canvas(
                                move |bounds, _, _| viewport.set(Some(bounds)),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(
                            list(self.review_scroll.clone(), move |ix, _, cx| {
                                view.update(cx, |this, cx| {
                                    let pending_ix = indices[ix];
                                    div()
                                        .min_h(px(120.))
                                        .pb_2()
                                        .debug_selector(move || format!("review-{pending_ix}"))
                                        .child(this.render_review(pending_ix, theme, cx))
                                        .into_any_element()
                                })
                            })
                            .flex_1()
                            .min_h_0(),
                        ),
                )
            })
    }

    fn render_review(
        &self,
        ix: usize,
        theme: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
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
            self.button(Control::Snooze(pr.key()), "Unsnooze", theme)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    // The card behind the button opens the PR.
                    cx.stop_propagation();
                    this.unsnooze(key.clone(), cx);
                }))
        } else {
            self.button(Control::Snooze(pr.key()), "Snooze…", theme)
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
        self.keyboard
            .control(Control::Review(pr.key()))
            .debug_selector(|| format!("review:{}#{}", pr.repository(), pr.number))
            .flex()
            .flex_col()
            .min_h(px(112.))
            .when(!picker_open, |s| s.h(px(112.)))
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
                            .min_w_0()
                            .child(div().truncate().child(format!(
                                "{}#{}",
                                pr.repo_label(),
                                pr.number
                            )))
                            .children(badge.map(|(label, color)| pill(label, color)))
                            .when(pr.is_draft, |s| s.child(pill("draft", theme.muted_text))),
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
                    .text_color(rgb(theme.muted_text))
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
                    &self.keyboard,
                    key.clone(),
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
    }

    fn render_repositories(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let roots = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Watched folders", theme))
            .children(self.store.roots.iter().map(|root| {
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
                        self.button(Control::RemoveRoot(root.clone()), "Remove", theme)
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
                        self.button(Control::AddRoot, "Add folder…", theme)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_root(cx))),
                    )
                    .child(
                        self.button(Control::Rescan, "Rescan", theme)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.rescan(cx))),
                    ),
            );

        let roots = roots.children(self.scan_issues.iter().map(|issue| {
            div().text_xs().text_color(rgb(theme.warning)).child(format!(
                "{}: {}. Previously found repositories are retained; retrying on the next check.",
                display_path(&issue.path),
                issue.message
            ))
        }));

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
            .children(repos.iter().map(|repo| {
                let enabled = self.store.is_enabled(&repo.id);
                let id = repo.id.clone();
                let paths = repo
                    .paths
                    .iter()
                    .map(|p| display_path(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.keyboard
                    .control(Control::Repository(repo.id.clone()))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(theme.surface))
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
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_repo(&id, cx)),
                    )
            }));

        div().flex().flex_col().gap_6().child(roots).child(list)
    }

    fn render_settings(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_title("Appearance", theme))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .children(Appearance::CHOICES.into_iter().map(|appearance| {
                                let active = self.store.appearance == appearance;
                                choice(
                                    self.keyboard.control(Control::Appearance(appearance)),
                                    appearance.label(),
                                    active,
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _: &ClickEvent, _, cx| {
                                        this.set_appearance(appearance, cx);
                                    },
                                ))
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.muted_text))
                            .child("System follows your desktop’s light or dark appearance."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_title("Launch at login", theme))
                    .child(match &self.login_state {
                        login::State::Unavailable(reason) => div()
                            .text_xs()
                            .text_color(rgb(theme.muted_text))
                            .child(reason.clone())
                            .into_any_element(),
                        state => {
                            div()
                                .flex()
                                .child(
                                    self.button(
                                        Control::LaunchAtLogin,
                                        match state {
                                            login::State::On => "On",
                                            login::State::NeedsApproval => "Awaiting approval",
                                            login::State::NeedsRepair => "Repair",
                                            _ => "Off",
                                        },
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, _, cx| this.toggle_login(cx),
                                    )),
                                )
                                .into_any_element()
                        }
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.secondary_text))
                            .child("Start quietly in the tray when you sign in."),
                    )
                    .when(self.login_state == login::State::NeedsApproval, |row| {
                        row.child(
                            div().text_xs().text_color(rgb(theme.warning)).child(
                                "Allow Octowatcher in System Settings → General → Login Items.",
                            ),
                        )
                    })
                    .when(self.login_state == login::State::NeedsRepair, |row| {
                        row.child(
                            div().text_xs().text_color(rgb(theme.warning)).child(
                                "The startup command has changed. Repair it to start this copy at login.",
                            ),
                        )
                    })
                    .when_some(self.login_error.clone(), |row, error| {
                        row.child(div().text_xs().text_color(rgb(theme.error)).child(error))
                    }),
            )
            .child(self.render_choices(
                theme,
                ("Check GitHub for review requests every", Control::Poll),
                &POLL_CHOICES,
                self.store.poll_minutes,
                Self::set_poll_minutes,
                cx,
            ))
            .child(self.render_choices(
                theme,
                ("Default snooze length", Control::SnoozeMinutes),
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
                                self.button(
                                    Control::MuteNotifications,
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
                        self.button(
                            Control::NotifyDrafts,
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
                            self.button(Control::TestNotification, "Send test notification", theme)
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
        &self,
        theme: Palette,
        (title, control): (&'static str, fn(u64) -> Control),
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
                        choice(
                            self.keyboard.control(control(minutes)),
                            label,
                            active,
                            theme,
                        )
                        .on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| pick(this, minutes, cx)),
                        )
                    })),
            )
    }

    fn button(
        &self,
        key: Control,
        label: impl Into<SharedString>,
        theme: Palette,
    ) -> gpui::Stateful<gpui::Div> {
        button(self.keyboard.control(key), label, theme)
    }
}

/// Inline choices stay inside the card, but none of their clicks open its URL.
fn snooze_picker<T: 'static>(
    ix: usize,
    default_minutes: u64,
    keyboard: &Keyboard,
    key: (RepositoryId, u64),
    theme: Palette,
    cx: &mut Context<T>,
    pick: impl Fn(&mut T, Option<u64>, &mut Context<T>) + Clone + 'static,
) -> gpui::Stateful<gpui::Div> {
    // Keep a default saved by an older build available even if it isn't a preset.
    let choices = snooze_duration_choices(default_minutes);
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
                    button(
                        keyboard.control(Control::SnoozeDuration(key.clone(), minutes)),
                        label,
                        theme,
                    )
                    .debug_selector(move || format!("snooze-duration-{ix}-{minutes}"))
                    .when(is_default, |s| s.border_1().border_color(rgb(theme.accent)))
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            pick(this, Some(minutes), cx);
                        },
                    ))
                }))
                .child(
                    button(
                        keyboard.control(Control::CancelSnooze(key.clone())),
                        "Cancel",
                        theme,
                    )
                    .debug_selector(move || format!("cancel-snooze-{ix}"))
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            pick(this, None, cx);
                        },
                    )),
                ),
        )
        .on_click(|_: &ClickEvent, _, cx| cx.stop_propagation())
}

fn snooze_duration_choices(default_minutes: u64) -> impl Iterator<Item = u64> {
    SNOOZE_CHOICES
        .into_iter()
        .chain((!SNOOZE_CHOICES.contains(&default_minutes)).then_some(default_minutes))
}

fn minutes_label(minutes: u64) -> String {
    if minutes < 60 || !minutes.is_multiple_of(60) {
        format!("{minutes} min")
    } else {
        format!("{} h", minutes / 60)
    }
}

fn choice(
    element: gpui::Stateful<gpui::Div>,
    label: impl Into<SharedString>,
    active: bool,
    theme: Palette,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    element
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
            let s =
                s.border_1()
                    .border_color(rgb(if active { theme.on_accent } else { theme.focus }));
            if active {
                s.bg(rgb(theme.accent)).text_color(rgb(theme.on_accent))
            } else {
                s
            }
        })
        .child(if active {
            SharedString::from(format!("{label} ✓"))
        } else {
            label
        })
}

fn button(
    element: gpui::Stateful<gpui::Div>,
    label: impl Into<SharedString>,
    theme: Palette,
) -> gpui::Stateful<gpui::Div> {
    element
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
    let instance = match instance::Instance::acquire(login::is_background_launch()) {
        Ok(Some(instance)) => instance,
        Ok(None) => return,
        Err(err) => {
            eprintln!("could not start Octowatcher: {err:#}");
            std::process::exit(1);
        }
    };
    let reopen = instance.reopen.clone();
    let app = Application::new();
    // Clicking the dock icon with the window closed brings it back.
    app.on_reopen(show_window);
    app.run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &Refresh, cx| refresh(cx));
        bind_review_keys(cx);
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
        let show = login::should_show_window(
            login::is_background_launch(),
            octowatcher.read(cx).tray.is_some(),
        );
        cx.set_global(MainView(octowatcher));
        cx.spawn(async move |cx| {
            while reopen.recv().await.is_ok() {
                if cx.update(show_window).is_err() {
                    break;
                }
            }
        })
        .detach();
        if show {
            show_window(cx);
        }
    });
    // Keep the descriptor and IPC worker alive through the entire app loop,
    // including window closure and the updater's wait-for-exit restart handoff.
    drop(instance);
}

struct MainView(Entity<Octowatcher>);

impl Global for MainView {}

/// Checks GitHub for review requests now, from the tray or the app menu.
pub fn refresh(cx: &mut App) {
    let view = cx.global::<MainView>().0.clone();
    view.update(cx, |this, cx| this.refresh(cx));
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
        cx.update(restart).ok();
    }
}

/// Restarts after the current process exits, retaining instance ownership until
/// then. macOS keeps GPUI's native bundle relaunch; Linux quotes the target path.
pub fn restart(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.restart();
    #[cfg(target_os = "linux")]
    {
        let view = cx.global::<MainView>().0.clone();
        let path = view
            .read(cx)
            .restart_path
            .clone()
            .map(Ok)
            .unwrap_or_else(updater::running_executable);
        match path.and_then(|path| updater::relaunch(&path)) {
            Ok(()) => cx.quit(),
            Err(err) => eprintln!("could not restart Octowatcher: {err:#}"),
        }
    }
}

/// Brings the window to the front, opening it again if it was closed.
pub fn show_window(cx: &mut App) {
    let view = cx.global::<MainView>().0.clone();
    view.update(cx, |this, cx| this.refresh_login_status(cx));
    cx.activate(true);
    let existing = cx.windows().first().copied();
    #[cfg(not(target_os = "linux"))]
    if let Some(window) = existing {
        window
            .update(cx, |_, window, _| window.activate_window())
            .ok();
        return;
    }
    let bounds = Bounds::centered(None, size(px(560.), px(680.)), cx);
    let window_bounds = WindowBounds::Windowed(bounds);
    // GPUI has no unminimize API on Linux, and activation alone can leave a
    // minimized surface hidden. Replace the native window, preserving the
    // shared view/state and bounds. Open first: zero windows stops GPUI Linux.
    #[cfg(target_os = "linux")]
    let window_bounds = existing
        .and_then(|window| {
            window
                .update(cx, |_, window, _| window.window_bounds())
                .ok()
        })
        .unwrap_or(window_bounds);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(window_bounds),
            app_id: Some("octowatcher".into()),
            ..Default::default()
        },
        |window, cx| {
            // GPUI 0.2.2 stops both Linux backends when their last window is
            // destroyed. Minimize on Close to keep the tray and polling alive.
            #[cfg(target_os = "linux")]
            window.on_window_should_close(cx, |window, _| {
                window.minimize_window();
                false
            });
            window.focus(&view.read(cx).focus_handle);
            view.update(cx, |this, cx| {
                this.appearance_subscription =
                    Some(cx.observe_window_appearance(window, |_, _, cx| cx.notify()));
            });
            view
        },
    )
    .unwrap();
    #[cfg(target_os = "linux")]
    if let Some(window) = existing {
        window
            .update(cx, |_, window, _| window.remove_window())
            .ok();
    }
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
        let keyboard = Keyboard::new(cx);
        let focus_handle = keyboard.root.clone();
        Octowatcher {
            keyboard,
            persist: false,
            store: Store {
                pending: (1..=count)
                    .rev()
                    .map(|number| PendingReview {
                        host: repository::default_host(),
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
            review_filter_cache: ReviewFilterCache::default(),
            review_filter_scroll: ScrollHandle::default(),
            review_no_results_scroll: ScrollHandle::default(),
            review_control_focus: RefCell::default(),
            rendered_snooze_picker: None,
            review_search,
            repository_picker_open: false,
            review_scroll: ListState::new(0, ListAlignment::Top, px(0.)),
            review_list_items: Vec::new(),
            snooze_picker: None,
            focus_handle,
            _search_subscription: subscription,
            review_filter_passes: 0,
            last_checked: None,
            fetch_error: None,
            scan_issues: Vec::new(),
            scan_error: None,
            tray_error: None,
            save_error: None,
            notification_error: None,
            login_state: login::State::Unavailable("Test build".into()),
            login_error: None,
            announced_hosts: BTreeSet::new(),
            launch_summary: false,
            review_delivery: Delivery::default(),
            notification_tasks: HashMap::new(),
            tray: None,
            update: None,
            #[cfg(target_os = "linux")]
            restart_path: None,
            scan_task: None,
            refresh_queue: refresh_queue::RefreshQueue::default(),
            fetch_task: None,
            poll_task: None,
            wake_task: None,
            update_check: None,
            _startup_and_updates: cx.spawn(async |_, _| {}),
            appearance_subscription: None,
        }
    }

    #[gpui::test]
    fn discovery_pruning_updates_filtered_rows_without_resetting_view_choices(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, visual) = cx.add_window_view(|_, cx| {
            let mut app = fixture(cx, 10);
            app.store.snooze(&app.store.pending[7].key(), 120, 0);
            app.review_filters.query = "#3".into();
            app.review_filters.repository = Some(app.store.pending[7].repository());
            app.review_filters.snooze = SnoozeFilter::Snoozed;
            app
        });
        visual.simulate_resize(size(px(560.), px(680.)));
        view.read_with(visual, |app, _| {
            assert_eq!(app.review_scroll.item_count(), 1);
            assert!(app.review_filter_passes > 0);
        });
        let filters = view.read_with(visual, |app, _| app.review_filters.clone());
        view.update(visual, |app, cx| {
            let changes = app
                .apply_discovery(Ok(discovery::ScanResult::default()))
                .unwrap();
            assert!(changes.reviews.pending_changed && changes.reviews.snoozes_changed);
            cx.notify();
        });
        visual.run_until_parked();
        view.read_with(visual, |app, _| {
            assert_eq!(app.review_scroll.item_count(), 0);
            assert!(app.review_filter_cache.visible_indices().is_empty());
            assert_eq!(app.review_filters, filters);
            assert!(app.store.pending.is_empty());
            assert!(app.store.snoozed.is_empty());
        });
    }

    #[gpui::test]
    fn repository_picker_distinguishes_same_pr_number_on_different_hosts(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = fixture(cx, 2);
            let public = PendingReview {
                number: 1,
                is_draft: false,
                ..view.store.pending[0].clone()
            };
            let enterprise = PendingReview {
                host: "ghe.example.com".into(),
                url: "https://ghe.example.com/Acme/API/pull/1".into(),
                ..public.clone()
            };
            view.store.pending = vec![public, enterprise];
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("#1");
        view.read_with(cx, |view, _| assert_eq!(view.review_scroll.item_count(), 2));
        assert!(cx.debug_bounds("review:github.com/acme/api#1").is_some());
        assert!(
            cx.debug_bounds("review:ghe.example.com/acme/api#1")
                .is_some()
        );
        cx.simulate_keystrokes("tab enter");
        let choice = cx
            .debug_bounds("review-filter-ghe.example.com/Acme/API")
            .unwrap();
        cx.simulate_click(choice.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.review_filters.repository,
                Some(RepositoryId::new("ghe.example.com", "acme/api"))
            );
            assert_eq!(view.review_scroll.item_count(), 1);
        });
        let snooze = cx.debug_bounds("snooze-1").unwrap();
        cx.simulate_click(snooze.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.snooze_picker,
                Some((RepositoryId::new("ghe.example.com", "acme/api"), 1))
            );
        });
        // Snoozing the counterpart on github.com must not hide or close
        // the Enterprise review's picker. No disk/notification effects.
        view.update(cx, |view, cx| {
            let public = view.store.pending[0].key();
            view.store.snooze(&public, 5, 0);
            view.review_filter_cache.invalidate_snoozes();
            view.dismiss_stale_snooze_picker();
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.snooze_picker.is_some());
            assert_eq!(view.review_scroll.item_count(), 1);
            assert_eq!(view.store.awake().len(), 1);
        });
        view.update(cx, |view, cx| {
            view.review_filters.snooze = SnoozeFilter::Snoozed;
            view.review_filters_changed(cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.review_scroll.item_count(), 0));
        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    fn summary_notification_responses_reopen_closed_reviews_preserving_filters(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, visual) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10);
            window.focus(&view.focus_handle);
            view
        });
        visual.simulate_keystrokes(find_key());
        visual.simulate_input("#1");
        cx.update(|cx| cx.set_global(MainView(view.clone())));
        view.update(cx, |view, cx| {
            view.review_filters.repository = Some(RepositoryId::new("github.com", "Acme/API"));
            view.review_filters.draft = DraftFilter::Ready;
            view.review_filters.review = ReviewFilter::First;
            view.review_filters.snooze = SnoozeFilter::Awake;
            view.review_filters_changed(cx);
        });
        for response in [Response::Clicked, Response::Action("open-reviews".into())] {
            // Use the same entity-update boundary as start_notification's
            // response callback, without sending any native notification.
            view.update(cx, |view, _| view.tab = Tab::Settings);
            cx.update(|cx| {
                let window = cx.windows()[0];
                window
                    .update(cx, |_, window, _| window.remove_window())
                    .unwrap();
            });
            view.update(cx, |view, cx| {
                view.respond_to_review(&Target::Summary, response, cx);
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|cx| assert_eq!(cx.windows().len(), 1));
            view.read_with(cx, |view, _| {
                assert!(view.tab == Tab::Reviews);
                assert_eq!(
                    view.review_filters,
                    ReviewFilters {
                        query: "#1".into(),
                        repository: Some(RepositoryId::new("github.com", "Acme/API")),
                        draft: DraftFilter::Ready,
                        review: ReviewFilter::First,
                        snooze: SnoozeFilter::Awake,
                    }
                );
                assert_eq!(view.review_scroll.item_count(), 1);
                assert!(view.appearance_subscription.is_some());
            });
        }
    }

    #[gpui::test]
    fn update_notification_click_reopens_closed_window_preserving_the_tab(
        cx: &mut gpui::TestAppContext,
    ) {
        let view = cx.new(|cx| {
            let mut view = fixture(cx, 2);
            view.tab = Tab::Settings;
            view
        });
        cx.update(|cx| cx.set_global(MainView(view.clone())));
        view.update(cx, |view, cx| {
            view.respond_to_update(
                Response::Clicked,
                "https://github.com/mattsverse/octowatch/releases",
                cx,
            );
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(cx.windows().len(), 1));
        view.read_with(cx, |view, _| assert!(view.tab == Tab::Settings));
        // An existing window is activated rather than duplicated.
        view.update(cx, |view, cx| {
            view.respond_to_update(
                Response::Clicked,
                "https://github.com/mattsverse/octowatch/releases",
                cx,
            );
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(cx.windows().len(), 1));
    }

    #[gpui::test]
    fn review_search_follows_the_palette_without_recomputing_filters(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 2);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("#1");
        let passes = view.read_with(cx, |view, _| view.review_filter_passes);
        for (appearance, palette) in [
            (Appearance::Light, Palette::LIGHT),
            (Appearance::Dark, Palette::DARK),
        ] {
            view.update(cx, |view, cx| {
                view.store.appearance = appearance;
                cx.notify();
            });
            cx.run_until_parked();
            view.read_with(cx, |view, cx| {
                assert_eq!(view.review_search.read(cx).palette(), palette);
                assert_eq!(view.review_filters.query, "#1");
                assert_eq!(view.review_scroll.item_count(), 1);
                assert_eq!(view.review_filter_passes, passes);
            });
        }
    }

    #[gpui::test]
    fn short_window_can_reach_lower_filter_controls(cx: &mut gpui::TestAppContext) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 2);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(400.), px(320.)));
        cx.simulate_keystrokes(find_key());
        cx.simulate_keystrokes("tab enter");
        // Reach the final filter control using the real focus order.
        cx.simulate_keystrokes("tab tab tab tab tab tab tab tab tab tab tab tab");
        cx.run_until_parked();
        let snoozed = cx.debug_bounds("review-filter-Snoozed").unwrap();
        view.read_with(cx, |view, _| {
            let viewport = view.review_filter_scroll.bounds();
            assert!(snoozed.top() >= viewport.top());
            assert!(
                snoozed.bottom() <= viewport.bottom(),
                "Snoozed control is clipped: {snoozed:?}"
            );
            assert!(
                viewport.bottom() < px(260.),
                "filters must leave room for reviews"
            );
            assert_eq!(view.review_scroll.item_count(), 2);
        });
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.snooze, SnoozeFilter::Snoozed)
        });
    }

    #[gpui::test]
    fn repository_choices_scroll_with_keyboard_and_mouse_in_a_short_window(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = fixture(cx, 20);
            for pr in &mut view.store.pending {
                pr.repo = format!("Acme/repo{:02}", pr.number);
            }
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(400.), px(320.)));
        cx.simulate_keystrokes(find_key());
        cx.simulate_keystrokes("tab enter tab tab"); // Repository -> Reset -> All repositories.
        for _ in 0..20 {
            cx.simulate_keystrokes("tab");
        }
        let last = cx
            .debug_bounds("review-filter-github.com/Acme/repo20")
            .unwrap();
        view.read_with(cx, |view, _| {
            let viewport = view.review_filter_scroll.bounds();
            assert!(
                last.top() >= viewport.top() && last.bottom() <= viewport.bottom(),
                "{last:?}, viewport {viewport:?}"
            );
        });
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.review_filters.repository.as_ref(),
                Some(&RepositoryId::new("github.com", "Acme/repo20"))
            );
            assert_eq!(view.review_scroll.item_count(), 1);
            assert_eq!(view.review_filter_scroll.offset().y, px(0.));
        });
        // Open again and scroll to the final repository using the mouse.
        cx.simulate_keystrokes("tab enter");
        let viewport = view.read_with(cx, |view, _| view.review_filter_scroll.bounds());
        let last = cx
            .debug_bounds("review-filter-github.com/Acme/repo20")
            .unwrap();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: viewport.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), viewport.top() - last.top())),
            ..Default::default()
        });
        let last = cx
            .debug_bounds("review-filter-github.com/Acme/repo20")
            .unwrap();
        view.read_with(cx, |view, _| {
            let viewport = view.review_filter_scroll.bounds();
            assert!(
                last.top() >= viewport.top() && last.bottom() <= viewport.bottom(),
                "{last:?}, viewport {viewport:?}"
            );
        });
        cx.simulate_click(last.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| assert!(!view.repository_picker_open));
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("#20");
        view.read_with(cx, |view, _| assert_eq!(view.review_scroll.item_count(), 1));
    }

    #[gpui::test]
    fn short_no_results_can_scroll_to_keyboard_reset(cx: &mut gpui::TestAppContext) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 2);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(400.), px(280.)));
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("no-match");
        for _ in 0..12 {
            cx.simulate_keystrokes("tab");
        }
        let reset = cx.debug_bounds("no-results-reset").unwrap();
        assert!(reset.bottom() <= px(280.), "{reset:?}");
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters, ReviewFilters::default());
            assert_eq!(view.review_scroll.item_count(), 2);
            assert_eq!(view.review_filter_scroll.offset().y, px(0.));
        });
    }

    #[gpui::test]
    fn choosing_an_already_active_filter_keeps_cached_rows(cx: &mut gpui::TestAppContext) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        cx.simulate_keystrokes(find_key());
        cx.simulate_keystrokes("tab tab tab tab enter"); // Ready.
        let passes = view.read_with(cx, |view, _| view.review_filter_passes);
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.draft, DraftFilter::Ready);
            assert_eq!(view.review_scroll.item_count(), 5);
            assert_eq!(view.review_filter_passes, passes);
        });
        assert!(cx.debug_bounds("review:github.com/acme/api#9").is_some());
    }

    #[gpui::test]
    fn unchanged_queue_and_filters_do_not_repeat_filtering_on_repaint(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10_000);
            window.focus(&view.focus_handle);
            view
        });
        let initial = view.read_with(cx, |view, _| view.review_filter_passes);
        assert_eq!(initial, 1);
        for _ in 0..5 {
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
        let after = view.read_with(cx, |view, _| view.review_filter_passes);
        assert_eq!(after, initial, "unrelated repaints repeat filtering");
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
            assert_eq!(
                view.review_filters.repository.as_ref(),
                Some(&RepositoryId::new("github.com", "Acme/API"))
            );
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
            view.review_filter_cache.invalidate_reviews();
            view.review_filter_cache.invalidate_snoozes();
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
        let closed = cx
            .debug_bounds("review:github.com/acme/api#3")
            .unwrap()
            .size
            .height;
        let snooze = cx.debug_bounds("snooze-7").unwrap();
        cx.simulate_click(snooze.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.snooze_picker,
                Some((RepositoryId::new("github.com", "acme/api"), 3))
            )
        });
        assert!(
            cx.debug_bounds("review:github.com/acme/api#3")
                .unwrap()
                .size
                .height
                > closed
        );
        assert!(cx.debug_bounds("snooze-duration-7-120").is_some());
        let cancel = cx.debug_bounds("cancel-snooze-7").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.debug_bounds("review:github.com/acme/api#3")
                .unwrap()
                .size
                .height,
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
            view.review_filter_cache.invalidate_reviews();
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_scroll.logical_scroll_top().item_ix, 11);
            assert_eq!(view.store.pending[11].number, 90);
        });
        assert!(cx.debug_bounds("review:github.com/acme/api#90").is_some());
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
        assert!(
            cx.debug_bounds("review:github.com/acme/api#10000")
                .is_some()
        );
        assert!(
            cx.debug_bounds("review:github.com/acme/api#1").is_none(),
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
        assert!(cx.debug_bounds("review:github.com/acme/api#1").is_some());
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
            host: repository::default_host(),
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
        app.announced_hosts.insert(repository::default_host());
        app
    }

    #[gpui::test]
    fn notification_controls_render_in_both_palettes_and_preference_states(
        cx: &mut TestAppContext,
    ) {
        for appearance in [Appearance::Light, Appearance::Dark] {
            for muted in [false, true] {
                let (_, window) = cx.add_window_view(|_, cx| {
                    let mut app = app_for_picker_test(cx);
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

    #[gpui::test]
    fn enabling_repo_during_fetch_checks_it_without_waiting_for_a_poll(cx: &mut TestAppContext) {
        use std::sync::{Arc, Mutex};

        for fail_first in [false, true] {
            let (view, cx) = cx.add_window_view(|_, cx| {
                let mut app = app_for_picker_test(cx);
                app.store.pending.clear();
                app.store
                    .disabled
                    .insert(RepositoryId::new("github.com", "owner/new"));
                app.repos = Some(vec![
                    LocalRepo {
                        id: RepositoryId::new("github.com", "owner/old"),
                        paths: vec![],
                    },
                    LocalRepo {
                        id: RepositoryId::new("github.com", "owner/new"),
                        paths: vec![],
                    },
                ]);
                app
            });
            let calls = Arc::new(Mutex::new(Vec::new()));
            let (release, blocked) = async_channel::bounded::<()>(1);
            let fetch = {
                let calls = calls.clone();
                move |watched| {
                    let calls = calls.clone();
                    let blocked = blocked.clone();
                    async move {
                        let first = {
                            let mut calls = calls.lock().unwrap();
                            calls.push(watched);
                            calls.len() == 1
                        };
                        if first {
                            blocked.recv().await.unwrap();
                        }
                        if first && fail_first {
                            Err(anyhow::anyhow!("first check failed"))
                        } else {
                            Ok(github::FetchedReviews::default())
                        }
                    }
                }
            };
            view.update(cx, |app, cx| app.refresh_with(fetch.clone(), cx));
            cx.run_until_parked();
            assert_eq!(
                *calls.lock().unwrap(),
                vec![HashSet::from([RepositoryId::new(
                    "github.com",
                    "owner/old"
                )])]
            );
            // Reproduce the enable action's refresh while the first check is blocked.
            view.update(cx, |app, cx| {
                app.store
                    .disabled
                    .remove(&RepositoryId::new("github.com", "owner/new"));
                app.refresh_with(fetch.clone(), cx);
                // Several requests during one check should coalesce into one follow-up.
                app.refresh_with(fetch.clone(), cx);
            });
            cx.run_until_parked();
            assert_eq!(calls.lock().unwrap().len(), 1);
            release.try_send(()).unwrap();
            cx.run_until_parked();
            assert_eq!(
                *calls.lock().unwrap(),
                vec![
                    HashSet::from([RepositoryId::new("github.com", "owner/old")]),
                    HashSet::from([
                        RepositoryId::new("github.com", "owner/old"),
                        RepositoryId::new("github.com", "owner/new")
                    ]),
                ],
                "the newly enabled repo must be checked before another timer tick"
            );
            assert!(view.read_with(cx, |app, _| app.fetch_task.is_none()));
        }
    }

    #[gpui::test]
    fn confirmed_requests_can_finish_launch_announcement_during_a_partial_check(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let mut app = app_for_picker_test(cx);
            app.announced_hosts.clear();
            app.store.pending = vec![review(1)];
            app.review_delivery = Delivery::for_launch(&app.store);
            // No notification or persistence side effects: this request is
            // already saved, snoozed, and unchanged by the partial response.
            app.store
                .snooze(&review(1).key(), 60, Local::now().timestamp());
            app.repos = Some(vec![LocalRepo {
                id: RepositoryId::new("github.com", "owner/repo"),
                paths: vec![],
            }]);
            app
        });
        view.update(cx, |app, cx| {
            app.reconcile(
                github::FetchedReviews {
                    errors: vec!["GitHub unavailable".into()],
                    ..Default::default()
                },
                cx,
            )
        });
        assert!(view.read_with(cx, |app, _| app.announced_hosts.is_empty()));
        view.update(cx, |app, cx| {
            app.reconcile(
                github::FetchedReviews {
                    pending: vec![review(1)],
                    completed_repos: HashSet::new(),
                    errors: vec!["unreadable team".into()],
                },
                cx,
            )
        });
        assert!(view.read_with(cx, |app, _| !app.announced_hosts.is_empty()));
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
    fn host_discovery_failure_retains_cache_but_honors_removed_roots(cx: &mut TestAppContext) {
        let app = cx.new(app_for_picker_test);
        app.update(cx, |app, _| {
            app.repos = None;
            let public = review(1);
            let enterprise = PendingReview {
                host: "github.example.com".into(),
                ..public.clone()
            };
            app.store.pending = vec![public.clone(), enterprise.clone()];
            app.store.roots = vec!["/missing/kept".into()];
            app.store.discovered = vec![
                LocalRepo {
                    id: public.repository(),
                    paths: vec!["/missing/removed/clone".into()],
                },
                LocalRepo {
                    id: enterprise.repository(),
                    paths: vec!["/missing/kept/clone".into()],
                },
            ];
            app.store.queue_notifications(&app.store.pending.clone());
            app.store.snooze(&enterprise.key(), 120, 0);
            let snoozed = app.store.snoozed.clone();
            app.snooze_picker = Some(public.key());
            let changes = app
                .apply_discovery(Err(anyhow::anyhow!("host discovery failed")))
                .unwrap();
            assert!(changes.start_fetch);
            assert!(changes.cache_changed);
            assert!(changes.reviews.pending_changed && changes.reviews.notifications_changed);
            assert_eq!(app.store.pending, vec![enterprise.clone()]);
            assert_eq!(app.store.snoozed, snoozed);
            assert!(app.store.notification_queue.is_empty());
            assert_eq!(app.store.discovered[0].id, enterprise.repository());
            assert_eq!(app.repos.as_ref().unwrap(), &app.store.discovered);
            assert!(app.snooze_picker.is_none());
            assert!(
                app.scan_error
                    .as_deref()
                    .unwrap()
                    .contains("host discovery failed")
            );
        });
    }

    #[gpui::test]
    fn failed_discovery_still_checks_cached_hosts_in_both_completion_orders(
        cx: &mut TestAppContext,
    ) {
        use std::sync::{Arc, Mutex};

        for fetch_finishes_first in [true, false] {
            let cached = review(1).repository();
            let (view, cx) = cx.add_window_view(|_, cx| {
                let mut app = app_for_picker_test(cx);
                app.store.pending.clear();
                app.store.roots = vec!["/missing/watched".into()];
                app.store.discovered = vec![LocalRepo {
                    id: cached.clone(),
                    paths: vec!["/missing/watched/clone".into()],
                }];
                app.repos = Some(app.store.discovered.clone());
                app
            });
            let calls = Arc::new(Mutex::new(Vec::new()));
            let (release, blocked) = async_channel::bounded::<()>(1);
            let fetch = {
                let calls = calls.clone();
                move |watched| {
                    let calls = calls.clone();
                    let blocked = blocked.clone();
                    async move {
                        let first = {
                            let mut calls = calls.lock().unwrap();
                            calls.push(watched);
                            calls.len() == 1
                        };
                        if first {
                            blocked.recv().await.unwrap();
                        }
                        Ok(github::FetchedReviews::default())
                    }
                }
            };
            view.update(cx, |app, cx| {
                app.refresh_with(fetch.clone(), cx);
                assert!(app.refresh_queue.request_scan(false));
                // Hold discovery open while the real fetch callback completes.
                app.scan_task = Some(Task::ready(()));
            });
            cx.run_until_parked();
            assert_eq!(calls.lock().unwrap().len(), 1);
            if fetch_finishes_first {
                release.try_send(()).unwrap();
                cx.run_until_parked();
                assert!(view.read_with(cx, |app, _| app.fetch_task.is_none()));
                assert!(view.read_with(cx, |app, _| app.last_checked.is_none()));
            }
            view.update(cx, |app, cx| {
                app.scan_task = None;
                assert!(!app.refresh_queue.scan_finished(false));
                let changes = app
                    .apply_discovery(Err(anyhow::anyhow!("host discovery failed")))
                    .unwrap();
                if changes.start_fetch {
                    app.refresh_with(fetch.clone(), cx);
                }
            });
            if !fetch_finishes_first {
                cx.run_until_parked();
                assert_eq!(calls.lock().unwrap().len(), 1, "checks must not overlap");
                release.try_send(()).unwrap();
            }
            cx.run_until_parked();
            assert_eq!(
                *calls.lock().unwrap(),
                vec![HashSet::from([cached.clone()]); 2],
                "failed scan must request a replacement check for the cached repository"
            );
            assert!(view.read_with(cx, |app, _| app.last_checked.is_some()));
            assert!(view.read_with(cx, |app, _| app.fetch_task.is_none()));

            // A persistent discovery error must not block the next scheduled check.
            view.update(cx, |app, cx| {
                assert!(app.refresh_queue.request_scan(false));
                assert!(!app.refresh_queue.scan_finished(false));
                let changes = app
                    .apply_discovery(Err(anyhow::anyhow!("host discovery still failed")))
                    .unwrap();
                if changes.start_fetch {
                    app.refresh_with(fetch.clone(), cx);
                }
            });
            cx.run_until_parked();
            assert_eq!(*calls.lock().unwrap(), vec![HashSet::from([cached]); 3]);
            assert!(view.read_with(cx, |app, _| app.fetch_task.is_none()));
            assert!(view.read_with(cx, |app, _| app.scan_error.is_some()));
        }
    }

    #[gpui::test]
    fn first_host_discovery_failure_preserves_legacy_reviews_and_snoozes(cx: &mut TestAppContext) {
        let app = cx.new(app_for_picker_test);
        app.update(cx, |app, _| {
            app.repos = None;
            app.store.snooze(&review(1).key(), 5, 0);
            let pending = app.store.pending.clone();
            let snoozed = app.store.snoozed.clone();
            assert!(
                app.apply_discovery(Err(anyhow::anyhow!("host discovery failed")))
                    .is_none()
            );
            assert_eq!(app.store.pending, pending);
            assert_eq!(app.store.snoozed, snoozed);
            assert!(app.repos.is_none());
        });
    }

    #[gpui::test]
    fn discovery_pruning_dismisses_the_removed_review_picker(cx: &mut TestAppContext) {
        let app = cx.new(app_for_picker_test);
        app.update(cx, |app, _| {
            app.snooze_picker = Some(review(1).key());
            let changes = app
                .apply_discovery(Ok(discovery::ScanResult::default()))
                .unwrap();
            assert!(changes.start_fetch);
            assert!(changes.reviews.pending_changed);
            assert!(app.snooze_picker.is_none());
        });
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
        let (view, cx) = cx.add_window_view(|_, cx| {
            let mut app = app_for_picker_test(cx);
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
        keyboard: Keyboard,
        store: Store,
        picked: Vec<Option<u64>>,
    }

    impl Render for PickerHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mut controls: Vec<_> = snooze_duration_choices(self.store.snooze_minutes)
                .map(|minutes| Control::SnoozeDuration(review(1).key(), minutes))
                .collect();
            controls.push(Control::CancelSnooze(review(1).key()));
            self.keyboard
                .set_palette(self.store.appearance.palette(window.appearance()));
            self.keyboard.reconcile(controls, window, cx);
            div()
                .id("card")
                .w(px(360.))
                .p_3()
                .text_sm()
                .child(snooze_picker(
                    0,
                    self.store.snooze_minutes,
                    &self.keyboard,
                    review(1).key(),
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
        let (view, cx) = cx.add_window_view(|_, cx| PickerHarness {
            keyboard: Keyboard::new(cx),
            store: Store {
                pending: vec![review(1)],
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
