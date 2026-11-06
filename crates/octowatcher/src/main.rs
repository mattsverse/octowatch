use octowatcher_core::{
    appearance::Appearance,
    engine::{self, Command, Effect, Engine, Job, JobKey, Prompt, Update, Work},
    github, health, instance, login, notifications, repository, review_filter, store, updater,
};

mod keyboard;
#[cfg(test)]
mod keyboard_tests;
mod search_input;
mod theme;
mod tray;

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    rc::Rc,
};

use chrono::{DateTime, Local};
use gpui::{
    App, Application, AsyncApp, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable,
    FontWeight, Global, KeyBinding, KeyDownEvent, ListAlignment, ListOffset, ListState,
    PathPromptOptions, PromptButton, PromptLevel, ScrollHandle, SharedString, Subscription, Task,
    WeakEntity, Window, WindowBounds, WindowOptions, actions, div, list, point, prelude::*, px,
    relative, rgb, size,
};

use health::ReviewsStatus;
use keyboard::{Control, Keyboard};
use repository::{PUBLIC_HOST, RepositoryId};
use review_filter::{DraftFilter, ReviewFilter, ReviewFilterCache, ReviewFilters, SnoozeFilter};
use search_input::SearchInput;
use store::ReviewKey;
use theme::Palette;
use tray::{Tray, UpdateItem};

/// Choices offered in Settings for minutes between GitHub checks.
const POLL_CHOICES: [u64; 7] = [1, 2, 5, 10, 15, 30, 60];
/// Choices offered in Settings and for an individual review's snooze.
const SNOOZE_CHOICES: [u64; 6] = [5, 10, 15, 30, 60, 120];

