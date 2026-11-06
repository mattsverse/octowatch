//! The app's behavior without its UI: what to scan, check, persist and
//! notify, and when. The engine never awaits. Each [`Event`] (a user command,
//! finished work, or a timer) updates state and returns [`Effect`]s. The UI
//! shell starts each [`Job`] with [`Job::run`] and feeds its event back in,
//! and handles the effects that only a UI can (opening URLs, windows, dialogs).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

use chrono::Local;
use futures_lite::future::BoxedLocal;
use semver::Version;

use crate::{
    appearance::Appearance,
    discovery::{self, LocalRepo},
    github,
    health::{ReviewsStatus, SyncHealth},
    login,
    notifications::{self, Delivered, Response},
    refresh_queue::RefreshQueue,
    repository::{PUBLIC_HOST, RepositoryId},
    review_notifications::{Batch, Delivery, ReviewAction, Target},
    store::{PendingReview, ReviewKey, Store},
    updater::{self, Release},
};

/// How long scheduled update checks wait between attempts.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// How often relative times and staleness are re-rendered.
const HEALTH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub enum Update {
    /// Newer than the running build, and installable in place.
    Available(Release),
    /// Being swapped in for the running copy.
    Installing(Version),
    /// Installed over the running copy; a restart switches to it.
    Ready(Version),
    /// Couldn't be installed in place; the release page has it.
    Manual(Release),
}

/// What the user asked for.
pub enum Command {
    Refresh,
    /// Refresh, also forgetting which repositories each account can access.
    ForceRefresh,
    Rescan,
    SetPollMinutes(u64),
    SetSnoozeMinutes(u64),
    RefreshLoginStatus,
    ToggleLogin,
    ToggleNotificationsMuted,
    ToggleNotifyDrafts,
    SetAppearance(Appearance),
    /// Snooze for the default duration.
    Snooze(ReviewKey),
    SnoozeFor(ReviewKey, u64),
    Unsnooze(ReviewKey),
    ToggleRepo(String),
    ToggleAccount(String),
    ToggleRepoAccount {
        repo: String,
        account: String,
    },
    AllRepoAccounts(String),
    AddRoots(Vec<PathBuf>),
    RemoveRoot(PathBuf),
    /// `manual` checks come from the tray and answer in a dialog; scheduled
    /// ones announce a new release in a notification.
    CheckForUpdates {
        manual: bool,
    },
    InstallUpdate,
    SendTestNotification,
    /// Act on a review notification response as the current session would.
    RespondToReview(Target, Response),
}

/// Timers the engine sets. Setting one again restarts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Timer {
    /// The next scheduled GitHub check.
    Poll,
    /// The earliest snooze running out.
    Wake,
    /// Re-renders relative times and staleness.
    Health,
    /// The next scheduled update check.
    Update,
}

/// Background work, run by the shell through [`Job::run`].
pub enum Job {
    Sleep(Timer, Duration),
    Scan {
        roots: Vec<PathBuf>,
        previous: Vec<LocalRepo>,
    },
    Fetch {
        watched: BTreeSet<RepositoryId>,
        preferences: Box<Store>,
        cache: github::AccessCache,
    },
    Permission,
    /// Asks the OS for notification permission at startup.
    Authorize,
    Notify {
        slot: usize,
        summary: String,
        body: String,
        action: Option<notifications::Action>,
    },
    AwaitResponse {
        slot: usize,
        delivered: Delivered,
    },
    CheckUpdate,
    Install(Release),
}

/// Identifies a running job. Starting a job replaces (and cancels) any
/// running job with the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKey {
    Timer(Timer),
    Scan,
    Fetch,
    Permission,
    Authorize,
    Notification(usize),
    UpdateCheck,
    Install,
}

/// A timer the shell provides, so jobs can time out on its executor.
pub type Sleep = Rc<dyn Fn(Duration) -> BoxedLocal<()>>;

/// How the shell should run a job.
pub enum Work {
    /// Blocking calls (gh subprocesses, filesystem scans, the updater) that
    /// must run off the UI thread.
    Blocking(Box<dyn FnOnce() -> Done + Send>),
    /// Awaits platform callbacks and timers on the UI thread's executor.
    Async(BoxedLocal<Event>),
}

/// The result of [`Work::Blocking`] work.
pub enum Done {
    Scanned {
        roots: Vec<PathBuf>,
        result: anyhow::Result<discovery::ScanResult>,
    },
    Fetched {
        public: anyhow::Result<github::Poll>,
        enterprise: anyhow::Result<github::FetchedReviews>,
    },
    UpdateChecked(anyhow::Result<Option<Release>>),
    Installed {
        release: Release,
        result: anyhow::Result<PathBuf>,
    },
}

pub enum Event {
    Command(Command),
    Timer(Timer),
    Done(Done),
    Permission(notifications::Permission),
    /// The startup permission request was answered, or gave up.
    Authorized,
    /// The OS accepted a notification, or it couldn't be sent.
    NotificationShown {
        slot: usize,
        result: Result<Delivered, String>,
    },
    NotificationResponse {
        slot: usize,
        response: Response,
    },
}

impl From<Command> for Event {
    fn from(command: Command) -> Self {
        Self::Command(command)
    }
}

