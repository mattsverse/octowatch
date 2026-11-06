//! The window's state: the engine, what the window shows, and the shell work the engine asks
//! for (jobs, the tray, URLs, dialogs). Views live in `ui`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use octowatcher_core::{
    appearance::Appearance,
    engine::{self, Command, Effect, Engine, Prompt, Update},
    health::ReviewsStatus,
    repository::RepositoryId,
    review_filter::{ReviewFilterCache, ReviewFilters},
    store::{PendingReview, ReviewKey},
};
use rune::{AppHandle, TextEdit, Tx};

use crate::jobs::Jobs;
use crate::tray::{Tray, UpdateItem};

/// Choices offered in Settings for minutes between GitHub checks.
pub const POLL_CHOICES: [u64; 7] = [1, 2, 5, 10, 15, 30, 60];
/// Choices offered in Settings and for an individual review's snooze.
pub const SNOOZE_CHOICES: [u64; 6] = [5, 10, 15, 30, 60, 120];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tab {
    Reviews,
    Repositories,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Reviews, Tab::Repositories, Tab::Settings];
}

/// The one pop-up open in the window. Never persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Popup {
    /// A review's snooze durations.
    Snooze(ReviewKey),
    /// The repository filter's choices.
    Repository,
    PollInterval,
    SnoozeDefault,
    /// The accounts a repository is monitored with, by its store key.
    RepoAccounts(String),
}

/// Shell work that needs the handler's `Tx`, run once one is at hand.
enum Outbox {
    OpenUrl(String),
}

pub struct Octowatcher {
    pub engine: Engine,
    pub jobs: Jobs,
    pub tab: Tab,
    pub filters: ReviewFilters,
    /// Filtering is cached across frames; the view refreshes it before reading.
    filter_cache: RefCell<ReviewFilterCache>,
    pub search: TextEdit,
    pub filters_open: bool,
    pub popup: Option<Popup>,
    /// A dialog the engine asked for, shown over the window.
    pub prompt: Option<Prompt>,
    pub tray: Option<Tray>,
    pub tray_error: Option<String>,
    pub handle: Option<AppHandle<Octowatcher>>,
    pub restart_path: Option<PathBuf>,
    /// Set while a folder picker is open, so a second click doesn't stack another.
    pub picking_folder: bool,
    /// The last time a login command was copied, for the button's confirmation.
    pub copied_login: bool,
    /// The app icon, for alerts.
    pub app_icon: rune::Pixels,
    /// Shows every setup check even when all of them pass.
    pub health_expanded: bool,
    /// The update banner was put off for this session; the status bar still offers it.
    pub update_banner_dismissed: bool,
    outbox: Vec<Outbox>,
}