actions!(
    octowatcher,
    [Quit, Refresh, FocusReviewSearch, ResetReviewFilters]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tab {
    Reviews,
    Repositories,
    Settings,
}

#[derive(PartialEq, Eq)]
struct ReviewListItem {
    key: ReviewKey,
    picker_open: bool,
}

struct Octowatcher {
    keyboard: Keyboard,
    engine: Engine,
    /// Running engine jobs, each with the id it was started under.
    jobs: HashMap<JobKey, (u64, Task<()>)>,
    next_job: u64,
    /// Tests record started jobs instead of running gh, timers or notifications.
    #[cfg(test)]
    run_jobs: bool,
    #[cfg(test)]
    started: Vec<JobKey>,
    tab: Tab,
    review_filters: ReviewFilters,
    review_filter_cache: ReviewFilterCache,
    review_filter_scroll: ScrollHandle,
    review_no_results_scroll: ScrollHandle,
    review_control_focus: RefCell<HashMap<gpui::ElementId, (FocusHandle, Option<usize>)>>,
    rendered_snooze_picker: Option<ReviewKey>,
    review_search: Entity<SearchInput>,
    review_filters_open: bool,
    repository_picker_open: bool,
    review_scroll: ListState,
    review_list_items: Vec<ReviewListItem>,
    /// The one PR whose duration picker is open; never persisted as a default.
    snooze_picker: Option<ReviewKey>,
    focus_handle: FocusHandle,
    _search_subscription: Subscription,
    #[cfg(test)]
    review_filter_passes: usize,
    tray_error: Option<String>,
    tray: Option<Tray>,
    #[cfg(target_os = "linux")]
    restart_path: Option<PathBuf>,
    /// Reattached when a closed window is opened again.
    appearance_subscription: Option<Subscription>,
    /// Writes unsaved state before exit, which a background save can miss.
    _flush_on_quit: Subscription,
}

impl Octowatcher {
    fn controls(&self) -> Vec<Control> {
        let mut controls = vec![Control::Refresh];
        if matches!(
            self.engine.update(),
            Some(Update::Available(_) | Update::Ready(_) | Update::Manual(_))
        ) {
            controls.push(Control::Update);
        }
        controls.extend([Tab::Reviews, Tab::Repositories, Tab::Settings].map(Control::Tab));
        match self.tab {
            Tab::Reviews => {
                controls.push(Control::Filter("toggle-review-filters".into()));
                if self.review_filters_open {
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
                }
                let indices = self.review_filter_cache.visible_indices();
                if indices.is_empty() && self.review_filters.active_count() > 0 {
                    controls.push(Control::Filter("no-results-reset".into()));
                }
                for &ix in indices.iter() {
                    let pr = &self.engine.store().pending[ix];
                    controls.extend([Control::Review(pr.key()), Control::Snooze(pr.key())]);
                    if self.snooze_picker.as_ref() == Some(&pr.key())
                        && self.engine.store().snooze_for(pr).is_none()
                    {
                        controls.extend(
                            snooze_duration_choices(self.engine.store().snooze_minutes)
                                .map(|minutes| Control::SnoozeDuration(pr.key(), minutes)),
                        );
                        controls.push(Control::CancelSnooze(pr.key()));
                    }
                }
            }
            Tab::Repositories => {
                controls.extend(
                    self.engine
                        .store()
                        .roots
                        .iter()
                        .cloned()
                        .map(Control::RemoveRoot),
                );
                controls.extend([Control::AddRoot, Control::Rescan]);
                for repo in self.engine.repos().into_iter().flatten() {
                    controls.push(Control::Repository(repo.id.clone()));
                    if repo.id.host == PUBLIC_HOST && !self.engine.store().known_accounts.is_empty()
                    {
                        controls.push(Control::AllRepositoryAccounts(repo.id.slug.clone()));
                        controls.extend(self.engine.store().known_accounts.iter().map(|account| {
                            Control::RepositoryAccount(repo.id.slug.clone(), account.clone())
                        }));
                    }
                }
            }
            Tab::Settings => {
                controls.extend(self.health_controls());
                controls.extend(
                    self.engine
                        .store()
                        .known_accounts
                        .iter()
                        .cloned()
                        .map(Control::Account),
                );
                controls.extend(Appearance::CHOICES.map(Control::Appearance));
                if !matches!(self.engine.login_state(), login::State::Unavailable(_)) {
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
        // Discovery and sync do not depend on answering the OS prompt.
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                let effects = this.engine.start();
                this.apply(effects, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        let engine = Engine::load();
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
        // Retain cached links/counts while the status makes their freshness
        // explicit. A confirmed account change clears them in `apply`.
        let (tray, tray_error) = match Tray::new(
            &engine.tray_reviews(),
            ReviewsStatus::Loading.tray_message(),
            engine.store().notifications_muted,
        ) {
            Ok(tray) => (Some(tray), None),
            Err(err) => (None, Some(format!("could not create tray icon: {err:#}"))),
        };
        let keyboard = Keyboard::new(cx);
        let focus_handle = keyboard.root.clone();
        let flush_on_quit = cx.on_app_quit(|this, _| {
            this.engine.flush();
            async {}
        });
        Self {
            keyboard,
            engine,
            jobs: HashMap::new(),
            next_job: 0,
            #[cfg(test)]
            run_jobs: true,
            #[cfg(test)]
            started: Vec::new(),
            tab: Tab::Reviews,
            review_filters: ReviewFilters::default(),
            review_filter_cache: ReviewFilterCache::default(),
            review_filter_scroll: ScrollHandle::default(),
            review_no_results_scroll: ScrollHandle::default(),
            review_control_focus: RefCell::default(),
            rendered_snooze_picker: None,
            review_search,
            review_filters_open: false,
            repository_picker_open: false,
            review_scroll: ListState::new(0, ListAlignment::Top, px(0.)),
            review_list_items: Vec::new(),
            snooze_picker: None,
            focus_handle,
            _search_subscription: search_subscription,
            #[cfg(test)]
            review_filter_passes: 0,
            tray_error,
            tray,
            #[cfg(target_os = "linux")]
            restart_path: None,
            appearance_subscription: None,
            _flush_on_quit: flush_on_quit,
        }
    }

    /// Hands an event to the engine and carries out what it asks for.
    fn dispatch(&mut self, event: impl Into<engine::Event>, cx: &mut Context<Self>) {
        let effects = self.engine.handle(event);
        self.apply(effects, cx);
        cx.notify();
    }

    fn apply(&mut self, effects: Vec<Effect>, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                Effect::Start(job) => self.run_job(job, cx),
                // Dropping a task cancels it.
                Effect::Cancel(key) => drop(self.jobs.remove(&key)),
                Effect::SyncTray => self.sync_tray(),
                Effect::ReviewsChanged => self.review_filter_cache.invalidate_reviews(),
                Effect::SnoozesChanged => self.review_filter_cache.invalidate_snoozes(),
                Effect::OpenUrl(url) => cx.open_url(&url),
                Effect::ShowReviews => {
                    self.tab = Tab::Reviews;
                    // This runs inside an entity update; opening a new window
                    // reads this view and must wait for it to be returned.
                    cx.defer(show_window);
                }
                Effect::ShowWindow => cx.defer(show_window),
                Effect::Prompt(prompt) => cx
                    .spawn(async move |this, cx| answer_prompt(prompt, this, cx).await)
                    .detach(),
                Effect::SetRestartPath(path) => {
                    cx.set_restart_path(path.clone());
                    #[cfg(target_os = "linux")]
                    {
                        self.restart_path = Some(path);
                    }
                }
            }
        }
        self.dismiss_stale_snooze_picker();
    }

    /// Runs a job on gpui's executors, replacing any running job with its key.
    fn run_job(&mut self, job: Job, cx: &mut Context<Self>) {
        let key = job.key();
        #[cfg(test)]
        if !self.run_jobs {
            self.started.push(key);
            return;
        }
        let id = self.next_job;
        self.next_job += 1;
        let executor = cx.background_executor().clone();
        let sleep: engine::Sleep = Rc::new(move |duration| Box::pin(executor.timer(duration)));
        let work = job.run(sleep);
        let task = cx.spawn(async move |this, cx| {
            let event = match work {
                Work::Blocking(work) => cx
                    .background_executor()
                    .spawn(async move { work() })
                    .await
                    .into(),
                Work::Async(work) => work.await,
            };
            this.update(cx, |this, cx| {
                // This task is finishing: let go of it without cancelling it.
                if this.jobs.get(&key).is_some_and(|(job, _)| *job == id) {
                    this.jobs.remove(&key).unwrap().1.detach();
                }
                this.dispatch(event, cx);
            })
            .ok();
        });
        self.jobs.insert(key, (id, task));
    }

    fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        self.dispatch(command, cx);
    }

    fn set_poll_minutes(&mut self, minutes: u64, cx: &mut Context<Self>) {
        self.command(Command::SetPollMinutes(minutes), cx);
    }

    fn set_snooze_minutes(&mut self, minutes: u64, cx: &mut Context<Self>) {
        self.command(Command::SetSnoozeMinutes(minutes), cx);
    }

    fn refresh_login_status(&mut self, cx: &mut Context<Self>) {
        self.command(Command::RefreshLoginStatus, cx);
    }

    fn toggle_login(&mut self, cx: &mut Context<Self>) {
        self.command(Command::ToggleLogin, cx);
    }

    fn toggle_notifications_muted(&mut self, cx: &mut Context<Self>) {
        self.command(Command::ToggleNotificationsMuted, cx);
    }

    fn toggle_notify_drafts(&mut self, cx: &mut Context<Self>) {
        self.command(Command::ToggleNotifyDrafts, cx);
    }

    fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        self.command(Command::SetAppearance(appearance), cx);
    }

    fn snooze_for_minutes(&mut self, key: ReviewKey, minutes: u64, cx: &mut Context<Self>) {
        if self.snooze_picker.as_ref() == Some(&key) {
            self.snooze_picker = None;
        }
        self.command(Command::SnoozeFor(key, minutes), cx);
    }

    /// Snoozes for the default duration, as a notification action does.
    #[cfg(test)]
    fn snooze(&mut self, key: ReviewKey, cx: &mut Context<Self>) {
        if self.snooze_picker.as_ref() == Some(&key) {
            self.snooze_picker = None;
        }
        self.command(Command::Snooze(key), cx);
    }

    fn unsnooze(&mut self, key: ReviewKey, cx: &mut Context<Self>) {
        self.command(Command::Unsnooze(key), cx);
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.command(Command::Rescan, cx);
    }

    fn force_refresh(&mut self, cx: &mut Context<Self>) {
        self.command(Command::ForceRefresh, cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.command(Command::Refresh, cx);
    }

    #[cfg(test)]
    fn respond_to_review(
        &mut self,
        target: &octowatcher_core::review_notifications::Target,
        response: notifications::Response,
        cx: &mut Context<Self>,
    ) {
        self.command(Command::RespondToReview(target.clone(), response), cx);
    }

    fn send_test_notification(&mut self, cx: &mut Context<Self>) {
        self.command(Command::SendTestNotification, cx);
    }

    fn watched_repositories(&self) -> BTreeSet<RepositoryId> {
        self.engine.watched_repositories()
    }

    fn toggle_repo(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.command(Command::ToggleRepo(slug.into()), cx);
    }

    fn toggle_account(&mut self, account: &str, cx: &mut Context<Self>) {
        self.command(Command::ToggleAccount(account.into()), cx);
    }

    fn toggle_repo_account(&mut self, repo: &str, account: &str, cx: &mut Context<Self>) {
        self.command(
            Command::ToggleRepoAccount {
                repo: repo.into(),
                account: account.into(),
            },
            cx,
        );
    }

    fn all_repo_accounts(&mut self, repo: &str, cx: &mut Context<Self>) {
        self.command(Command::AllRepoAccounts(repo.into()), cx);
    }

    fn dismiss_stale_snooze_picker(&mut self) {
        let store = self.engine.store();
        if let Some(key) = &self.snooze_picker
            && !store
                .pending
                .iter()
                .any(|pr| pr.key() == *key && store.visible(pr) && store.snooze_for(pr).is_none())
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
            this.update(cx, |this, cx| this.command(Command::AddRoots(paths), cx))
                .ok();
        })
        .detach();
    }

    fn remove_root(&mut self, root: &std::path::Path, cx: &mut Context<Self>) {
        self.command(Command::RemoveRoot(root.to_path_buf()), cx);
    }

    fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        self.command(Command::CheckForUpdates { manual }, cx);
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        self.command(Command::InstallUpdate, cx);
    }

    fn sync_tray(&mut self) {
        let Some(tray) = &self.tray else { return };
        let item = match self.engine.update() {
            _ if self.engine.checking_update() => UpdateItem::Checking,
            Some(Update::Available(release)) => UpdateItem::Available(&release.version),
            Some(Update::Installing(version)) => UpdateItem::Installing(version),
            Some(Update::Ready(version)) => UpdateItem::Ready(version),
            Some(Update::Manual(_)) | None => UpdateItem::Check,
        };
        self.tray_error = tray
            .update(
                &self.engine.tray_reviews(),
                item,
                self.engine.review_status().tray_message(),
                self.engine.store().notifications_muted,
            )
            .err()
            .map(|err| format!("could not update tray menu: {err:#}"));
    }

    fn errors(&self) -> Vec<&str> {
        self.engine.errors(self.tray_error.as_deref())
    }

    fn review_status(&self) -> ReviewsStatus {
        self.engine.review_status()
    }
}