impl From<Done> for Event {
    fn from(done: Done) -> Self {
        Self::Done(done)
    }
}

/// Dialogs a manual update check answers with. The first button accepts.
#[derive(Clone, Debug)]
pub enum Prompt {
    UpToDate,
    CheckFailed(String),
    /// Accepting installs it in place.
    Available(Release),
    /// Accepting opens the release page.
    Download(Release),
    /// Accepting restarts into the installed version.
    Restart(Version),
    /// Accepting opens the release page.
    InstallFailed {
        error: String,
        release: Release,
    },
}

pub enum Effect {
    Start(Job),
    Cancel(JobKey),
    /// The tray's reviews, status or update item may have changed.
    SyncTray,
    /// The set of pending reviews changed; cached filtering is stale.
    ReviewsChanged,
    /// Snoozes changed; cached snooze filtering is stale.
    SnoozesChanged,
    OpenUrl(String),
    /// Show the window, switched to the reviews.
    ShowReviews,
    ShowWindow,
    Prompt(Prompt),
    /// The executable to relaunch after an update replaced it.
    SetRestartPath(PathBuf),
}

/// What a notification's response does once the user acts on it.
enum Responder {
    Reviews {
        generation: u64,
        batch: Box<Batch>,
        target: Target,
    },
    Update {
        url: String,
    },
    Ignore,
}

struct DiscoveryChanges {
    cache_changed: bool,
    reviews: crate::store::Reconciled,
    start_fetch: bool,
}

pub struct Engine {
    persist: bool,
    clock: fn() -> i64,
    store: Store,
    /// `None` until the first scan of the roots finishes.
    repos: Option<Vec<LocalRepo>>,
    health: SyncHealth,
    scan_issues: Vec<discovery::ScanIssue>,
    load_warning: Option<String>,
    permission: notifications::Permission,
    /// Each source keeps its own error, so a successful GitHub check doesn't
    /// hide a save, permission or delivery error.
    save_error: Option<String>,
    notification_error: Option<String>,
    login_state: login::State,
    login_error: Option<String>,
    notifications_ready: bool,
    scan_error: Option<String>,
    account_errors: BTreeMap<String, String>,
    repo_errors: BTreeMap<(String, String), String>,
    access_cache: github::AccessCache,
    /// Whether any request or complete repository check has been confirmed.
    announced_launch: bool,
    /// Retained until the launch batch is accepted, including after failures.
    launch_summary: bool,
    review_delivery: Delivery,
    /// Old-account delivery and action callbacks cannot affect a new session.
    review_account_generation: u64,
    review_account_changes: BTreeMap<String, u64>,
    /// Occupied notification slots, until their response arrives.
    notifications: HashMap<usize, Responder>,
    update: Option<Update>,
    refresh_queue: RefreshQueue,
    scanning: bool,
    fetching: bool,
    checking_permission: bool,
    checking_update: bool,
    /// Whether the running update check answers in a dialog.
    manual_update_check: bool,
    effects: Vec<Effect>,
}

fn now() -> i64 {
    Local::now().timestamp()
}

impl Engine {
    /// Loads saved state and the launch-at-login setting.
    pub fn load() -> Self {
        let (mut store, load_warning) = Store::load();
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
        let mut engine = Self::new(store);
        engine.load_warning = load_warning;
        engine.login_state = login_state;
        engine.login_error = login_error;
        engine
    }

    /// An engine over `store`, with nothing scanned or checked yet.
    pub fn new(store: Store) -> Self {
        Self {
            persist: true,
            clock: now,
            review_delivery: Delivery::for_launch(&store),
            store,
            repos: None,
            health: SyncHealth::default(),
            scan_issues: Vec::new(),
            load_warning: None,
            permission: notifications::Permission::default(),
            save_error: None,
            notification_error: None,
            login_state: login::State::Off,
            login_error: None,
            notifications_ready: !cfg!(target_os = "macos"),
            scan_error: None,
            account_errors: BTreeMap::new(),
            repo_errors: BTreeMap::new(),
            access_cache: github::AccessCache::default(),
            announced_launch: false,
            launch_summary: true,
            review_account_generation: 0,
            review_account_changes: BTreeMap::new(),
            notifications: HashMap::new(),
            update: None,
            refresh_queue: RefreshQueue::default(),
            scanning: false,
            fetching: false,
            checking_permission: false,
            checking_update: false,
            manual_update_check: false,
            effects: Vec::new(),
        }
    }

    /// Never writes the store to disk, for tests and previews.
    pub fn without_persistence(mut self) -> Self {
        self.persist = false;
        self
    }

    /// Starts polling, discovery and the permission check. Notifications
    /// wait for the OS permission prompt, where there is one.
    pub fn start(&mut self) -> Vec<Effect> {
        self.schedule_poll();
        self.rescan();
        self.check_permission();
        self.spawn(Job::Sleep(Timer::Health, HEALTH_INTERVAL));
        if cfg!(target_os = "macos") {
            // Ask without blocking the UI, before any background notification
            // can be sent. macOS only prompts when permission is undecided.
            self.spawn(Job::Authorize);
        } else {
            self.authorized();
        }
        self.take_effects()
    }