impl Octowatcher {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            jobs: Jobs::default(),
            tab: Tab::Reviews,
            filters: ReviewFilters::default(),
            filter_cache: RefCell::default(),
            search: TextEdit::new(""),
            filters_open: false,
            popup: None,
            prompt: None,
            tray: None,
            tray_error: None,
            handle: None,
            restart_path: None,
            picking_folder: false,
            copied_login: false,
            health_expanded: false,
            app_icon: app_icon(),
            update_banner_dismissed: false,
            outbox: Vec::new(),
        }
    }

    /// Starts discovery, polling and the permission check. Runs on the UI thread once the
    /// event loop is up, whether or not a window shows.
    pub fn start(&mut self, tx: &mut Tx) {
        let effects = self.engine.start();
        self.apply(effects);
        self.flush(tx);
    }

    /// Hands an event to the engine and carries out what it asks for.
    pub fn dispatch(&mut self, event: impl Into<engine::Event>, tx: &mut Tx) {
        let effects = self.engine.handle(event);
        self.apply(effects);
        self.flush(tx);
    }

    pub fn command(&mut self, command: Command, tx: &mut Tx) {
        self.dispatch(command, tx);
    }

    /// Lands finished jobs. True when anything changed.
    pub fn pump(&mut self, tx: &mut Tx) -> bool {
        let events = self.jobs.finished();
        let changed = !events.is_empty();
        for event in events {
            self.dispatch(event, tx);
        }
        changed
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Start(job) => self.jobs.start(job),
                Effect::Cancel(key) => self.jobs.cancel(key),
                Effect::SyncTray => self.sync_tray(),
                Effect::ReviewsChanged => self.filter_cache.get_mut().invalidate_reviews(),
                Effect::SnoozesChanged => self.filter_cache.get_mut().invalidate_snoozes(),
                Effect::OpenUrl(url) => self.outbox.push(Outbox::OpenUrl(url)),
                Effect::ShowReviews => {
                    self.tab = Tab::Reviews;
                    self.popup = None;
                    self.show_window();
                }
                Effect::ShowWindow => self.show_window(),
                Effect::Prompt(prompt) => {
                    self.prompt = Some(prompt);
                    self.show_window();
                }
                Effect::SetRestartPath(path) => self.restart_path = Some(path),
            }
        }
        self.dismiss_stale_popup();
    }

    fn flush(&mut self, tx: &mut Tx) {
        for item in std::mem::take(&mut self.outbox) {
            match item {
                Outbox::OpenUrl(url) => open_url(&url, tx),
            }
        }
    }

    /// Brings the window to the front, opening it again if it was closed.
    pub fn show_window(&mut self) {
        let effects = self.engine.handle(Command::RefreshLoginStatus);
        self.apply(effects);
        if let Some(handle) = &self.handle {
            handle.open();
        }
    }

    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.popup = None;
    }

    pub fn toggle_popup(&mut self, popup: Popup) {
        self.popup = if self.popup.as_ref() == Some(&popup) {
            None
        } else {
            Some(popup)
        };
    }

    pub fn snooze_for(&mut self, key: ReviewKey, minutes: u64, tx: &mut Tx) {
        if self.popup == Some(Popup::Snooze(key.clone())) {
            self.popup = None;
        }
        self.command(Command::SnoozeFor(key, minutes), tx);
    }

    /// A pop-up for a review that left the list, or was snoozed elsewhere, closes.
    fn dismiss_stale_popup(&mut self) {
        let store = self.engine.store();
        if let Some(Popup::Snooze(key)) = &self.popup
            && !store
                .pending
                .iter()
                .any(|pr| pr.key() == *key && store.visible(pr) && store.snooze_for(pr).is_none())
        {
            self.popup = None;
        }
    }

    // Search and filters.

    pub fn filters_changed(&mut self) {
        // A hidden card must not keep an invisible duration picker open.
        if matches!(self.popup, Some(Popup::Snooze(_))) {
            self.popup = None;
        }
    }

    /// Follows the search field's text into the filters.
    pub fn search_edited(&mut self) {
        if self.filters.query != self.search.text {
            self.filters.query = self.search.text.clone();
            self.filters_changed();
        }
    }

    pub fn reset_filters(&mut self) {
        self.filters = ReviewFilters::default();
        self.search = TextEdit::new("");
        if self.popup == Some(Popup::Repository) {
            self.popup = None;
        }
        self.filters_changed();
    }

    pub fn focus_search(&mut self) {
        self.select_tab(Tab::Reviews);
        self.filters_open = true;
        self.search.focused = true;
    }

    /// Indices into the pending list of the reviews that pass the filters, in queue order.
    pub fn visible_indices(&self) -> Rc<Vec<usize>> {
        let mut cache = self.filter_cache.borrow_mut();
        cache.refresh(self.engine.store(), &self.filters);
        cache.visible_indices()
    }

    /// Repositories the filter offers, with their labels.
    pub fn filter_repositories(&self) -> Vec<(RepositoryId, String)> {
        let mut cache = self.filter_cache.borrow_mut();
        cache.refresh(self.engine.store(), &self.filters);
        cache.repositories(&self.filters)
    }

    /// The visible reviews, in queue order.
    pub fn visible_reviews(&self) -> impl Iterator<Item = &PendingReview> {
        let store = self.engine.store();
        store.pending.iter().filter(|pr| store.visible(pr))
    }

    pub fn total_reviews(&self) -> usize {
        self.visible_reviews().count()
    }

    /// The owner's place among the owners of discovered repositories, for its colour.
    pub fn owner_slot(&self, owner: &str) -> Option<usize> {
        let mut owners: Vec<String> = self
            .engine
            .repos()?
            .iter()
            .filter_map(|repo| repo.id.slug.split_once('/').map(|(o, _)| o.to_lowercase()))
            .collect();
        owners.sort();
        owners.dedup();
        owners.iter().position(|o| *o == owner.to_lowercase())
    }

    // Status.

    pub fn errors(&self) -> Vec<&str> {
        self.engine.errors(self.tray_error.as_deref())
    }

    pub fn review_status(&self) -> ReviewsStatus {
        self.engine.review_status()
    }

    pub fn needs_health(&self) -> bool {
        use octowatcher_core::notifications::Permission;
        self.review_status() != ReviewsStatus::Ready
            || !self.errors().is_empty()
            || matches!(self.engine.permission(), Permission::Denied)
            || (cfg!(target_os = "macos")
                && matches!(self.engine.permission(), Permission::Unknown(_)))
    }

    pub fn needs_login(&self) -> bool {
        use octowatcher_core::github::Readiness;
        let health = self.engine.health();
        matches!(health.readiness, Readiness::Missing | Readiness::SignedOut)
            || health
                .hosts
                .values()
                .any(|ready| matches!(ready, Readiness::SignedOut))
    }

    pub fn login_host(&self) -> &str {
        use octowatcher_core::github::Readiness;
        self.engine
            .health()
            .hosts
            .iter()
            .find(|(_, readiness)| matches!(readiness, Readiness::SignedOut))
            .map(|(host, _)| host.as_str())
            .unwrap_or("github.com")
    }

    pub fn set_appearance(&mut self, appearance: Appearance, tx: &mut Tx) {
        // The whole window recolours; let the colours glide rather than jump.
        tx.color_motion(rune::tok::motion::CALM);
        self.command(Command::SetAppearance(appearance), tx);
    }

    // Folders.

    pub fn add_root(&mut self, tx: &mut Tx) {
        if self.picking_folder {
            return;
        }
        self.picking_folder = true;
        let options = rune::FileDialogOptions {
            title: "Watch folder".into(),
            ..Default::default()
        };
        tx.open_folder(options, |s: &mut Octowatcher, result, tx| {
            s.picking_folder = false;
            match result {
                Ok(Some(path)) => s.command(Command::AddRoots(vec![path]), tx),
                Ok(None) => {}
                Err(err) => eprintln!("could not open the folder picker: {err}"),
            }
        });
    }

    // Updates and the tray.

    pub fn sync_tray(&mut self) {
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

    /// Writes unsaved state, then quits.
    pub fn quit(&mut self, tx: &mut Tx) {
        self.engine.flush();
        tx.quit();
    }

    /// Restarts after this process exits, keeping instance ownership until then.
    pub fn restart(&mut self, tx: &mut Tx) {
        let path = self
            .restart_path
            .clone()
            .map(Ok)
            .unwrap_or_else(octowatcher_core::updater::running_executable);
        match path.and_then(|path| octowatcher_core::updater::relaunch(&path)) {
            Ok(()) => self.quit(tx),
            Err(err) => eprintln!("could not restart Octowatcher: {err:#}"),
        }
    }

    /// Carries out the answer to the open dialog: `accept` is its primary button.
    pub fn answer_prompt(&mut self, accept: bool, tx: &mut Tx) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        if !accept {
            return;
        }
        match prompt {
            Prompt::UpToDate | Prompt::CheckFailed(_) => {}
            Prompt::Available(_) => self.command(Command::InstallUpdate, tx),
            Prompt::Download(release) | Prompt::InstallFailed { release, .. } => {
                open_url(&release.url, tx)
            }
            Prompt::Restart(_) => self.restart(tx),
        }
    }
}