/// Shows a manual update check's dialog and carries out the answer.
async fn answer_prompt(prompt: Prompt, this: WeakEntity<Octowatcher>, cx: &mut AsyncApp) {
    let current = updater::current_version();
    let later = PromptButton::cancel("Later");
    let (level, message, detail, buttons) = match &prompt {
        Prompt::UpToDate => (
            PromptLevel::Info,
            "Octowatcher is up to date".to_string(),
            format!("Version {current} is the latest release."),
            vec![PromptButton::ok("OK")],
        ),
        Prompt::CheckFailed(error) => (
            PromptLevel::Warning,
            "Could not check for updates".into(),
            error.clone(),
            vec![PromptButton::ok("OK")],
        ),
        Prompt::Available(release) => (
            PromptLevel::Info,
            format!("Octowatcher {} is available", release.version),
            format!("You have {current}."),
            vec![PromptButton::ok("Update"), later],
        ),
        Prompt::Download(release) => (
            PromptLevel::Info,
            format!("Octowatcher {} is available", release.version),
            "Download it from the release page.".into(),
            vec![PromptButton::ok("Download"), later],
        ),
        Prompt::Restart(version) => (
            PromptLevel::Info,
            format!("Octowatcher {version} is installed"),
            "Restart now to start using it?".into(),
            vec![PromptButton::ok("Restart Now"), later],
        ),
        Prompt::InstallFailed { error, .. } => (
            PromptLevel::Warning,
            "Could not install the update".into(),
            error.clone(),
            vec![PromptButton::ok("Download"), later],
        ),
    };
    if ask(cx, level, &message, &detail, &buttons).await != Some(0) {
        return;
    }
    match prompt {
        Prompt::UpToDate | Prompt::CheckFailed(_) => {}
        Prompt::Available(_) => {
            this.update(cx, |this, cx| this.install_update(cx)).ok();
        }
        Prompt::Download(release) | Prompt::InstallFailed { release, .. } => {
            cx.update(|cx| cx.open_url(&release.url)).ok();
        }
        Prompt::Restart(_) => {
            cx.update(restart).ok();
        }
    }
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
        let theme = theme::palette(self.engine.store().appearance, window.appearance());
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
                    .when(self.tab == Tab::Reviews && self.needs_health(), |s| {
                        s.py_2()
                    })
                    .child(content),
            )
    }
}