    pub fn handle(&mut self, event: impl Into<Event>) -> Vec<Effect> {
        match event.into() {
            Event::Command(command) => self.command(command),
            Event::Timer(timer) => self.timer(timer),
            Event::Done(done) => self.done(done),
            Event::Permission(permission) => {
                self.permission = permission;
                self.checking_permission = false;
            }
            Event::Authorized => self.authorized(),
            Event::NotificationShown { slot, result } => self.notification_shown(slot, result),
            Event::NotificationResponse { slot, response } => {
                self.notification_response(slot, response)
            }
        }
        self.take_effects()
    }

    fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn emit(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    fn spawn(&mut self, job: Job) {
        self.emit(Effect::Start(job));
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Refresh => self.refresh(),
            Command::ForceRefresh => self.force_refresh(),
            Command::Rescan => self.rescan(),
            Command::SetPollMinutes(minutes) => {
                if self.store.poll_minutes != minutes {
                    self.store.poll_minutes = minutes;
                    self.save();
                    self.schedule_poll();
                }
            }
            Command::SetSnoozeMinutes(minutes) => {
                if self.store.snooze_minutes != minutes {
                    self.store.snooze_minutes = minutes;
                    self.save();
                }
            }
            Command::RefreshLoginStatus => self.refresh_login_status(),
            Command::ToggleLogin => self.toggle_login(),
            Command::ToggleNotificationsMuted => {
                self.store.notifications_muted = !self.store.notifications_muted;
                self.save();
                self.emit(Effect::SyncTray);
                if !self.store.notifications_muted {
                    self.deliver_reviews();
                }
            }
            Command::ToggleNotifyDrafts => {
                self.store.notify_drafts = !self.store.notify_drafts;
                self.save();
                self.deliver_reviews();
            }
            Command::SetAppearance(appearance) => {
                if self.store.appearance != appearance {
                    self.store.appearance = appearance;
                    self.save();
                }
            }
            Command::Snooze(key) => self.snooze(key),
            Command::SnoozeFor(key, minutes) => self.snooze_for_minutes(key, minutes),
            Command::Unsnooze(key) => {
                self.store.unsnooze(&key);
                self.snoozes_changed();
            }
            Command::ToggleRepo(slug) => {
                let key = slug.to_lowercase();
                if !self.store.disabled.remove(&key) {
                    self.store.disabled.insert(key);
                }
                self.scope_changed();
            }
            Command::ToggleAccount(account) => {
                self.store.toggle_account(&account);
                self.store
                    .available_accounts
                    .remove(&account.to_lowercase());
                self.scope_changed();
            }
            Command::ToggleRepoAccount { repo, account } => {
                self.store.toggle_repo_account(&repo, &account);
                self.scope_changed();
            }
            Command::AllRepoAccounts(repo) => {
                self.store.reset_repo_accounts(&repo);
                self.scope_changed();
            }
            Command::AddRoots(paths) => {
                for path in paths {
                    if !self.store.roots.contains(&path) {
                        self.store.roots.push(path);
                    }
                }
                self.save();
                self.rescan();
            }
            Command::RemoveRoot(root) => {
                self.store.roots.retain(|r| *r != root);
                self.save();
                self.rescan();
            }
            Command::CheckForUpdates { manual } => self.check_for_updates(manual),
            Command::InstallUpdate => self.install_update(),
            Command::SendTestNotification => self.show_notification(
                "Notifications work".into(),
                "Octowatcher will tell you here when a review is requested.".into(),
                None,
                Responder::Ignore,
            ),
            Command::RespondToReview(target, response) => {
                let action = self.review_action(self.review_account_generation, &target, response);
                self.respond_to_review_action(action);
            }
        }
    }

    fn timer(&mut self, timer: Timer) {
        match timer {
            Timer::Poll => {
                self.spawn(Job::Sleep(Timer::Poll, self.poll_interval()));
                self.refresh();
            }
            Timer::Wake => {
                self.wake();
                self.deliver_reviews();
            }
            Timer::Health => {
                self.spawn(Job::Sleep(Timer::Health, HEALTH_INTERVAL));
                self.emit(Effect::SyncTray);
            }
            Timer::Update => {
                self.check_for_updates(false);
                self.spawn(Job::Sleep(Timer::Update, UPDATE_INTERVAL));
            }
        }
    }

    fn done(&mut self, done: Done) {
        match done {
            Done::Scanned { roots, result } => self.scanned(roots, result),
            Done::Fetched { public, enterprise } => self.fetched(public, enterprise),
            Done::UpdateChecked(checked) => self.update_checked(checked),
            Done::Installed { release, result } => self.installed(release, result),
        }
    }