/// The bundled 128 px app icon, decoded once.
fn app_icon() -> rune::Pixels {
    const PNG: &[u8] = include_bytes!("../../../assets/icons/128x128.png");
    let pixels = rune::Pixels::new();
    let mut decoder = png::Decoder::new(std::io::Cursor::new(PNG));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    if let Ok(mut reader) = decoder.read_info()
        && let Some(size) = reader.output_buffer_size()
    {
        let mut rgba = vec![0; size];
        if let Ok(info) = reader.next_frame(&mut rgba)
            && info.color_type == png::ColorType::Rgba
        {
            rgba.truncate(info.buffer_size());
            pixels.set(info.width, info.height, rgba);
        }
    }
    pixels
}

pub fn open_url(url: &str, tx: &mut Tx) {
    if let Err(err) = tx.open_url(url) {
        eprintln!("could not open {url}: {err}");
    }
}

pub fn minutes_label(minutes: u64) -> String {
    if minutes < 60 || !minutes.is_multiple_of(60) {
        format!("{minutes} min")
    } else if minutes == 60 {
        "1 hour".into()
    } else {
        format!("{} hours", minutes / 60)
    }
}

/// Durations offered for one review: the presets, plus a default saved by an older build.
pub fn snooze_duration_choices(default_minutes: u64) -> impl Iterator<Item = u64> {
    SNOOZE_CHOICES
        .into_iter()
        .chain((!SNOOZE_CHOICES.contains(&default_minutes)).then_some(default_minutes))
}

pub fn display_path(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}