impl Octowatcher {
    fn last_sync_text(&self) -> String {
        match self
            .engine
            .store()
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

    fn github_readiness_text(&self, host: &str, readiness: &github::Readiness) -> String {
        let cached = self.engine.store().sync_accounts.get(host).or_else(|| {
            (host == "github.com")
                .then_some(self.engine.store().sync_account.as_ref())
                .flatten()
        });
        let previous = cached
            .map(|login| format!(" Last verified account: @{login}."))
            .unwrap_or_default();
        match readiness {
            github::Readiness::Checking => {
                format!("{host}: checking GitHub CLI and active account…")
            }
            github::Readiness::Missing => format!(
                "{host}: GitHub CLI not found. Install gh 2.81+, run gh auth login --hostname {host}, then Rescan."
            ),
            github::Readiness::NotRunnable => format!(
                "{host}: GitHub CLI could not start. Check its installation and executable permissions, then Rescan."
            ),
            github::Readiness::SignedOut => format!(
                "{host}: authentication needs attention. Run gh auth login --hostname {host}, then Refresh."
            ),
            github::Readiness::Offline => format!(
                "{host}: cannot be reached. Check your connection or proxy, then Refresh.{previous}"
            ),
            github::Readiness::Unavailable => format!(
                "{host}: account readiness could not be verified. Check connection, CLI version and server compatibility, then Refresh or Rescan.{previous}"
            ),
            github::Readiness::Ready(login) => format!(
                "{host}: GitHub CLI ready · verified account(s) @{login}.{}",
                if self.engine.health().error.is_some() {
                    " Some review checks failed; cached results may be stale."
                } else {
                    ""
                }
            ),
        }
    }

    fn needs_health(&self) -> bool {
        self.review_status() != ReviewsStatus::Ready
            || !self.errors().is_empty()
            || matches!(self.engine.permission(), notifications::Permission::Denied)
            || (cfg!(target_os = "macos")
                && matches!(
                    self.engine.permission(),
                    notifications::Permission::Unknown(_)
                ))
    }

    fn needs_login(&self) -> bool {
        matches!(
            self.engine.health().readiness,
            github::Readiness::Missing | github::Readiness::SignedOut
        ) || self
            .engine
            .health()
            .hosts
            .values()
            .any(|ready| matches!(ready, github::Readiness::SignedOut))
    }

    fn health_controls(&self) -> Vec<Control> {
        let mut controls = Vec::new();
        if matches!(self.engine.health().readiness, github::Readiness::Missing) {
            controls.push(Control::InstallGh);
        }
        if self.needs_login() {
            controls.push(Control::CopyLogin);
        }
        controls.extend([
            Control::HealthFolders,
            Control::HealthAddRoot,
            Control::HealthRescan,
        ]);
        if cfg!(target_os = "macos") {
            controls.push(Control::NotificationSettings);
        }
        controls.extend([Control::HealthTestNotification, Control::HealthRefresh]);
        controls
    }

    fn render_health(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let github = if self.engine.health().hosts.is_empty() {
            self.github_readiness_text("github.com", &self.engine.health().readiness)
        } else {
            self.engine
                .health()
                .hosts
                .iter()
                .map(|(host, readiness)| self.github_readiness_text(host, readiness))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let login_host = self
            .engine
            .health()
            .hosts
            .iter()
            .find(|(_, readiness)| matches!(readiness, github::Readiness::SignedOut))
            .map(|(host, _)| host.as_str())
            .unwrap_or("github.com");
        let github = if self.engine.health().accounts.is_empty() {
            github
        } else {
            format!(
                "{}\nSaved github.com accounts: {}\nCLI active account: {}",
                github,
                self.engine
                    .health()
                    .accounts
                    .iter()
                    .map(|(account, ready)| format!(
                        "@{account}: {}",
                        match ready {
                            github::Readiness::Ready(_) => "identity verified",
                            github::Readiness::Offline => "unreachable · known cache may be stale",
                            github::Readiness::SignedOut => "signed out · sign in and Refresh",
                            _ => "check unavailable · Refresh",
                        }
                    ))
                    .collect::<Vec<_>>()
                    .join("; "),
                self.engine
                    .health()
                    .active_public
                    .as_deref()
                    .map(|account| format!("@{account}"))
                    .unwrap_or_else(|| "unknown".into())
            )
        };
        let needs_login = self.needs_login();
        let login_command = format!("gh auth login --hostname {login_host}");
        let folders = if self.engine.scan_error().is_some() {
            "Host discovery failed. Fix GitHub CLI setup, then Rescan.".into()
        } else if self.engine.repos().is_none() || self.engine.scanning() {
            "Scanning watched folders…".into()
        } else if self.engine.store().roots.is_empty() {
            "No watched folders. Add a folder containing GitHub clones.".into()
        } else if !self.engine.scan_issues().is_empty() {
            format!(
                "Folder scan incomplete: {} issue(s). Fix the paths or access permissions, then Rescan.",
                self.engine.scan_issues().len()
            )
        } else if self.engine.repos().is_some_and(<[_]>::is_empty) {
            "No GitHub clones found. Add a project folder or clone a repository there, then Rescan."
                .into()
        } else if self.watched_repositories().is_empty() {
            "Every discovered repository is off. Enable a repository in Repositories.".into()
        } else {
            format!(
                "{} repositories enabled in {} watched folder(s).",
                self.watched_repositories().len(),
                self.engine.store().roots.len()
            )
        };
        let permission = match &self.engine.permission() {
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
        div().w_full().flex_shrink_0().debug_selector(|| "setup-health-panel".into()).flex().flex_col().gap_3().p_3().rounded_lg().bg(rgb(theme.surface))
            .child(section_title("Setup & health", theme))
            .child(health_row("GitHub", github, theme))
            .when(matches!(self.engine.health().readiness, github::Readiness::Missing), |s| s.child(self.button(Control::InstallGh, "Install GitHub CLI", theme).on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.open_url("https://cli.github.com/")))))
            .when(needs_login, |s| s.child(self.button(Control::CopyLogin, "Copy login command", theme).on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(login_command.clone()))))))
            .child(health_row("Watched folders", folders, theme))
            .child(div().flex().flex_wrap().gap_2()
                .child(self.button(Control::HealthFolders, "Manage folders", theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| { this.tab = Tab::Repositories; this.snooze_picker = None; cx.notify(); })))
                .child(self.button(Control::HealthAddRoot, "Add folder…", theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_root(cx))))
                .child(self.button(Control::HealthRescan, "Rescan", theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.rescan(cx)))))
            .child(health_row("Notification permission", permission, theme))
            .child(div().flex().flex_wrap().gap_2()
                .when(cfg!(target_os = "macos"), |s| s.child(self.button(Control::NotificationSettings, "Notification settings", theme).on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.open_url("x-apple.systempreferences:com.apple.Notifications-Settings.extension")))))
                .child(self.button(Control::HealthTestNotification, "Send test notification", theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.send_test_notification(cx)))))
            .child(health_row("Review sync", format!("{} · {sync}", self.last_sync_text()), theme))
            .child(self.button(Control::HealthRefresh, "Refresh", theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx))))
            .children(self.errors().into_iter().map(str::to_string).chain(self.engine.scan_issues().iter().map(|issue| format!("{}: {}", issue.path.display(), issue.message))).map(|error| div().text_xs().text_color(rgb(theme.error)).child(error)))
    }

    fn render_header(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let status: SharedString = if self.engine.scanning() {
            "Scanning folders…".into()
        } else if self.engine.fetching() {
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
        let repo_count = self.engine.repos().map_or(0, <[_]>::len);

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
                                cx.listener(|this, _: &ClickEvent, _, cx| this.force_refresh(cx)),
                            )),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme.secondary_text))
                    .child(self.last_sync_text()),
            )
            .when(!self.errors().is_empty(), |s| {
                s.child(div().text_xs().text_color(rgb(theme.error)).child(format!(
                    "{} issue(s) need attention — see Settings",
                    self.errors().len()
                )))
            })
            .when(!self.engine.scan_issues().is_empty(), |s| {
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
                        format!(
                                "Reviews ({})",
                                self.engine.store()
                                    .pending
                                    .iter()
                                    .filter(|pr| self.engine.store().visible(pr))
                                    .count()
                            ),
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
            match self.engine.update().as_ref()? {
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
            .debug_selector(move || format!("tab-{tab:?}"))
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
        self.review_filters_open = true;
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
        if self.review_filters_open {
            window.focus(&self.review_search.focus_handle(cx));
        } else {
            self.keyboard
                .focus(&Control::Filter("toggle-review-filters".into()), window);
        }
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
            Some(control) if self.health_controls().contains(&control) => {
                let Some(bounds) = self.keyboard.bounds(&control) else {
                    return;
                };
                let viewport = self.review_no_results_scroll.bounds();
                let adjustment = if bounds.top() < viewport.top() {
                    viewport.top() - bounds.top()
                } else if bounds.bottom() > viewport.bottom() {
                    viewport.bottom() - bounds.bottom()
                } else {
                    px(0.)
                };
                if adjustment != px(0.) {
                    let mut offset = self.review_no_results_scroll.offset();
                    offset.y += adjustment;
                    self.review_no_results_scroll.set_offset(offset);
                    window.refresh();
                }
            }
            _ => {}
        }
    }

    fn scroll_focused_review_control(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.keyboard.focused(window) == Some(Control::Filter("toggle-review-filters".into())) {
            return;
        }
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
                self.review_no_results_scroll.scroll_to_bottom();
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
                if picker_open && !this.repository_picker_open && this.review_filters_open {
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
                        if picker_open && !this.repository_picker_open && this.review_filters_open {
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
            .debug_selector(|| "review-filter-controls".into())
            .max_h(relative(0.35))
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
            .refresh(self.engine.store(), &self.review_filters);
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
                    let key = self.engine.store().pending[ix].key();
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
        let status = self.review_status();
        let needs_health = self.needs_health();
        let indices = self.review_filter_cache.visible_indices();
        let count = indices.len();
        let total = self
            .engine
            .store()
            .pending
            .iter()
            .filter(|pr| self.engine.store().visible(pr))
            .count();
        let active_filters = self.review_filters.active_count();
        let arrow = if self.review_filters_open {
            "▴"
        } else {
            "▾"
        };
        let filter_label = if active_filters > 0 {
            format!("Search & filters ({active_filters} active) {arrow}")
        } else {
            format!("Search & filters {arrow}")
        };
        let view = cx.entity();
        let viewport = self.keyboard.review_viewport.clone();
        div()
            .flex()
            .flex_col()
            .h_full()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        self.review_control(
                            "toggle-review-filters",
                            filter_label,
                            active_filters > 0,
                            None,
                            |this, window, cx| {
                                this.review_filters_open = !this.review_filters_open;
                                this.repository_picker_open = false;
                                this.review_filter_scroll.set_offset(point(px(0.), px(0.)));
                                this.keyboard.focus(
                                    &Control::Filter("toggle-review-filters".into()),
                                    window,
                                );
                                cx.notify();
                            },
                            cx,
                        )
                        .debug_selector(|| "toggle-review-filters".into()),
                    )
                    .child(div().text_xs().text_color(rgb(theme.secondary_text)).child(
                        if active_filters > 0 {
                            format!("{count} of {total} reviews")
                        } else {
                            format!("{total} reviews")
                        },
                    )),
            )
            .when(self.review_filters_open, |s| {
                s.child(self.render_review_filters(cx))
            })
            .when(
                needs_health
                    && self
                        .engine
                        .store()
                        .pending
                        .iter()
                        .any(|pr| self.engine.store().visible(pr)),
                |s| {
                    s.child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(rgb(theme.warning))
                            .debug_selector(|| "review-health-warning".into())
                            .child(if status != ReviewsStatus::Ready {
                                "Last known reviews · may be stale. See Settings."
                            } else {
                                "Setup needs attention. See Settings."
                            }),
                    )
                },
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
                        .gap_2()
                        .pt_2()
                        .text_color(rgb(theme.muted_text))
                        .child(
                            if !self
                                .engine
                                .store()
                                .pending
                                .iter()
                                .any(|pr| self.engine.store().visible(pr))
                            {
                                status.empty_message()
                            } else {
                                "No reviews match your search and filters."
                            },
                        )
                        .when(active_filters > 0, |s| {
                            s.child(
                                self.review_control(
                                    "no-results-reset",
                                    "Reset search and filters",
                                    false,
                                    None,
                                    Self::reset_review_filters_and_focus,
                                    cx,
                                )
                                .debug_selector(|| "no-results-reset".into()),
                            )
                        }),
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
        let pr = &self.engine.store().pending[ix];
        let url = pr.url.clone();
        let key = pr.key();
        let picker_open = self.snooze_picker.as_ref() == Some(&key);
        let snoozed_until = self.engine.store().snooze_for(pr).map(|snooze| {
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
                                pr.request_label(),
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
                    self.engine.store().snooze_minutes,
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
            .children(self.engine.store().roots.iter().map(|root| {
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

        let roots = roots.children(self.engine.scan_issues().iter().map(|issue| {
            div().text_xs().text_color(rgb(theme.warning)).child(format!(
                "{}: {}. Previously found repositories are retained; retrying on the next check.",
                display_path(&issue.path),
                issue.message
            ))
        }));

        let repos = self.engine.repos().unwrap_or_default();
        let list = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("GitHub repositories found", theme))
            .when(repos.is_empty() && !self.engine.scanning(), |s| {
                s.child(
                    div()
                        .text_color(rgb(theme.muted_text))
                        .child("No GitHub clones in these folders."),
                )
            })
            .children(repos.iter().enumerate().map(|(ix, repo)| {
                let enabled = self.engine.store().is_enabled(&repo.id.store_key());
                let slug = repo.id.store_key();
                let paths = repo
                    .paths
                    .iter()
                    .map(|p| display_path(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.keyboard
                    .control(Control::Repository(repo.id.clone()))
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_repo(&slug, cx)),
                    )
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
                            }),
                    )
                    .when(
                        repo.id.host == PUBLIC_HOST
                            && !self.engine.store().known_accounts.is_empty(),
                        |view| view.child(self.render_repo_accounts(&repo.id.slug, ix, theme, cx)),
                    )
                    .children(
                        self.engine
                            .repo_errors()
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
        _ix: usize,
        theme: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let all = self.engine.store().all_repo_accounts(repo);
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
                        self.button(
                            Control::AllRepositoryAccounts(repo.to_string()),
                            "All enabled accounts",
                            theme,
                        )
                        .when(all, |s| {
                            s.bg(rgb(theme.accent)).text_color(rgb(theme.on_accent))
                        })
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.all_repo_accounts(&slug, cx)
                            },
                        )),
                    )
                    .children(self.engine.store().known_accounts.iter().map(|account| {
                        let enabled = self.engine.store().repo_account_selected(repo, account);
                        let account = account.clone();
                        let slug = repo.to_string();
                        self.keyboard
                            .control(Control::RepositoryAccount(
                                repo.to_string(),
                                account.clone(),
                            ))
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
                                cx.stop_propagation();
                                this.toggle_repo_account(&slug, &account, cx)
                            }))
                    })),
            )
            .when(
                !all && !self
                    .engine
                    .store()
                    .known_accounts
                    .iter()
                    .any(|account| self.engine.store().repo_account_selected(repo, account)),
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
            .when(self.engine.store().known_accounts.is_empty(), |s| s.child(
                div().text_xs().text_color(rgb(theme.muted_text)).child("No saved accounts discovered yet.")))
            .children(self.engine.store().known_accounts.iter().map(|account| {
                let enabled = self.engine.store().account_enabled(account);
                let login = account.clone();
                let status = if !enabled {
                    "Monitoring off".into()
                } else if let Some(error) = self.engine.account_errors().get(account) {
                    error.clone()
                } else if self.engine.store().available_accounts.contains(account) {
                    "Monitoring github.com".into()
                } else {
                    "Waiting for a successful check".into()
                };
                div().flex().flex_col().gap_1().p_3().rounded_md().bg(rgb(theme.surface))
                    .child(div().flex().items_center().justify_between()
                        .child(format!("@{account}"))
                        .child(self.button(Control::Account(account.clone()), if enabled { "Disable" } else { "Enable" }, theme)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_account(&login, cx)))))
                    .child(div().text_xs().text_color(rgb(if enabled && self.engine.account_errors().contains_key(account) { theme.error } else { theme.secondary_text })).child(status))
            }))
    }

    fn render_settings(&self, theme: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.render_health(theme, cx))
            .child(self.render_accounts(theme, cx))
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
                                let active = self.engine.store().appearance == appearance;
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
                    .child(match &self.engine.login_state() {
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
                    .when(*self.engine.login_state() == login::State::NeedsApproval, |row| {
                        row.child(
                            div().text_xs().text_color(rgb(theme.warning)).child(
                                "Allow Octowatcher in System Settings → General → Login Items.",
                            ),
                        )
                    })
                    .when(*self.engine.login_state() == login::State::NeedsRepair, |row| {
                        row.child(
                            div().text_xs().text_color(rgb(theme.warning)).child(
                                "The startup command has changed. Repair it to start this copy at login.",
                            ),
                        )
                    })
                    .when_some(self.engine.login_error().map(str::to_owned), |row, error| {
                        row.child(div().text_xs().text_color(rgb(theme.error)).child(error))
                    }),
            )
            .child(self.render_choices(
                theme,
                ("Check GitHub for review requests every", Control::Poll),
                &POLL_CHOICES,
                self.engine.store().poll_minutes,
                Self::set_poll_minutes,
                cx,
            ))
            .child(self.render_choices(
                theme,
                ("Default snooze length", Control::SnoozeMinutes),
                &SNOOZE_CHOICES,
                self.engine.store().snooze_minutes,
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
                                    if self.engine.store().notifications_muted {
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
                                    .child(if self.engine.store().notifications_muted {
                                        "Muted · polling continues"
                                    } else {
                                        "On"
                                    }),
                            ),
                    )
                    .child(
                        self.button(
                            Control::NotifyDrafts,
                            if self.engine.store().notify_drafts {
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
    key: ReviewKey,
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

fn health_row(label: &'static str, detail: String, theme: Palette) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(section_title(label, theme))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.secondary_text))
                .child(detail),
        )
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
mod test_prelude {
    pub use std::collections::BTreeMap;

    pub use octowatcher_core::{
        discovery::{self, LocalRepo},
        engine::Done,
        health::SyncHealth,
        notifications::Response,
        review_notifications::Target,
        store::{PendingReview, Store},
    };
}

#[cfg(test)]
mod review_view_tests {
    use super::test_prelude::*;
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
        let flush_on_quit = cx.on_app_quit(|_, _| async {});
        let mut engine = Engine::settled(Store {
            pending: (1..=count)
                .rev()
                .map(|number| PendingReview {
                    host: repository::default_host(),
                    account: "alice".into(),
                    account_id: 1,
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
            sync_account: Some("test-viewer".into()),
            last_successful_sync: Some(Local::now().timestamp()),
            recovery_blocked: Some("test state must not be written".into()),
            available_accounts: BTreeSet::from(["alice".into()]),
            local_repos: BTreeSet::from([
                "acme/api".into(),
                "ghe.example.com/acme/api".into(),
                "github.example.com/owner/repo".into(),
            ]),
            ..Default::default()
        });
        engine.set_repos(Some(vec![LocalRepo {
            id: RepositoryId::new("github.com", "acme/api"),
            paths: Vec::new(),
        }]));
        *engine.health_mut() = SyncHealth {
            readiness: github::Readiness::Ready("alice".into()),
            verified: true,
            ..SyncHealth::default()
        };
        Octowatcher {
            keyboard,
            engine,
            jobs: HashMap::new(),
            next_job: 0,
            run_jobs: false,
            started: Vec::new(),
            tab: Tab::Reviews,
            review_filters: ReviewFilters::default(),
            review_filter_cache: ReviewFilterCache::default(),
            review_filter_scroll: ScrollHandle::default(),
            review_no_results_scroll: ScrollHandle::default(),
            review_control_focus: RefCell::default(),
            rendered_snooze_picker: None,
            review_search,
            review_filters_open: false,
            repository_picker_open: false,
            review_scroll: ListState::new(0, ListAlignment::Top, px(0.)),
            review_list_items: Vec::new(),
            snooze_picker: None,
            focus_handle,
            _search_subscription: subscription,
            review_filter_passes: 0,
            tray_error: None,
            tray: None,
            #[cfg(target_os = "linux")]
            restart_path: None,
            appearance_subscription: None,
            _flush_on_quit: flush_on_quit,
        }
    }

    #[gpui::test]
    fn dropdown_hides_controls_preserves_filters_and_resets_while_closed(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = fixture(cx, 10);
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        assert!(cx.debug_bounds("review-filter-controls").is_none());
        assert!(cx.debug_bounds("review:github.com/acme/api#10").is_some());
        let list_height = view.read_with(cx, |view, _| {
            view.keyboard.review_viewport.get().unwrap().size.height
        });
        let toggle = cx.debug_bounds("toggle-review-filters").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("review-filter-controls").is_some());
        view.read_with(cx, |view, _| {
            assert!(view.keyboard.review_viewport.get().unwrap().size.height < list_height);
        });

        cx.simulate_keystrokes(find_key());
        cx.simulate_input("#3");
        cx.simulate_keystrokes("tab tab tab tab enter"); // Ready.
        let filters = view.read_with(cx, |view, _| {
            assert_eq!(view.review_scroll.item_count(), 1);
            assert_eq!(view.review_filters.active_count(), 2);
            view.review_filters.clone()
        });
        cx.simulate_keystrokes(find_key());
        cx.simulate_keystrokes("tab enter"); // Open the repository picker.
        let toggle = cx.debug_bounds("toggle-review-filters").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.review_filters_open);
            assert!(!view.repository_picker_open);
            assert_eq!(view.review_filters, filters);
            assert_eq!(view.review_scroll.item_count(), 1);
            assert_eq!(view.engine.store().awake().len(), 10);
            assert!(!view.controls().contains(&Control::Search));
            assert_eq!(
                view.keyboard.review_viewport.get().unwrap().size.height,
                list_height
            );
        });
        cx.simulate_keystrokes("tab");
        cx.update(|window, cx| {
            assert_eq!(
                view.read(cx).keyboard.focused(window),
                Some(Control::Review(
                    view.read(cx).engine.store().pending[7].key()
                ))
            );
        });

        // Find reopens the dropdown and focuses the retained query.
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("no-match");
        let toggle = cx.debug_bounds("toggle-review-filters").unwrap();
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let reset = cx.debug_bounds("no-results-reset").unwrap();
        cx.simulate_click(reset.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.review_filters_open);
            assert!(
                !view
                    .controls()
                    .contains(&Control::Filter("no-results-reset".into()))
            );
            assert_eq!(view.review_filters, ReviewFilters::default());
            assert_eq!(view.review_scroll.item_count(), 10);
        });
        cx.update(|window, cx| {
            assert_eq!(
                view.read(cx).keyboard.focused(window),
                Some(Control::Filter("toggle-review-filters".into()))
            );
        });
    }

    #[gpui::test]
    fn filtered_cached_reviews_keep_the_offline_warning_and_health_navigation(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = fixture(cx, 2);
            view.engine.health_mut().verified = false;
            view.engine.health_mut().connection_failed = true;
            view.engine.health_mut().error = Some("Connection unavailable".into());
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        assert!(cx.debug_bounds("review-health-warning").is_some());
        let last_success = view.read_with(cx, |view, _| view.engine.store().last_successful_sync);
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("no-match");
        assert!(cx.debug_bounds("review-no-results").is_some());
        assert!(cx.debug_bounds("review-health-warning").is_some());
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_status(), ReviewsStatus::Offline);
            assert_eq!(view.engine.store().awake().len(), 2);
            assert_eq!(view.engine.store().last_successful_sync, last_success);
        });
        let settings = cx.debug_bounds("tab-Settings").unwrap();
        cx.simulate_click(settings.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("setup-health-panel").is_some());
        view.read_with(cx, |view, _| {
            assert!(view.tab == Tab::Settings);
            assert_eq!(view.review_filters.query, "no-match");
        });
    }

    #[gpui::test]
    fn empty_loading_and_offline_views_keep_health_in_settings_and_keyboard_reset(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(bind_review_keys);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = fixture(cx, 0);
            *view.engine.health_mut() = SyncHealth::default();
            view.engine.set_repos(None);
            view.engine.store_mut().last_successful_sync = None;
            window.focus(&view.focus_handle);
            view
        });
        cx.simulate_resize(size(px(560.), px(680.)));
        assert!(cx.debug_bounds("setup-health-panel").is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_status(), ReviewsStatus::Loading);
            assert!(!view.controls().contains(&Control::HealthRefresh));
        });
        view.update(cx, |view, cx| {
            view.engine.set_repos(Some(vec![LocalRepo {
                id: RepositoryId::new("github.com", "acme/api"),
                paths: Vec::new(),
            }]));
            view.engine.health_mut().readiness = github::Readiness::Offline;
            view.engine.health_mut().connection_failed = true;
            view.engine.health_mut().error = Some("Connection unavailable".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("setup-health-panel").is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_status(), ReviewsStatus::Offline);
            assert_eq!(view.engine.store().last_successful_sync, None);
            assert!(!view.controls().contains(&Control::HealthRefresh));
        });
        cx.simulate_keystrokes(find_key());
        cx.simulate_input("no-match");
        for _ in 0..12 {
            cx.simulate_keystrokes("tab");
        }
        let reset = cx.debug_bounds("no-results-reset").unwrap();
        let viewport = view.read_with(cx, |view, _| view.review_no_results_scroll.bounds());
        assert!(reset.top() >= viewport.top() && reset.bottom() <= viewport.bottom());
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters, ReviewFilters::default())
        });
        let settings = cx.debug_bounds("tab-Settings").unwrap();
        cx.simulate_click(settings.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("setup-health-panel").is_some());
        view.read_with(cx, |view, _| {
            assert!(view.tab == Tab::Settings);
            assert!(view.controls().contains(&Control::HealthRefresh));
        });
    }

    #[gpui::test]
    fn discovery_pruning_updates_filtered_rows_without_resetting_view_choices(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, visual) = cx.add_window_view(|_, cx| {
            let mut app = fixture(cx, 10);
            let key = app.engine.store().pending[7].key();
            app.engine.store_mut().snooze(&key, 120, 0);
            app.review_filters.query = "#3".into();
            app.review_filters.repository = Some(app.engine.store_mut().pending[7].repository());
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
            let roots = app.engine.store().roots.clone();
            app.dispatch(
                Done::Scanned {
                    roots,
                    result: Ok(discovery::ScanResult::default()),
                },
                cx,
            );
            assert!(app.engine.store().pending.is_empty());
        });
        visual.run_until_parked();
        view.read_with(visual, |app, _| {
            assert_eq!(app.review_scroll.item_count(), 0);
            assert!(app.review_filter_cache.visible_indices().is_empty());
            assert_eq!(app.review_filters, filters);
            assert!(app.engine.store().pending.is_empty());
            assert!(app.engine.store().snoozed.is_empty());
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
                ..view.engine.store_mut().pending[0].clone()
            };
            let enterprise = PendingReview {
                host: "ghe.example.com".into(),
                url: "https://ghe.example.com/Acme/API/pull/1".into(),
                ..public.clone()
            };
            view.engine.store_mut().pending = vec![public, enterprise];
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
                Some((1, "ghe.example.com/acme/api".into(), 1))
            );
        });
        // Snoozing the counterpart on github.com must not hide or close
        // the Enterprise review's picker. No disk/notification effects.
        view.update(cx, |view, cx| {
            let public = view.engine.store_mut().pending[0].key();
            view.engine.store_mut().snooze(&public, 5, 0);
            view.review_filter_cache.invalidate_snoozes();
            view.dismiss_stale_snooze_picker();
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.snooze_picker.is_some());
            assert_eq!(view.review_scroll.item_count(), 1);
            assert_eq!(view.engine.store().awake().len(), 1);
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
            view.apply(vec![Effect::ShowWindow], cx);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(cx.windows().len(), 1));
        view.read_with(cx, |view, _| assert!(view.tab == Tab::Settings));
        // An existing window is activated rather than duplicated.
        view.update(cx, |view, cx| {
            view.apply(vec![Effect::ShowWindow], cx);
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
                view.engine.store_mut().appearance = appearance;
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
                "filters must leave room for reviews: {viewport:?}"
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
            let store = view.engine.store_mut();
            for pr in &mut store.pending {
                pr.repo = format!("Acme/repo{:02}", pr.number);
                store.local_repos.insert(pr.repository().store_key());
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
        assert!(
            reset.bottom() <= px(280.),
            "{reset:?}; viewport {:?}, filters {:?}",
            view.read_with(cx, |view, _| view.review_no_results_scroll.bounds()),
            view.read_with(cx, |view, _| view.review_filter_scroll.bounds())
        );
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
            assert_eq!(
                view.review_filters
                    .visible_indices(view.engine.store())
                    .len(),
                10
            );
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
            assert_eq!(
                view.review_filters
                    .visible_indices(view.engine.store())
                    .len(),
                5
            )
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
            assert_eq!(
                view.review_filters.visible_indices(view.engine.store()),
                vec![7]
            );
        });
        cx.simulate_keystrokes("escape");
        cx.simulate_input("login");
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_filters.query, "login");
            assert_eq!(view.review_filters.draft, DraftFilter::All);
            assert_eq!(
                view.review_filters
                    .visible_indices(view.engine.store())
                    .len(),
                10
            );
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
            assert_eq!(
                view.review_filters.visible_indices(view.engine.store()),
                vec![7]
            )
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
            let mut fetched = view.engine.store_mut().pending.clone();
            fetched[7].title = "Updated after refresh".into();
            view.engine
                .store_mut()
                .reconcile(fetched, &BTreeMap::from([("alice".into(), 1)]));
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
            assert_eq!(
                view.review_filters.visible_indices(view.engine.store()),
                vec![6]
            );
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
            assert_eq!(view.snooze_picker, Some((1, "acme/api".into(), 3)))
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
            assert!(
                view.review_filters
                    .visible_indices(view.engine.store())
                    .is_empty()
            );
            assert_eq!(view.engine.store().awake().len(), 10);
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
            let mut new = view.engine.store_mut().pending[0].clone();
            new.number = 101;
            view.engine.store_mut().pending.insert(0, new);
            view.review_filter_cache.invalidate_reviews();
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.review_scroll.logical_scroll_top().item_ix, 11);
            assert_eq!(view.engine.store().pending[11].number, 90);
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
                view.review_filters.visible_indices(view.engine.store()),
                vec![9_999]
            )
        });
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.review_filters
                    .visible_indices(view.engine.store())
                    .len(),
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
                view.review_filters
                    .visible_indices(view.engine.store())
                    .len(),
                10_000
            );
        });
        cx.simulate_input("#2");
        view.read_with(cx, |view, _| assert_eq!(view.review_filters.query, "#2"));
    }
}