    fn authorized(&mut self) {
        self.notifications_ready = true;
        self.deliver_reviews();
        self.schedule_wake();
        self.check_permission();
        // A dev build would overwrite its own target dir with a release.
        if !cfg!(debug_assertions) {
            self.check_for_updates(false);
            self.spawn(Job::Sleep(Timer::Update, UPDATE_INTERVAL));
        }
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.store.poll_minutes.max(1) * 60)
    }

    /// Restarts the countdown to the next check, so a new interval applies now.
    /// The scan at startup does the first check.
    fn schedule_poll(&mut self) {
        self.spawn(Job::Sleep(Timer::Poll, self.poll_interval()));
    }

    fn refresh_login_status(&mut self) {
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
    }

    fn toggle_login(&mut self) {
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
    }

    /// Notification actions keep using the global default.
    fn snooze(&mut self, key: ReviewKey) {
        self.snooze_for_minutes(key, self.store.snooze_minutes);
    }

    fn snooze_for_minutes(&mut self, key: ReviewKey, minutes: u64) {
        if self.store.snooze(&key, minutes, (self.clock)()) {
            self.snoozes_changed();
        }
    }

    fn snoozes_changed(&mut self) {
        self.emit(Effect::SnoozesChanged);
        self.save();
        self.emit(Effect::SyncTray);
        self.schedule_wake();
    }

    /// Sets a timer for the earliest snooze to run out.
    fn schedule_wake(&mut self) {
        let Some(until) = self.store.next_snooze_until() else {
            self.emit(Effect::Cancel(JobKey::Timer(Timer::Wake)));
            return;
        };
        let wait = (until - (self.clock)()).max(0) as u64;
        self.spawn(Job::Sleep(Timer::Wake, Duration::from_secs(wait)));
    }

    /// Brings back expired reviews and returns them.
    fn wake(&mut self) -> Vec<PendingReview> {
        if !self.notifications_ready {
            // Startup schedules the next wake after the permission wait. Keep
            // the deadlines intact so no reminder is consumed in the meantime.
            self.emit(Effect::Cancel(JobKey::Timer(Timer::Wake)));
            return Vec::new();
        }
        let woken = self.store.take_expired((self.clock)());
        self.snoozes_changed();
        // Scanning or failing to refresh changes freshness, not the user's
        // saved deadline. A confirmed account change already cleared snoozes.
        woken
    }

    /// Rediscovers local checkouts off the UI thread, then checks GitHub.
    /// Requests received during a scan coalesce into one follow-up snapshot.
    fn rescan(&mut self) {
        if !self.refresh_queue.request_scan(self.scanning) {
            return;
        }
        self.health.verified = false;
        self.scanning = true;
        self.spawn(Job::Scan {
            roots: self.store.roots.clone(),
            previous: self.store.discovered.clone(),
        });
        self.emit(Effect::SyncTray);
    }

    fn scanned(&mut self, roots: Vec<PathBuf>, scan: anyhow::Result<discovery::ScanResult>) {
        self.scanning = false;
        if self.refresh_queue.scan_finished(roots != self.store.roots) {
            self.rescan();
            return;
        }
        if let Some(changes) = self.apply_discovery(scan) {
            if changes.reviews.snoozes_changed {
                self.snoozes_changed();
            } else if changes.cache_changed
                || changes.reviews.pending_changed
                || changes.reviews.notifications_changed
            {
                self.save();
                self.emit(Effect::SyncTray);
            }
            // A later scan queues a fresh check for its current hosts.
            // Do not cancel and overlap synchronous gh subprocesses.
            if changes.start_fetch {
                self.fetch_reviews();
            }
        }
    }

    /// Apply discovery independently of save/fetch side effects. Failed host
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
                self.health.readiness = github::classify_failure(&err);
                self.health.hosts.clear();
                self.health.verified = false;
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
        self.store.local_repos = repos.iter().map(|repo| repo.id.store_key()).collect();
        self.repos = Some(repos);
        self.emit(Effect::ReviewsChanged);
        let reviews = self
            .store
            .retain_watched(&self.watched_repositories().into_iter().collect());
        if reviews.snoozes_changed {
            self.emit(Effect::SnoozesChanged);
        }
        Some(DiscoveryChanges {
            cache_changed,
            reviews,
            // Even a failed host lookup leaves a known, root-filtered snapshot.
            // Replace any result discarded during scanning and keep cached hosts
            // refreshing while discovery is unavailable.
            start_fetch: self.refresh_queue.request_fetch(self.fetching),
        })
    }

    fn force_refresh(&mut self) {
        self.access_cache = github::AccessCache::default();
        self.refresh();
    }

    fn refresh(&mut self) {
        self.check_permission();
        self.rescan();
    }

    fn fetch_reviews(&mut self) {
        // Scope the API request to enabled local repos after discovery finishes.
        if self.repos.is_none() || !self.refresh_queue.request_fetch(self.fetching) {
            return;
        }
        self.fetching = true;
        self.spawn(Job::Fetch {
            watched: self.watched_repositories(),
            preferences: Box::new(self.store.clone()),
            cache: self.access_cache.clone(),
        });
    }

    fn fetched(
        &mut self,
        public: anyhow::Result<github::Poll>,
        enterprise: anyhow::Result<github::FetchedReviews>,
    ) {
        self.fetching = false;
        if self.refresh_queue.fetch_finished(self.scanning) {
            // Discard the older result, including errors. This requested
            // check must run after it, without overlapping subprocesses.
            self.fetch_reviews();
            return;
        }
        // A scan in progress may invalidate the filtering list. Its
        // completion starts a fresh fetch; do not announce stale repos.
        if self.scanning {
            return;
        }
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
        let enterprise = enterprise.unwrap_or_else(|err| github::FetchedReviews {
            errors: vec![format!("{err:#}")],
            ..Default::default()
        });
        self.reconcile(poll, enterprise, error);
        self.deliver_reviews();
    }

    fn check_permission(&mut self) {
        if self.checking_permission {
            return;
        }
        self.checking_permission = true;
        self.spawn(Job::Permission);
    }

    /// Apply successful partitions independently. Keep known caches stale
    /// on unavailable checks; auth failures hide their account and block alerts.
    fn reconcile(
        &mut self,
        poll: github::Poll,
        enterprise: github::FetchedReviews,
        public_error: Option<String>,
    ) {
        let watched = self.watched_repositories();
        for account in &poll.accounts {
            if let Some(id) = account.account_id {
                self.store.record_account(&account.login, id);
            }
        }
        let changed = self.health.apply_poll(
            &poll,
            public_error.as_deref(),
            &enterprise,
            &watched,
            &mut self.store,
            (self.clock)(),
        );
        if !changed.is_empty() {
            self.reset_review_accounts(&changed);
        }

        self.account_errors.clear();
        self.repo_errors.clear();
        self.store
            .stale_accounts
            .extend(self.store.available_accounts.iter().cloned());
        if public_error.is_none() {
            // A successful auth inventory can prove removal. Network failures
            // retain a known cache; auth and identity failures do not.
            let stale: BTreeSet<_> = poll
                .checks
                .iter()
                .filter(|check| {
                    check.reviews.as_ref().err().is_some_and(|error| {
                        !matches!(
                            github::classify_failure(error),
                            github::Readiness::SignedOut
                        )
                    })
                })
                .map(|check| check.account.to_lowercase())
                .collect();
            self.store
                .stale_accounts
                .retain(|account| stale.contains(account));
        }
        self.store.available_accounts.clear();
        let failed_ids: BTreeSet<_> = poll
            .checks
            .iter()
            .filter(|check| check.reviews.is_err())
            .filter_map(|check| {
                self.store
                    .account_ids
                    .get(&check.account.to_lowercase())
                    .copied()
            })
            .collect();
        self.store
            .unavailable_repos
            .retain(|(id, _)| public_error.is_some() || failed_ids.contains(id));
        self.store.confirmed_requests.clear();
        self.access_cache = poll.access_cache;
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
            self.health.error = Some("No saved github.com accounts. Run gh auth login --hostname github.com, then Refresh.".into());
        }
        let validated_hosts: BTreeSet<_> = enterprise
            .pending
            .iter()
            .map(|pr| pr.host.clone())
            .chain(
                enterprise
                    .completed_repos
                    .iter()
                    .map(|repo| repo.host.clone()),
            )
            .collect();
        self.store
            .available_hosts
            .extend(validated_hosts.iter().cloned());
        let mut completed: BTreeSet<_> = enterprise
            .completed_repos
            .iter()
            .map(|repo| (0, repo.store_key()))
            .collect();
        let mut fetched = enterprise.pending;
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
                    completed.extend(
                        reviews
                            .completed_repos
                            .iter()
                            .map(|repo| (reviews.account_id, repo.clone())),
                    );
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
        self.store.confirmed_requests.extend(
            fetched
                .iter()
                .filter(|pr| pr.host == PUBLIC_HOST)
                .map(|pr| (pr.key(), pr.requested_at.clone())),
        );
        let confirmed = fetched.clone();
        self.store
            .reconcile_partitions(fetched, &completed, &watched);
        self.review_delivery.confirm(&mut self.store, &confirmed);
        self.emit(Effect::ReviewsChanged);
        self.emit(Effect::SnoozesChanged);
        if public_error.is_some() {
            for error in self.account_errors.values_mut() {
                *error = "Account check unavailable; see the error above.".into();
            }
        }
        if !confirmed.is_empty() || !completed.is_empty() || watched.is_empty() {
            self.announced_launch = true;
        }
        if self.store.notification_queue.is_empty() {
            self.launch_summary = false;
        }
        // Discovery and availability changes matter even when the cached list is unchanged.
        self.wake();
    }

    /// Sends eligible, undelivered review events in one batch. A failed send
    /// stays persisted and is attempted at the next successful GitHub poll.
    fn deliver_reviews(&mut self) {
        // Validate cached requests against GitHub before any launch delivery.
        if !self.announced_launch || !self.notifications_ready {
            return;
        }
        let Some(batch) = self.review_delivery.begin(&self.store) else {
            return;
        };
        self.send_review_batch(batch);
    }

    fn reset_review_accounts(&mut self, changed: &BTreeSet<String>) {
        self.store
            .available_hosts
            .retain(|host| !changed.contains(host));
        self.launch_summary = true;
        // Keep any bounded in-flight send's gate: resetting it could duplicate
        // an unaffected host's alert before OS acceptance is acknowledged.
        self.review_account_generation = self.review_account_generation.wrapping_add(1);
        for host in changed {
            self.review_account_changes
                .insert(host.clone(), self.review_account_generation);
        }
    }

    fn send_review_batch(&mut self, batch: Batch) {
        let target = Target::for_reviews(&batch.reviews);
        let (summary, body) = if self.launch_summary {
            waiting_text(&batch.reviews)
        } else {
            notification_text(&batch.reviews)
        };
        let action = target.action();
        let responder = Responder::Reviews {
            generation: self.review_account_generation,
            batch: Box::new(batch),
            target,
        };
        if !self.start_notification(summary, body, Some(action), responder) {
            self.review_delivery.deferred();
        }
    }

    fn complete_review_batch(
        &mut self,
        generation: u64,
        batch: &Batch,
        delivered: bool,
    ) -> Option<Batch> {
        // Notice sequences are never reset on account changes. An old batch
        // can acknowledge unaffected notices, but cannot consume newly queued
        // events for the changed account. Its gate stayed locked until now.
        let next = self
            .review_delivery
            .complete(&mut self.store, batch, delivered);
        if delivered {
            if generation == self.review_account_generation {
                self.launch_summary = false;
            }
            self.save();
        }
        next
    }

    fn review_action(
        &self,
        generation: u64,
        target: &Target,
        response: Response,
    ) -> Option<ReviewAction> {
        let current = match target {
            Target::Single(pr) => {
                self.review_account_changes
                    .get(&pr.host)
                    .copied()
                    .unwrap_or(0)
                    <= generation
            }
            Target::Summary => generation == self.review_account_generation,
        };
        current
            .then(|| target.respond(response, &self.store))
            .flatten()
    }

    fn respond_to_review_action(&mut self, action: Option<ReviewAction>) {
        match action {
            Some(ReviewAction::OpenPr(url)) => self.emit(Effect::OpenUrl(url)),
            Some(ReviewAction::OpenReviews) => self.emit(Effect::ShowReviews),
            Some(ReviewAction::Snooze(pr)) => self.snooze(pr.key()),
            None => {}
        }
    }

    /// Generic notifications (test/update) use the same bounded sender.
    fn show_notification(
        &mut self,
        summary: String,
        body: String,
        action: Option<notifications::Action>,
        responder: Responder,
    ) {
        if !self.start_notification(summary, body, action, responder) {
            self.notification_error =
                Some("Too many active notifications. Try again after dismissing one.".into());
        }
    }

    /// Reports OS acceptance before waiting for any action. Fixed slots and
    /// a finite observer lifetime bound tasks and platform response registries.
    fn start_notification(
        &mut self,
        summary: String,
        body: String,
        action: Option<notifications::Action>,
        responder: Responder,
    ) -> bool {
        let Some(slot) = notifications::free_slot(|slot| self.notifications.contains_key(&slot))
        else {
            return false;
        };
        self.notifications.insert(slot, responder);
        self.spawn(Job::Notify {
            slot,
            summary,
            body,
            action,
        });
        true
    }

    fn notification_shown(&mut self, slot: usize, shown: Result<Delivered, String>) {
        self.notification_error = shown
            .as_ref()
            .err()
            .map(|err| format!("could not send notification: {err}"));
        let accepted = shown.is_ok();
        if let Some(Responder::Reviews {
            generation, batch, ..
        }) = self.notifications.remove(&slot)
        {
            let next = self.complete_review_batch(generation, &batch, accepted);
            // Keep the slot occupied until the response arrives.
            self.notifications.insert(
                slot,
                Responder::Reviews {
                    generation,
                    target: Target::for_reviews(&batch.reviews),
                    batch,
                },
            );
            if let Some(next) = next {
                self.send_review_batch(next);
            }
        }
        match shown {
            Ok(delivered) => self.spawn(Job::AwaitResponse { slot, delivered }),
            // A failed send waits for a later poll or explicit resume.
            Err(_) => {
                self.notifications.remove(&slot);
            }
        }
    }

    fn notification_response(&mut self, slot: usize, response: Response) {
        match self.notifications.remove(&slot) {
            Some(Responder::Reviews {
                generation, target, ..
            }) => {
                let action = self.review_action(generation, &target, response);
                self.respond_to_review_action(action);
            }
            Some(Responder::Update { url }) => self.respond_to_update(response, url),
            Some(Responder::Ignore) | None => {}
        }
        // Successful observers also free bounded sender capacity.
        self.deliver_reviews();
    }

    fn scope_changed(&mut self) {
        self.health.verified = false;
        // Prune disabled repositories immediately; account toggles only hide
        // their partition until the queued check verifies it again.
        let watched = self.watched_repositories().into_iter().collect();
        let changes = self.store.retain_watched(&watched);
        if changes.snoozes_changed {
            self.emit(Effect::SnoozesChanged);
        }
        self.emit(Effect::ReviewsChanged);
        self.save();
        self.emit(Effect::SyncTray);
        self.schedule_wake();
        self.force_refresh();
    }

    fn check_for_updates(&mut self, manual: bool) {
        if self.checking_update || matches!(self.update, Some(Update::Installing(_))) {
            return;
        }
        self.checking_update = true;
        self.manual_update_check = manual;
        self.spawn(Job::CheckUpdate);
        self.emit(Effect::SyncTray);
    }

    fn update_checked(&mut self, checked: anyhow::Result<Option<Release>>) {
        self.checking_update = false;
        let manual = std::mem::take(&mut self.manual_update_check);
        if let Ok(Some(release)) = &checked
            && self.found(release.clone())
            && !manual
        {
            self.announce_update();
        }
        self.emit(Effect::SyncTray);
        let release = match checked {
            Ok(Some(release)) => release,
            Ok(None) => {
                if manual {
                    self.emit(Effect::Prompt(Prompt::UpToDate));
                }
                return;
            }
            Err(err) => {
                eprintln!("could not check for updates: {err:#}");
                if manual {
                    self.emit(Effect::Prompt(Prompt::CheckFailed(format!("{err:#}"))));
                }
                return;
            }
        };
        if !manual {
            return;
        }
        match self.update.clone() {
            Some(Update::Ready(version)) => self.emit(Effect::Prompt(Prompt::Restart(version))),
            Some(Update::Available(_)) => self.emit(Effect::Prompt(Prompt::Available(release))),
            Some(Update::Manual(release)) => self.emit(Effect::Prompt(Prompt::Download(release))),
            Some(Update::Installing(_)) | None => {}
        }
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
    fn announce_update(&mut self) {
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
            Responder::Update { url },
        );
    }

    fn respond_to_update(&mut self, response: Response, url: String) {
        match response {
            Response::Action(id) if id == "update" => self.install_update(),
            Response::Action(id) if id == "download" => self.emit(Effect::OpenUrl(url)),
            Response::Clicked => self.emit(Effect::ShowWindow),
            _ => {}
        }
    }

    /// Swaps in the available update, then offers to restart into it.
    fn install_update(&mut self) {
        let Some(Update::Available(release)) = self.update.clone() else {
            return;
        };
        self.update = Some(Update::Installing(release.version.clone()));
        self.emit(Effect::SyncTray);
        self.spawn(Job::Install(release));
    }

    fn installed(&mut self, release: Release, result: anyhow::Result<PathBuf>) {
        match result {
            Ok(path) => {
                // Linux relaunches the executable path, which reads as
                // deleted once it was replaced.
                self.emit(Effect::SetRestartPath(path));
                self.update = Some(Update::Ready(release.version.clone()));
                self.emit(Effect::SyncTray);
                self.emit(Effect::Prompt(Prompt::Restart(release.version)));
            }
            Err(err) => {
                eprintln!("could not install update: {err:#}");
                self.update = Some(Update::Manual(release.clone()));
                self.emit(Effect::SyncTray);
                self.emit(Effect::Prompt(Prompt::InstallFailed {
                    error: format!("{err:#}"),
                    release,
                }));
            }
        }
    }

    fn save(&mut self) {
        if !self.persist {
            return;
        }
        self.save_error = self
            .store
            .save()
            .err()
            .map(|err| format!("could not save state: {err:#}"));
    }
}

/// Read access for rendering.
impl Engine {
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// `None` until the first scan of the roots finishes.
    pub fn repos(&self) -> Option<&[LocalRepo]> {
        self.repos.as_deref()
    }

    pub fn health(&self) -> &SyncHealth {
        &self.health
    }

    pub fn scan_issues(&self) -> &[discovery::ScanIssue] {
        &self.scan_issues
    }

    pub fn scan_error(&self) -> Option<&str> {
        self.scan_error.as_deref()
    }

    pub fn scanning(&self) -> bool {
        self.scanning
    }

    pub fn fetching(&self) -> bool {
        self.fetching
    }

    pub fn permission(&self) -> &notifications::Permission {
        &self.permission
    }

    pub fn login_state(&self) -> &login::State {
        &self.login_state
    }

    pub fn login_error(&self) -> Option<&str> {
        self.login_error.as_deref()
    }

    pub fn account_errors(&self) -> &BTreeMap<String, String> {
        &self.account_errors
    }

    pub fn repo_errors(&self) -> &BTreeMap<(String, String), String> {
        &self.repo_errors
    }

    pub fn update(&self) -> Option<&Update> {
        self.update.as_ref()
    }

    pub fn checking_update(&self) -> bool {
        self.checking_update
    }

    pub fn watched_repositories(&self) -> BTreeSet<RepositoryId> {
        self.repos
            .iter()
            .flatten()
            .filter(|repo| self.store.is_enabled(&repo.id.store_key()))
            .map(|repo| repo.id.clone())
            .collect()
    }