#[cfg(test)]
mod snooze_tests {
    use super::test_prelude::*;
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
    fn app_for_picker_test(cx: &mut Context<Octowatcher>) -> Octowatcher {
        let mut app = review_view_tests::fixture(cx, 2);
        app.engine.store_mut().pending = vec![review(1), review(2)];
        app.engine.store_mut().local_repos = BTreeSet::from(["owner/repo".into()]);
        app.engine.set_repos(Some(vec![LocalRepo {
            id: RepositoryId::new("github.com", "owner/repo"),
            paths: Vec::new(),
        }]));
        app.engine.set_announced_launch(true);
        app.engine
            .store_mut()
            .available_hosts
            .insert(repository::default_host());
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
                    app.engine.store_mut().appearance = appearance;
                    app.engine.store_mut().notifications_muted = muted;
                    app.engine.store_mut().notify_drafts = !muted;
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
    fn all_hidden_cached_accounts_stay_unavailable_with_health_in_settings(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let mut app = app_for_picker_test(cx);
            app.engine.store_mut().available_accounts.clear();
            app.engine.store_mut().stale_accounts.clear();
            app.engine.health_mut().readiness = github::Readiness::SignedOut;
            app.engine.health_mut().verified = false;
            app.engine.health_mut().error = Some("Not signed in".into());
            app
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("setup-health-panel").is_none());
        view.read_with(cx, |app, _| {
            assert_eq!(app.engine.store().pending.len(), 2);
            assert_eq!(app.review_status(), ReviewsStatus::Unavailable);
            assert!(!app.controls().contains(&Control::CopyLogin));
        });
        click(cx, "tab-Settings");
        assert!(cx.debug_bounds("setup-health-panel").is_some());
        view.read_with(cx, |app, _| {
            assert!(app.tab == Tab::Settings);
            assert!(app.controls().contains(&Control::CopyLogin));
        });
    }

    #[gpui::test]
    fn picker_tracks_the_pr_key_and_closes_when_the_review_disappears_or_is_snoozed(
        cx: &mut TestAppContext,
    ) {
        let app = cx.new(app_for_picker_test);
        app.update(cx, |app, _| {
            let key = review(1).key();
            app.snooze_picker = Some(key.clone());
            app.engine.store_mut().pending.reverse();
            app.dismiss_stale_snooze_picker();
            assert_eq!(app.snooze_picker, Some(key.clone()));
            app.engine.store_mut().snooze(&key, 30, 1_000);
            app.dismiss_stale_snooze_picker();
            assert_eq!(app.snooze_picker, None);
            app.snooze_picker = Some(key);
            app.engine.store_mut().pending.retain(|pr| pr.number != 1);
            app.dismiss_stale_snooze_picker();
            assert_eq!(app.snooze_picker, None);
        });
    }

    #[gpui::test]
    fn health_navigation_dismisses_the_snooze_picker(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| app_for_picker_test(cx));
        for appearance in [Appearance::Light, Appearance::Dark] {
            view.update(cx, |app, cx| {
                app.engine.store_mut().appearance = appearance;
                app.tab = Tab::Reviews;
                cx.notify();
            });
            cx.run_until_parked();
            click(cx, "snooze-0");
            assert!(view.read_with(cx, |v, _| v.snooze_picker.is_some()));
            click(cx, "tab-Settings");
            assert!(view.read_with(cx, |v, _| v.tab == Tab::Settings
                && v.snooze_picker.is_none()));
            assert_eq!(cx.opened_url(), None);
        }
    }

    #[gpui::test]
    fn discovery_pruning_dismisses_the_removed_review_picker(cx: &mut TestAppContext) {
        let app = cx.new(app_for_picker_test);
        app.update(cx, |app, cx| {
            app.snooze_picker = Some(review(1).key());
            let roots = app.engine.store().roots.clone();
            app.dispatch(
                Done::Scanned {
                    roots,
                    result: Ok(discovery::ScanResult::default()),
                },
                cx,
            );
            assert!(app.started.contains(&JobKey::Fetch));
            assert!(app.engine.store().pending.is_empty());
            assert!(app.snooze_picker.is_none());
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
            && v.engine.store().snoozed.is_empty()));
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
        let (view, cx) = cx.add_window_view(|_, cx| {
            let mut app = app_for_picker_test(cx);
            app.engine
                .store_mut()
                .local_repos
                .insert("github.example.com/owner/repo".into());
            app.engine.store_mut().pending = vec![public.clone(), enterprise];
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
            v.engine.store_mut().snooze(&public.key(), 5, 1_000);
            v.dismiss_stale_snooze_picker();
            assert_eq!(v.snooze_picker, Some(enterprise_key));
        });
        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    fn shared_pr_picker_tracks_the_receiving_account_and_closes_when_unavailable(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| {
            let mut app = app_for_picker_test(cx);
            let alice = review(1);
            let mut bob = alice.clone();
            bob.account = "bob".into();
            bob.account_id = 2;
            app.engine.store_mut().pending = vec![alice.clone(), bob.clone()];
            app.engine
                .store_mut()
                .available_accounts
                .insert("bob".into());
            app
        });
        let alice = review(1);
        let mut bob = alice.clone();
        bob.account = "bob".into();
        bob.account_id = 2;
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
            v.engine.store_mut().available_accounts.remove("bob");
            v.review_filter_cache.invalidate_reviews();
            v.dismiss_stale_snooze_picker();
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |v, _| {
            assert!(v.snooze_picker.is_none());
            assert_eq!(v.engine.store().pending.len(), 2);
            assert_eq!(v.engine.store().visible_pending(), vec![alice]);
        });
        assert_eq!(cx.opened_url(), None);
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
                .set_palette(theme::palette(self.store.appearance, window.appearance()));
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
                    theme::palette(self.store.appearance, window.appearance()),
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