    /// Awake reviews, as the tray lists them.
    pub fn tray_reviews(&self) -> Vec<PendingReview> {
        self.store.awake()
    }

    /// Every distinct error line, with `ui_error` (from the shell) in its place.
    pub fn errors<'a>(&'a self, ui_error: Option<&'a str>) -> Vec<&'a str> {
        let mut seen = std::collections::HashSet::new();
        [
            self.load_warning.as_deref(),
            self.health.error.as_deref(),
            self.scan_error.as_deref(),
            ui_error,
            self.save_error.as_deref(),
            self.notification_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .chain(self.account_errors.values().map(String::as_str))
        .chain(self.repo_errors.values().map(String::as_str))
        .flat_map(str::lines)
        .filter(|line| seen.insert(*line))
        .collect()
    }

    pub fn review_status(&self) -> ReviewsStatus {
        self.health.status(
            &self.store,
            (self.clock)(),
            (self.repos.is_none() && self.scan_error.is_none()) || self.scanning,
            self.fetching,
            self.watched_repositories().len(),
            !self.scan_issues.is_empty() || self.scan_error.is_some(),
        )
    }
}

/// State setup for tests in this crate and the shell's view tests.
#[cfg(any(test, feature = "test-support"))]
impl Engine {
    /// A session past startup over `store`: notifications permitted and
    /// ready, launch announced, nothing persisted, no login item support.
    pub fn settled(store: Store) -> Self {
        let mut engine = Self::new(store).without_persistence();
        engine.review_delivery = Delivery::default();
        engine.permission = notifications::Permission::Allowed("Allowed".into());
        engine.notifications_ready = true;
        engine.login_state = login::State::Unavailable("Test build".into());
        engine.announced_launch = true;
        engine.launch_summary = false;
        engine
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    pub fn set_repos(&mut self, repos: Option<Vec<LocalRepo>>) {
        self.repos = repos;
    }

    pub fn health_mut(&mut self) -> &mut SyncHealth {
        &mut self.health
    }

    pub fn set_announced_launch(&mut self, announced: bool) {
        self.announced_launch = announced;
    }

    pub fn set_update(&mut self, update: Option<Update>) {
        self.update = update;
    }

    /// Pretends a scan is (or isn't) running.
    pub fn set_scanning(&mut self, scanning: bool) {
        self.scanning = scanning;
    }

    /// Pretends a GitHub check is (or isn't) running.
    pub fn set_fetching(&mut self, fetching: bool) {
        self.fetching = fetching;
    }

    pub fn set_login(&mut self, state: login::State, error: Option<String>) {
        self.login_state = state;
        self.login_error = error;
    }
}

impl Job {
    pub fn key(&self) -> JobKey {
        match self {
            Self::Sleep(timer, _) => JobKey::Timer(*timer),
            Self::Scan { .. } => JobKey::Scan,
            Self::Fetch { .. } => JobKey::Fetch,
            Self::Permission => JobKey::Permission,
            Self::Authorize => JobKey::Authorize,
            Self::Notify { slot, .. } | Self::AwaitResponse { slot, .. } => {
                JobKey::Notification(*slot)
            }
            Self::CheckUpdate => JobKey::UpdateCheck,
            Self::Install(_) => JobKey::Install,
        }
    }

    pub fn run(self, sleep: Sleep) -> Work {
        match self {
            Self::Sleep(timer, duration) => Work::Async(Box::pin(async move {
                sleep(duration).await;
                Event::Timer(timer)
            })),
            Self::Scan { roots, previous } => Work::Blocking(Box::new(move || {
                let result = github::known_hosts().map(|hosts| {
                    let mut scan = discovery::discover(&roots, &hosts);
                    let previous = discovery::within_roots(&previous, &roots);
                    scan.retain_unavailable(&previous);
                    scan
                });
                Done::Scanned { roots, result }
            })),
            Self::Fetch {
                watched,
                preferences,
                cache,
            } => Work::Blocking(Box::new(move || {
                let repos = watched
                    .iter()
                    .filter(|repo| repo.host == PUBLIC_HOST)
                    .map(|repo| repo.slug.clone())
                    .collect::<Vec<_>>();
                let enterprise_repos: std::collections::HashSet<_> = watched
                    .into_iter()
                    .filter(|repo| repo.host != PUBLIC_HOST)
                    .collect();
                Done::Fetched {
                    public: github::poll(&repos, *preferences, cache),
                    enterprise: if enterprise_repos.is_empty() {
                        Ok(github::FetchedReviews::default())
                    } else {
                        github::fetch_awaiting_reviews(&enterprise_repos)
                    },
                }
            })),
            Self::Permission => Work::Async(Box::pin(async move {
                Event::Permission(
                    futures_lite::future::race(notifications::permission(), async {
                        sleep(Duration::from_secs(10)).await;
                        notifications::Permission::Unknown(
                            "Permission check timed out. Refresh to retry.".into(),
                        )
                    })
                    .await,
                )
            })),
            Self::Authorize => Work::Async(Box::pin(async move {
                #[cfg(target_os = "macos")]
                let _authorization =
                    futures_lite::future::race(notifications::request_auth(), async {
                        sleep(Duration::from_secs(30)).await;
                        Err("Notification permission request timed out; check System Settings and Refresh".into())
                    })
                    .await;
                #[cfg(not(target_os = "macos"))]
                let _ = sleep;
                Event::Authorized
            })),
            Self::Notify {
                slot,
                summary,
                body,
                action,
            } => Work::Async(Box::pin(async move {
                let result = futures_lite::future::or(
                    notifications::send(slot, &summary, &body, action),
                    async {
                        sleep(Duration::from_secs(15)).await;
                        Err("notification service did not answer within 15 seconds".into())
                    },
                )
                .await;
                Event::NotificationShown { slot, result }
            })),
            Self::AwaitResponse { slot, delivered } => Work::Async(Box::pin(async move {
                let response = delivered.response(|duration| sleep(duration)).await;
                Event::NotificationResponse { slot, response }
            })),
            Self::CheckUpdate => Work::Blocking(Box::new(|| Done::UpdateChecked(updater::check()))),
            Self::Install(release) => Work::Blocking(Box::new(move || {
                let result = updater::install(&release);
                Done::Installed { release, result }
            })),
        }
    }
}

pub fn waiting_text(prs: &[PendingReview]) -> (String, String) {
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

pub fn notification_text(prs: &[PendingReview]) -> (String, String) {
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

#[cfg(test)]
mod tests;
