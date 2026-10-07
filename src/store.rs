use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::repository::{RepositoryId, default_host};
use crate::theme::Appearance;

/// Everything that survives a restart, saved as JSON in the platform config dir.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    /// Folders scanned for local git clones.
    pub roots: Vec<PathBuf>,
    /// Host-qualified repository identities the user switched off.
    pub disabled: BTreeSet<RepositoryId>,
    /// Pull requests currently waiting on the user's review.
    pub pending: Vec<PendingReview>,
    /// Minutes between checks of GitHub for review requests.
    pub poll_minutes: u64,
    /// Default minutes a snoozed review stays hidden.
    pub snooze_minutes: u64,
    /// Reviews the user put aside for now.
    pub snoozed: Vec<Snooze>,
    /// Account the cached reviews and snoozes belong to. Older files omit it.
    pub sync_account: Option<String>,
    /// Active account per host; legacy sync_account belongs to github.com.
    pub sync_accounts: BTreeMap<String, String>,
    /// Unix seconds of the last complete, successful review sync.
    pub last_successful_sync: Option<i64>,
    /// Prevent replacement of a state file that couldn't be read or preserved.
    #[serde(skip)]
    pub(crate) recovery_blocked: Option<String>,
    /// Review alerts are muted; polling and the queue stay active.
    pub notifications_muted: bool,
    /// Drafts stay in the queue regardless of this delivery preference.
    pub notify_drafts: bool,
    /// Review events not yet accepted by the desktop notification service.
    pub notification_queue: Vec<ReviewNotice>,
    /// Distinguishes a snooze reminder from an earlier delivery of the same request.
    pub notification_sequence: u64,
    /// System appearance or a persistent light/dark override.
    pub appearance: Appearance,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            disabled: BTreeSet::new(),
            pending: Vec::new(),
            poll_minutes: 2,
            snooze_minutes: 5,
            snoozed: Vec::new(),
            sync_account: None,
            sync_accounts: BTreeMap::new(),
            last_successful_sync: None,
            recovery_blocked: None,
            notifications_muted: false,
            notify_drafts: true,
            notification_queue: Vec::new(),
            notification_sequence: 0,
            appearance: Appearance::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
    #[serde(default = "default_host")]
    pub host: String,
    /// `owner/name` as GitHub spells it.
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub is_draft: bool,
    /// True when the user already reviewed and was asked again.
    pub rereview: bool,
    /// When the latest review request for the user landed (RFC 3339).
    pub requested_at: Option<String>,
}

impl PendingReview {
    pub fn repository(&self) -> RepositoryId {
        RepositoryId::new(&self.host, &self.repo)
    }

    pub fn key(&self) -> (RepositoryId, u64) {
        (self.repository(), self.number)
    }

    pub fn repo_label(&self) -> String {
        format!("{}/{}", self.host, self.repo)
    }
}

/// A delivery event, independently persisted from the current review list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewNotice {
    #[serde(default = "default_host")]
    pub host: String,
    pub repo: String,
    pub number: u64,
    pub requested_at: Option<String>,
    pub sequence: u64,
}

impl ReviewNotice {
    pub fn key(&self) -> (RepositoryId, u64) {
        (RepositoryId::new(&self.host, &self.repo), self.number)
    }

    pub fn matches(&self, pr: &PendingReview) -> bool {
        self.key() == pr.key() && self.requested_at == pr.requested_at
    }
}

/// A pending review hidden from the list and the tray until `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snooze {
    #[serde(default = "default_host")]
    pub host: String,
    /// `owner/name`, lowercase.
    pub repo: String,
    pub number: u64,
    /// Unix seconds when the review comes back.
    pub until: i64,
    /// The request that was snoozed; a newer one ends the snooze.
    pub requested_at: Option<String>,
}

impl Snooze {
    pub fn key(&self) -> (RepositoryId, u64) {
        (RepositoryId::new(&self.host, &self.repo), self.number)
    }
}

/// What `Store::reconcile` changed, so the caller knows which side effects to run.
#[derive(Debug, PartialEq, Eq)]
pub struct Reconciled {
    /// Reviews requested for the first time, or requested again.
    pub fresh: Vec<PendingReview>,
    pub pending_changed: bool,
    pub snoozes_changed: bool,
    pub notifications_changed: bool,
}

impl Store {
    fn path() -> Result<PathBuf> {
        let dir = dirs::config_dir().context("no config directory on this platform")?;
        Ok(dir.join("octowatcher").join("state.json"))
    }

    pub fn load() -> (Self, Option<String>) {
        match Self::path() {
            Ok(path) => Self::load_from(&path),
            Err(err) => Self::load_failed(format!("{err:#}")),
        }
    }

    fn initial() -> Self {
        Self {
            roots: default_roots(),
            ..Self::default()
        }
    }

    fn load_failed(error: String) -> (Self, Option<String>) {
        let message = format!(
            "Could not load saved state: {error}. Defaults are in use; saving is paused to protect the original. Fix the file or its permissions, then restart Octowatcher."
        );
        let mut store = Self::initial();
        store.recovery_blocked = Some(message.clone());
        (store, Some(message))
    }

    fn load_from(path: &std::path::Path) -> (Self, Option<String>) {
        let json = match fs::read(path) {
            Ok(json) => json,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return (Self::initial(), None);
            }
            Err(err) => return Self::load_failed(format!("{}: {err}", path.display())),
        };
        match serde_json::from_slice(&json) {
            Ok(store) => (store, None),
            Err(err) => {
                // A unique backup is kept before defaults can ever be saved.
                let backup = (|| -> Result<PathBuf> {
                    let parent = path.parent().context("state file has no parent")?;
                    let file = tempfile::Builder::new()
                        .prefix("state-recovery-")
                        .suffix(".json")
                        .tempfile_in(parent)?;
                    fs::write(file.path(), &json)?;
                    Ok(file.keep()?.1)
                })();
                match backup {
                    Ok(backup) => (
                        Self::initial(),
                        Some(format!(
                            "Saved state was invalid ({err}). Defaults are in use. Original contents preserved at {}. Review your watched folders and settings; restore the backup to state.json and restart if needed.",
                            backup.display()
                        )),
                    ),
                    Err(backup_err) => Self::load_failed(format!(
                        "{} is invalid ({err}); could not preserve it: {backup_err:#}",
                        path.display()
                    )),
                }
            }
        }
    }

    /// Cached data from an unknown (legacy) or different account cannot be
    /// assigned to the current account, even when its next review query fails.
    #[cfg(test)]
    pub fn activate_account(&mut self, login: &str) -> bool {
        self.activate_host_account("github.com", login)
    }

    pub fn activate_host_account(&mut self, host: &str, login: &str) -> bool {
        let previous = self.sync_accounts.get(host).or_else(|| {
            (host == "github.com")
                .then_some(self.sync_account.as_ref())
                .flatten()
        });
        let changed = !previous.is_some_and(|previous| previous.eq_ignore_ascii_case(login));
        let saved_login = if changed {
            login.to_string()
        } else {
            previous.cloned().unwrap()
        };
        self.sync_accounts
            .insert(host.to_string(), saved_login.clone());
        if host == "github.com" {
            self.sync_account = Some(saved_login);
        }
        if changed {
            self.pending.retain(|pr| pr.repository().host != host);
            self.snoozed.retain(|pr| pr.key().0.host != host);
            self.notification_queue.retain(|pr| pr.key().0.host != host);
            self.last_successful_sync = None;
        }
        changed
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(error) = &self.recovery_blocked {
            bail!("{error}");
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Write then rename so a crash mid-write never leaves a truncated file.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn is_enabled(&self, repository: &RepositoryId) -> bool {
        !self.disabled.contains(repository)
    }

    pub fn snooze_for(&self, pr: &PendingReview) -> Option<&Snooze> {
        let key = pr.key();
        self.snoozed.iter().find(|snooze| snooze.key() == key)
    }

    /// Snoozes only this PR for the chosen duration, leaving the default alone.
    pub fn snooze(&mut self, key: &(RepositoryId, u64), minutes: u64, now: i64) -> bool {
        let Some(pr) = self.pending.iter().find(|pr| pr.key() == *key) else {
            return false;
        };
        let snooze = Snooze {
            host: key.0.host.clone(),
            repo: key.0.slug.clone(),
            number: key.1,
            until: now + minutes as i64 * 60,
            requested_at: pr.requested_at.clone(),
        };
        self.snoozed.retain(|s| s.key() != *key);
        self.snoozed.push(snooze);
        self.discard_notification(key);
        true
    }

    /// Brings a PR back without treating it as an expired snooze.
    pub fn unsnooze(&mut self, key: &(RepositoryId, u64)) {
        self.snoozed.retain(|s| s.key() != *key);
        self.discard_notification(key);
    }

    pub fn next_snooze_until(&self) -> Option<i64> {
        self.snoozed.iter().map(|s| s.until).min()
    }

    /// Pending reviews that aren't snoozed.
    pub fn awake(&self) -> Vec<PendingReview> {
        self.pending
            .iter()
            .filter(|pr| self.snooze_for(pr).is_none())
            .cloned()
            .collect()
    }

    /// Retains at most one undelivered event per PR. Re-enqueuing a reminder
    /// advances its generation so an older in-flight send cannot consume it.
    pub fn queue_notifications(&mut self, prs: &[PendingReview]) {
        for pr in prs {
            self.discard_notification(&pr.key());
            self.notification_sequence = self.notification_sequence.wrapping_add(1);
            self.notification_queue.push(ReviewNotice {
                host: pr.repository().host,
                repo: pr.repo.to_lowercase(),
                number: pr.number,
                requested_at: pr.requested_at.clone(),
                sequence: self.notification_sequence,
            });
        }
    }

    pub fn discard_notification(&mut self, key: &(RepositoryId, u64)) {
        self.notification_queue
            .retain(|notice| notice.key() != *key);
    }

    /// Resolved/disabled requests also leave the delivery queue. Titles and
    /// draft status are read from the current review list at each attempt.
    pub fn prune_notifications(&mut self) {
        self.notification_queue
            .retain(|notice| self.pending.iter().any(|pr| notice.matches(pr)));
    }

    pub fn notifications_due(&self) -> Vec<(ReviewNotice, PendingReview)> {
        if self.notifications_muted {
            return Vec::new();
        }
        self.pending
            .iter()
            .filter_map(|pr| {
                if self.snooze_for(pr).is_some() || (pr.is_draft && !self.notify_drafts) {
                    return None;
                }
                let notice = self
                    .notification_queue
                    .iter()
                    .find(|notice| notice.matches(pr))?;
                Some((notice.clone(), pr.clone()))
            })
            .collect()
    }

    /// Delivery is acknowledged on OS acceptance, before any user interaction.
    pub fn mark_delivered(&mut self, notices: &[ReviewNotice]) {
        self.notification_queue
            .retain(|notice| !notices.contains(notice));
    }

    /// Replaces the pending list with what GitHub reports now, newest first.
    /// A PR that drops out (reviewed, request removed, closed) is gone, and
    /// so is its snooze.
    pub fn reconcile(&mut self, mut fetched: Vec<PendingReview>) -> Reconciled {
        let known: HashMap<_, _> = self
            .pending
            .iter()
            .map(|pr| (pr.key(), pr.requested_at.clone()))
            .collect();
        // A missing timestamp on a still-pending PR is not evidence of a new
        // request; keep its known identity, undelivered notice, and snooze deadline.
        for pr in &mut fetched {
            if pr.requested_at.is_none() {
                pr.requested_at = known.get(&pr.key()).cloned().flatten();
            }
        }
        fetched.sort_by(|a, b| b.requested_at.cmp(&a.requested_at));

        // A request is new when the PR wasn't listed, or when it was asked
        // again after the request already on file.
        let fresh: Vec<PendingReview> = fetched
            .iter()
            .filter(|pr| match known.get(&pr.key()) {
                None => true,
                Some(previous) => pr.requested_at > *previous,
            })
            .cloned()
            .collect();

        // A snooze ends when its PR leaves the list or is requested again.
        let snoozed = self.snoozed.len();
        self.snoozed.retain(|snooze| {
            fetched
                .iter()
                .any(|pr| pr.key() == snooze.key() && pr.requested_at == snooze.requested_at)
        });
        let snoozes_changed = self.snoozed.len() != snoozed;

        let pending_changed = fetched != self.pending;
        self.pending = fetched;
        let notifications_before = self.notification_queue.clone();
        self.prune_notifications();
        self.queue_notifications(&fresh);
        let notifications_changed = self.notification_queue != notifications_before;
        Reconciled {
            fresh,
            pending_changed,
            snoozes_changed,
            notifications_changed,
        }
    }

    /// Add confirmed requests even from incomplete checks. Only complete
    /// snapshots can remove requests; incomplete checks cannot regress saved
    /// timestamps or end snoozes without evidence of a newer request.
    /// Repositories no longer watched still leave the queue.
    pub fn reconcile_repositories(
        &mut self,
        fetched: Vec<PendingReview>,
        completed: &HashSet<RepositoryId>,
        watched: &HashSet<RepositoryId>,
    ) -> Reconciled {
        let known: HashMap<_, _> = self.pending.iter().map(|pr| (pr.key(), pr)).collect();
        let mut pending: Vec<_> = fetched
            .into_iter()
            .filter(|pr| watched.contains(&pr.repository()))
            .map(|pr| {
                if !completed.contains(&pr.repository())
                    && let Some(previous) = known.get(&pr.key())
                    && previous.requested_at > pr.requested_at
                {
                    return (*previous).clone();
                }
                pr
            })
            .collect();
        let confirmed: HashSet<_> = pending.iter().map(PendingReview::key).collect();
        pending.extend(
            self.pending
                .iter()
                .filter(|pr| {
                    let repo = pr.repository();
                    watched.contains(&repo)
                        && !completed.contains(&repo)
                        && !confirmed.contains(&pr.key())
                })
                .cloned(),
        );
        self.reconcile(pending)
    }

    /// Ends every snooze that ran out by `now`, and returns the pending
    /// reviews they hid.
    pub fn take_expired(&mut self, now: i64) -> Vec<PendingReview> {
        let (expired, remaining): (Vec<Snooze>, Vec<Snooze>) = std::mem::take(&mut self.snoozed)
            .into_iter()
            .partition(|s| s.until <= now);
        self.snoozed = remaining;
        let woken: Vec<_> = self
            .pending
            .iter()
            .filter(|pr| expired.iter().any(|s| s.key() == pr.key()))
            .cloned()
            .collect();
        self.queue_notifications(&woken);
        woken
    }
}

fn default_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let dev = home.join("Dev");
    vec![if dev.is_dir() { dev } else { home }]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{PendingReview, Snooze, Store};
    use crate::repository::{RepositoryId, default_host};
    use crate::theme::Appearance;

    #[test]
    fn legacy_settings_default_to_system_without_losing_state() {
        let original = Store {
            roots: vec!["/projects".into()],
            disabled: [RepositoryId::new("github.com", "o/off")].into(),
            pending: vec![pr("o/r", 1, Some(T1))],
            snoozed: vec![snooze("o/r", 1, Some(T1))],
            poll_minutes: 15,
            snooze_minutes: 30,
            ..Store::default()
        };
        let mut legacy = serde_json::to_value(&original).unwrap();
        legacy.as_object_mut().unwrap().remove("appearance");
        let restored: Store = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.appearance, Appearance::System);
        assert_eq!(restored.roots, original.roots);
        assert_eq!(restored.disabled, original.disabled);
        assert_eq!(restored.pending, original.pending);
        assert_eq!(restored.snoozed, original.snoozed);
        assert_eq!(restored.poll_minutes, 15);
        assert_eq!(restored.snooze_minutes, 30);
    }

    #[test]
    fn appearance_choices_survive_state_serialization() {
        for (appearance, value) in [
            (Appearance::System, "system"),
            (Appearance::Light, "light"),
            (Appearance::Dark, "dark"),
        ] {
            let store = Store {
                appearance,
                ..Store::default()
            };
            let json = serde_json::to_vec_pretty(&store).unwrap();
            let restored: Store = serde_json::from_slice(&json).unwrap();
            assert_eq!(restored.appearance, appearance);
            assert_eq!(serde_json::to_value(store).unwrap()["appearance"], value);
        }
    }

    #[test]
    fn invalid_appearance_preserves_the_rest_of_the_store() {
        let mut original = Store {
            roots: vec!["/projects".into()],
            disabled: [
                RepositoryId::new("github.com", "o/off"),
                RepositoryId::new("github.example.com", "o/off"),
            ]
            .into(),
            pending: vec![
                pr("o/r", 1, Some(T1)),
                pr("o/r", 2, Some(T1)),
                PendingReview {
                    host: "github.example.com".into(),
                    ..pr("o/r", 1, Some(T1))
                },
            ],
            snoozed: vec![
                snooze("o/r", 1, Some(T1)),
                Snooze {
                    host: "github.example.com".into(),
                    ..snooze("o/r", 1, Some(T1))
                },
            ],
            sync_account: Some("viewer".into()),
            last_successful_sync: Some(1_700_000_000),
            poll_minutes: 15,
            snooze_minutes: 30,
            notifications_muted: true,
            notify_drafts: false,
            ..Store::default()
        };
        original.queue_notifications(&[pr("o/r", 2, Some(T1))]);
        for appearance in [
            serde_json::json!("high_contrast"),
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(0),
            serde_json::json!(2.5),
            serde_json::json!([]),
            serde_json::json!(["dark"]),
            serde_json::json!({"light": null}),
        ] {
            let mut saved = serde_json::to_value(&original).unwrap();
            saved["appearance"] = appearance.clone();
            let restored: Store = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap())
                .unwrap_or_else(|error| panic!("appearance {appearance}: {error}"));
            assert_eq!(restored.appearance, Appearance::System);
            // The next save must retain all existing data, normalizing only the
            // unsupported preference to the safe default.
            assert_eq!(
                serde_json::to_value(&restored).unwrap(),
                serde_json::to_value(&original).unwrap()
            );
        }
    }

    #[test]
    fn old_settings_and_explicitly_empty_roots_survive_loading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"roots":[],"disabled":["o/r"],"poll_minutes":5,"snooze_minutes":15}"#,
        )
        .unwrap();
        let (store, warning) = Store::load_from(&path);
        assert!(warning.is_none());
        assert!(store.roots.is_empty());
        assert!(!store.is_enabled(&RepositoryId::new("github.com", "o/r")));
        assert_eq!((store.poll_minutes, store.snooze_minutes), (5, 15));
        assert_eq!(store.sync_account, None);
        assert_eq!(store.last_successful_sync, None);
    }

    #[test]
    fn corrupt_state_is_visible_and_preserved_before_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = b"{broken json";
        std::fs::write(&path, original).unwrap();
        let (store, warning) = Store::load_from(&path);
        assert!(warning.unwrap().contains("Original contents preserved"));
        let backup = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("state-recovery-")
            })
            .unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        store.save_to(&path).unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        assert!(Store::load_from(&path).1.is_none());
    }

    #[test]
    fn unreadable_state_blocks_saving_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the expected file path reliably fails on either OS,
        // even when tests run under a user that bypasses file permissions.
        let (store, warning) = Store::load_from(dir.path());
        assert!(warning.unwrap().contains("saving is paused"));
        let target = dir.path().join("replacement.json");
        assert!(store.save_to(&target).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn account_and_success_time_persist_and_same_account_keeps_snoozes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut store = Store::default();
        store.activate_account("Alice");
        store.last_successful_sync = Some(123);
        store.pending.push(pr("o/r", 1, None));
        store.snoozed.push(snooze("o/r", 1, None));
        assert!(!store.activate_account("alice"));
        store.save_to(&path).unwrap();
        let (store, warning) = Store::load_from(&path);
        assert!(warning.is_none());
        assert_eq!(store.sync_account.as_deref(), Some("Alice"));
        assert_eq!(store.last_successful_sync, Some(123));
        assert_eq!(store.pending.len(), 1);
        assert_eq!(store.snoozed.len(), 1);
    }

    fn pr(repo: &str, number: u64, requested_at: Option<&str>) -> PendingReview {
        PendingReview {
            host: default_host(),
            repo: repo.to_string(),
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/{repo}/pull/{number}"),
            author: "someone".to_string(),
            is_draft: false,
            rereview: false,
            requested_at: requested_at.map(str::to_string),
        }
    }

    fn snooze(repo: &str, number: u64, requested_at: Option<&str>) -> Snooze {
        Snooze {
            host: default_host(),
            repo: repo.to_string(),
            number,
            until: 100,
            requested_at: requested_at.map(str::to_string),
        }
    }

    const T1: &str = "2026-01-01T00:00:00Z";
    const T2: &str = "2026-01-02T00:00:00Z";

    #[test]
    fn independent_durations_survive_restart_and_expire_in_deadline_order() {
        let long = pr("Owner/Repo", 1, Some(T1));
        let short = pr("Owner/Repo", 2, Some(T1));
        let mut store = Store {
            pending: vec![long.clone(), short.clone()],
            snooze_minutes: 15,
            ..Store::default()
        };
        assert!(store.snooze(&long.key(), 120, 1_000));
        assert!(store.snooze(&short.key(), 5, 1_000));
        assert_eq!(store.snooze_minutes, 15);
        assert!(store.awake().is_empty());
        assert_eq!(store.next_snooze_until(), Some(1_300));

        // The production save/load format retains absolute deadlines and requests.
        let json = serde_json::to_vec(&store).unwrap();
        let mut restarted: Store = serde_json::from_slice(&json).unwrap();
        assert_eq!(restarted.snoozed, store.snoozed);
        assert_eq!(restarted.snooze_minutes, 15);
        assert!(!restarted.reconcile(store.pending.clone()).snoozes_changed);
        assert!(restarted.take_expired(1_299).is_empty());
        assert_eq!(restarted.take_expired(1_300), vec![short.clone()]);
        assert_eq!(restarted.awake(), vec![short]);
        assert_eq!(restarted.next_snooze_until(), Some(8_200));
        assert!(restarted.take_expired(8_199).is_empty());
        assert_eq!(restarted.take_expired(8_200), vec![long]);
        assert_eq!(restarted.next_snooze_until(), None);

        // Starting after both deadlines also wakes both reviews in one batch.
        let mut overdue: Store = serde_json::from_slice(&json).unwrap();
        assert_eq!(overdue.take_expired(9_000), store.pending);
        assert!(overdue.snoozed.is_empty());
    }

    #[test]
    fn chosen_durations_and_unsnooze_are_independent_across_hosts() {
        let public = pr("owner/repo", 7, Some(T1));
        let enterprise = PendingReview {
            host: "github.example.com".into(),
            ..public.clone()
        };
        let mut store = Store {
            pending: vec![public.clone(), enterprise.clone()],
            snooze_minutes: 15,
            ..Store::default()
        };
        assert!(store.snooze(&public.key(), 5, 1_000));
        assert!(store.snooze(&enterprise.key(), 120, 1_000));
        let json = serde_json::to_vec(&store).unwrap();
        let mut restarted: Store = serde_json::from_slice(&json).unwrap();
        assert_eq!(restarted.snooze_for(&public).unwrap().until, 1_300);
        assert_eq!(restarted.snooze_for(&enterprise).unwrap().until, 8_200);
        restarted.unsnooze(&public.key());
        assert_eq!(restarted.awake(), vec![public]);
        assert_eq!(restarted.next_snooze_until(), Some(8_200));
        assert!(restarted.take_expired(1_300).is_empty());
        assert_eq!(restarted.take_expired(8_200), vec![enterprise]);
        assert_eq!(restarted.snooze_minutes, 15);
    }

    #[test]
    fn resnooze_replaces_deadline_and_unsnooze_does_not_wake_it_again() {
        let review = pr("o/r", 1, Some(T1));
        let mut store = Store {
            pending: vec![review.clone()],
            ..Store::default()
        };
        assert!(store.snooze(&review.key(), 120, 1_000));
        assert!(store.snooze(&review.key(), 10, 1_000));
        assert_eq!(store.snoozed.len(), 1);
        assert_eq!(store.next_snooze_until(), Some(1_600));
        store.snooze_minutes = 30;
        assert_eq!(store.next_snooze_until(), Some(1_600));
        store.unsnooze(&review.key());
        assert_eq!(store.awake(), vec![review]);
        assert!(store.take_expired(10_000).is_empty());
        assert_eq!(store.next_snooze_until(), None);
        assert!(!store.snooze(&(RepositoryId::new("github.com", "o/r"), 2), 5, 1_000));
        assert!(store.snoozed.is_empty());
    }

    #[test]
    fn rerequest_cancels_only_its_snooze_and_retargets_the_next_wake() {
        let mut store = Store {
            pending: vec![pr("o/r", 1, Some(T1)), pr("o/r", 2, Some(T1))],
            ..Store::default()
        };
        assert!(store.snooze(&(RepositoryId::new("github.com", "o/r"), 1), 5, 1_000));
        assert!(store.snooze(&(RepositoryId::new("github.com", "o/r"), 2), 60, 1_000));
        let fresh = pr("o/r", 1, Some(T2));
        let result = store.reconcile(vec![fresh.clone(), pr("o/r", 2, Some(T1))]);
        assert!(result.snoozes_changed);
        assert_eq!(result.fresh, vec![fresh.clone()]);
        assert_eq!(store.awake(), vec![fresh]);
        assert_eq!(store.next_snooze_until(), Some(4_600));
        assert!(store.take_expired(1_300).is_empty());
        assert_eq!(store.take_expired(4_600), vec![pr("o/r", 2, Some(T1))]);
    }

    #[test]
    fn loads_existing_snooze_format_and_missing_settings_defaults() {
        let mut store: Store = serde_json::from_str(
            r#"{"snoozed":[{"repo":"o/r","number":1,"until":1300,"requested_at":null}]}"#,
        )
        .unwrap();
        assert_eq!(store.snooze_minutes, 5);
        assert_eq!(store.next_snooze_until(), Some(1_300));
        store.pending = vec![pr("o/r", 1, None)];
        assert_eq!(store.take_expired(1_300), store.pending);
    }

    #[test]
    fn missing_request_timestamp_preserves_snooze_until_newer_request_or_withdrawal() {
        let review = pr("Owner/Repo", 1, Some(T1));
        let mut store = Store {
            pending: vec![review.clone()],
            ..Store::default()
        };
        store.snooze(&review.key(), 5, 1_000);
        let deadline = store.snoozed[0].clone();
        let result = store.reconcile(vec![pr("Owner/Repo", 1, None)]);
        assert!(result.fresh.is_empty());
        assert!(!result.pending_changed);
        assert!(!result.snoozes_changed);
        assert_eq!(store.pending, vec![review.clone()]);
        assert_eq!(store.snoozed, vec![deadline]);
        let newer = pr("Owner/Repo", 1, Some(T2));
        assert_eq!(
            store.reconcile(vec![newer.clone()]).fresh,
            vec![newer.clone()]
        );
        assert!(store.snoozed.is_empty());
        store.snooze(&newer.key(), 5, 1_000);
        store.reconcile(vec![]);
        assert!(store.snoozed.is_empty());
        assert!(store.notification_queue.is_empty());
    }

    /// Pins request freshness and snooze reconciliation.
    #[test]
    fn reconciles_requests_and_snoozes() {
        struct Case {
            name: &'static str,
            pending: Vec<PendingReview>,
            snoozed: Vec<Snooze>,
            fetched: Vec<PendingReview>,
            fresh: Vec<u64>,
            pending_changed: bool,
            snoozes_kept: usize,
        }
        let cases = [
            Case {
                name: "unknown key is fresh",
                pending: vec![],
                snoozed: vec![],
                fetched: vec![pr("o/r", 1, Some(T1))],
                fresh: vec![1],
                pending_changed: true,
                snoozes_kept: 0,
            },
            Case {
                name: "later request is fresh and ends the snooze",
                pending: vec![pr("o/r", 1, Some(T1))],
                snoozed: vec![snooze("o/r", 1, Some(T1))],
                fetched: vec![pr("o/r", 1, Some(T2))],
                fresh: vec![1],
                pending_changed: true,
                snoozes_kept: 0,
            },
            Case {
                name: "equal request is not fresh and keeps the snooze",
                pending: vec![pr("o/r", 1, Some(T1))],
                snoozed: vec![snooze("o/r", 1, Some(T1))],
                fetched: vec![pr("o/r", 1, Some(T1))],
                fresh: vec![],
                pending_changed: false,
                snoozes_kept: 1,
            },
            Case {
                // `Option` orders `None` before `Some`.
                name: "None to Some is fresh",
                pending: vec![pr("o/r", 1, None)],
                snoozed: vec![snooze("o/r", 1, None)],
                fetched: vec![pr("o/r", 1, Some(T1))],
                fresh: vec![1],
                pending_changed: true,
                snoozes_kept: 0,
            },
            Case {
                name: "PR that drops out ends its snooze",
                pending: vec![pr("o/r", 1, Some(T1)), pr("o/r", 2, Some(T1))],
                snoozed: vec![snooze("o/r", 1, Some(T1)), snooze("o/r", 2, Some(T1))],
                fetched: vec![pr("o/r", 2, Some(T1))],
                fresh: vec![],
                pending_changed: true,
                snoozes_kept: 1,
            },
            Case {
                name: "repo case doesn't matter, keys are lowercase",
                pending: vec![pr("owner/repo", 1, Some(T1))],
                snoozed: vec![snooze("owner/repo", 1, Some(T1))],
                fetched: vec![pr("Owner/Repo", 1, Some(T1))],
                fresh: vec![],
                pending_changed: true,
                snoozes_kept: 1,
            },
            Case {
                name: "nothing changes",
                pending: vec![],
                snoozed: vec![],
                fetched: vec![],
                fresh: vec![],
                pending_changed: false,
                snoozes_kept: 0,
            },
        ];
        for case in cases {
            let snoozed = case.snoozed.len();
            let mut store = Store {
                pending: case.pending,
                snoozed: case.snoozed,
                ..Store::default()
            };
            let reconciled = store.reconcile(case.fetched.clone());
            let fresh: Vec<u64> = reconciled.fresh.iter().map(|pr| pr.number).collect();
            assert_eq!(fresh, case.fresh, "{}: fresh", case.name);
            assert_eq!(
                reconciled.pending_changed, case.pending_changed,
                "{}: pending_changed",
                case.name
            );
            assert_eq!(
                store.snoozed.len(),
                case.snoozes_kept,
                "{}: snoozes",
                case.name
            );
            assert_eq!(
                reconciled.snoozes_changed,
                case.snoozes_kept != snoozed,
                "{}: snoozes_changed",
                case.name
            );
            assert_eq!(store.pending, case.fetched, "{}: pending", case.name);
        }
    }

    #[test]
    fn reconcile_sorts_newest_first() {
        let mut store = Store::default();
        let reconciled = store.reconcile(vec![
            pr("o/r", 1, Some(T1)),
            pr("o/r", 2, None),
            pr("o/r", 3, Some(T2)),
        ]);
        let order: Vec<u64> = store.pending.iter().map(|pr| pr.number).collect();
        assert_eq!(order, vec![3, 1, 2]);
        let fresh: Vec<u64> = reconciled.fresh.iter().map(|pr| pr.number).collect();
        assert_eq!(fresh, vec![3, 1, 2]);
    }

    #[test]
    fn key_lowercases_repo() {
        assert_eq!(
            pr("Owner/Repo", 7, None).key(),
            (RepositoryId::new("github.com", "owner/repo"), 7)
        );
    }

    #[test]
    fn failed_repos_keep_requests_and_snoozes_while_complete_repos_update() {
        let mut store = Store {
            pending: vec![pr("o/failed", 1, Some(T1)), pr("o/good", 2, Some(T1))],
            snoozed: vec![
                snooze("o/failed", 1, Some(T1)),
                snooze("o/good", 2, Some(T1)),
            ],
            ..Store::default()
        };
        let watched = HashSet::from([
            RepositoryId::new("github.com", "o/failed"),
            RepositoryId::new("github.com", "o/good"),
        ]);
        // A confirmed request with the same timestamp keeps its saved snooze.
        let result = store.reconcile_repositories(
            vec![pr("o/failed", 1, Some(T1)), pr("o/good", 3, Some(T2))],
            &HashSet::from([RepositoryId::new("github.com", "o/good")]),
            &watched,
        );
        assert_eq!(
            store.pending,
            vec![pr("o/good", 3, Some(T2)), pr("o/failed", 1, Some(T1))]
        );
        assert_eq!(result.fresh, vec![pr("o/good", 3, Some(T2))]);
        assert_eq!(store.snoozed, vec![snooze("o/failed", 1, Some(T1))]);

        // Once the failed repo succeeds, a later request ends its snooze.
        let result = store.reconcile_repositories(
            vec![pr("o/failed", 1, Some(T2)), pr("o/good", 3, Some(T2))],
            &watched,
            &watched,
        );
        assert_eq!(result.fresh, vec![pr("o/failed", 1, Some(T2))]);
        assert!(store.snoozed.is_empty());
        // A complete empty response clears requests after review, withdrawal
        // or closure, and won't preserve old state as though it were an error.
        store.reconcile_repositories(vec![], &watched, &watched);
        assert!(store.pending.is_empty());
    }

    #[test]
    fn partial_snapshots_preserve_undelivered_alerts_until_requests_are_confirmed_gone() {
        let saved = pr("o/failed", 1, Some(T1));
        let snoozed = pr("o/failed", 2, Some(T1));
        let incoming = pr("o/failed", 3, Some(T2));
        let resolved = pr("o/good", 4, Some(T1));
        let mut store = Store {
            notifications_muted: true,
            ..Store::default()
        };
        store.reconcile(vec![saved.clone(), snoozed.clone(), resolved]);
        store.snooze(&snoozed.key(), 60, 1_000);
        let saved_notice = store.notification_queue[0].clone();
        let watched = HashSet::from([
            RepositoryId::new("github.com", "o/failed"),
            RepositoryId::new("github.com", "o/good"),
        ]);
        store.reconcile_repositories(
            vec![incoming.clone()],
            &HashSet::from([RepositoryId::new("github.com", "o/good")]),
            &watched,
        );
        assert_eq!(store.notification_queue.len(), 2);
        assert!(store.notification_queue.contains(&saved_notice));
        assert!(store.snooze_for(&snoozed).is_some());
        assert!(store.notifications_due().is_empty());

        let mut restarted: Store =
            serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
        restarted.notifications_muted = false;
        assert_eq!(
            restarted
                .notifications_due()
                .into_iter()
                .map(|(_, pr)| pr)
                .collect::<Vec<_>>(),
            vec![incoming.clone(), saved]
        );
        restarted.reconcile_repositories(vec![incoming], &watched, &watched);
        assert_eq!(restarted.notification_queue.len(), 1);
        assert!(restarted.snoozed.is_empty());
        restarted.reconcile_repositories(vec![], &watched, &watched);
        assert!(restarted.notification_queue.is_empty());
    }

    #[test]
    fn incomplete_check_adds_confirmed_requests_without_regressing_saved_timestamps() {
        let mut store = Store {
            pending: vec![pr("o/r", 1, Some(T2)), pr("o/r", 2, Some(T1))],
            snoozed: vec![snooze("o/r", 1, Some(T2)), snooze("o/r", 2, Some(T1))],
            ..Store::default()
        };
        let result = store.reconcile_repositories(
            vec![
                pr("o/r", 1, Some(T1)),
                pr("o/r", 2, Some(T2)),
                pr("o/r", 3, Some(T1)),
            ],
            &HashSet::new(),
            &HashSet::from([RepositoryId::new("github.com", "o/r")]),
        );
        assert_eq!(
            store.pending,
            vec![
                pr("o/r", 1, Some(T2)),
                pr("o/r", 2, Some(T2)),
                pr("o/r", 3, Some(T1))
            ]
        );
        assert_eq!(
            result.fresh,
            vec![pr("o/r", 2, Some(T2)), pr("o/r", 3, Some(T1))]
        );
        assert_eq!(store.snoozed, vec![snooze("o/r", 1, Some(T2))]);
        assert!(result.snoozes_changed);
    }

    #[test]
    fn failed_check_keeps_watched_state_but_removed_repos_leave_queue() {
        let mut store = Store {
            pending: vec![pr("o/failed", 1, Some(T1)), pr("o/off", 2, Some(T1))],
            snoozed: vec![
                snooze("o/failed", 1, Some(T1)),
                snooze("o/off", 2, Some(T1)),
            ],
            ..Store::default()
        };
        let result = store.reconcile_repositories(
            vec![],
            &HashSet::new(),
            &HashSet::from([RepositoryId::new("github.com", "o/failed")]),
        );
        assert!(result.fresh.is_empty());
        assert_eq!(store.pending, vec![pr("o/failed", 1, Some(T1))]);
        assert_eq!(store.snoozed, vec![snooze("o/failed", 1, Some(T1))]);
        let result = store.reconcile_repositories(
            vec![],
            &HashSet::new(),
            &HashSet::from([RepositoryId::new("github.com", "o/failed")]),
        );
        assert!(!result.pending_changed);
        assert!(!result.snoozes_changed);
        assert!(result.fresh.is_empty());
    }

    #[test]
    fn existing_state_format_remains_compatible() {
        let old = serde_json::json!({
            "pending": [pr("o/r", 1, Some(T1))],
            "snoozed": [snooze("o/r", 1, Some(T1))]
        });
        let mut store: Store = serde_json::from_value(old).unwrap();
        assert_eq!(store.poll_minutes, 2);
        let result = store.reconcile_repositories(
            vec![],
            &HashSet::new(),
            &HashSet::from([RepositoryId::new("github.com", "o/r")]),
        );
        assert!(!result.pending_changed);
        assert!(!result.snoozes_changed);
        assert_eq!(store.pending[0].requested_at.as_deref(), Some(T1));
    }

    #[test]
    fn take_expired_wakes_snoozes_that_ran_out() {
        let mut store = Store {
            pending: vec![pr("o/r", 1, Some(T1)), pr("o/r", 2, Some(T1))],
            snoozed: vec![
                Snooze {
                    until: 50,
                    ..snooze("o/r", 1, Some(T1))
                },
                Snooze {
                    until: 150,
                    ..snooze("o/r", 2, Some(T1))
                },
                // A snooze whose PR is no longer pending wakes nothing.
                Snooze {
                    until: 50,
                    ..snooze("o/r", 3, Some(T1))
                },
            ],
            ..Store::default()
        };
        let woken: Vec<u64> = store.take_expired(100).iter().map(|pr| pr.number).collect();
        assert_eq!(woken, vec![1]);
        assert_eq!(store.snoozed.len(), 1);
        assert_eq!(store.snoozed[0].number, 2);
    }

    #[test]
    fn legacy_notification_queue_defaults_to_public_host_and_preserves_delivery_state() {
        let legacy = r#"{
            "notifications_muted":true,"notify_drafts":false,"notification_sequence":42,
            "pending":[{"repo":"owner/repo","number":7,"title":"Review","url":"https://github.com/owner/repo/pull/7","author":"someone","is_draft":false,"rereview":false,"requested_at":null}],
            "notification_queue":[{"repo":"owner/repo","number":7,"requested_at":null,"sequence":42}]
        }"#;
        let store: Store = serde_json::from_str(legacy).unwrap();
        assert!(store.notifications_muted);
        assert!(!store.notify_drafts);
        assert_eq!(store.notification_sequence, 42);
        assert_eq!(store.notification_queue[0].host, "github.com");
        assert!(store.notification_queue[0].matches(&store.pending[0]));
        assert!(!store.notification_queue[0].matches(&PendingReview {
            host: "github.example.com".into(),
            ..store.pending[0].clone()
        }));
        let saved: Store = serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
        assert_eq!(saved.notification_queue, store.notification_queue);
    }

    #[test]
    fn migrates_v043_state_without_losing_settings_or_snoozes() {
        let legacy = r#"{
            "roots":["/tmp/projects"], "disabled":["Owner/Repo"],
            "poll_minutes":15, "snooze_minutes":30,
            "pending":[{"repo":"other/repo","number":7,"title":"Review","url":"https://github.com/other/repo/pull/7","author":"someone","is_draft":false,"rereview":true,"requested_at":"2026-01-01T00:00:00Z"}],
            "snoozed":[{"repo":"other/repo","number":7,"until":123456789,"requested_at":"2026-01-01T00:00:00Z"}]
        }"#;
        let store: Store = serde_json::from_str(legacy).unwrap();
        assert_eq!(store.roots, vec![std::path::PathBuf::from("/tmp/projects")]);
        assert_eq!((store.poll_minutes, store.snooze_minutes), (15, 30));
        assert!(!store.is_enabled(&RepositoryId::new("github.com", "owner/repo")));
        assert!(store.is_enabled(&RepositoryId::new("github.example.com", "owner/repo")));
        assert_eq!(store.pending[0].host, "github.com");
        assert_eq!(
            store.snooze_for(&store.pending[0]).unwrap().until,
            123456789
        );
        let saved = serde_json::to_string(&store).unwrap();
        let reloaded: Store = serde_json::from_str(&saved).unwrap();
        assert_eq!(store.disabled, reloaded.disabled);
        assert_eq!(store.pending, reloaded.pending);
        assert_eq!(store.snoozed, reloaded.snoozed);
        assert_eq!(reloaded.awake().len(), 0);
    }

    #[test]
    fn same_slug_and_pr_number_on_different_hosts_stay_independent() {
        let public = pr("Owner/Repo", 7, Some(T1));
        let enterprise = PendingReview {
            host: "github.example.com".into(),
            ..public.clone()
        };
        let mut store = Store::default();
        let changes = store.reconcile(vec![public.clone(), enterprise.clone()]);
        assert_eq!(changes.fresh.len(), 2);
        store.snoozed.push(Snooze {
            host: enterprise.host.clone(),
            ..snooze("owner/repo", 7, Some(T1))
        });
        assert_eq!(store.awake(), vec![public.clone()]);
        store.disabled.insert(public.repository());
        assert!(!store.is_enabled(&public.repository()));
        assert!(store.is_enabled(&enterprise.repository()));
        let next_public = PendingReview {
            requested_at: Some(T2.into()),
            ..public
        };
        let changes = store.reconcile(vec![next_public, enterprise.clone()]);
        assert_eq!(changes.fresh.len(), 1);
        assert_eq!(store.snoozed.len(), 1);
        assert_eq!(store.take_expired(100), vec![enterprise]);
    }

    #[test]
    fn failed_hosts_keep_cache_and_snoozes_while_healthy_hosts_reconcile() {
        let public = pr("owner/repo", 7, Some(T1));
        let server = PendingReview {
            host: "github.example.com".into(),
            ..public.clone()
        };
        let cloud = PendingReview {
            host: "acme.ghe.com".into(),
            ..public.clone()
        };
        let mut store = Store {
            pending: vec![public.clone(), server.clone(), cloud.clone()],
            snoozed: vec![Snooze {
                host: server.host.clone(),
                ..snooze("owner/repo", 7, Some(T1))
            }],
            ..Store::default()
        };
        let watched = [public.repository(), server.repository(), cloud.repository()]
            .into_iter()
            .collect();
        let healthy = [public.repository(), cloud.repository()]
            .into_iter()
            .collect();
        let new_cloud = PendingReview {
            requested_at: Some(T2.into()),
            ..cloud
        };
        let changes = store.reconcile_repositories(vec![new_cloud.clone()], &healthy, &watched);
        assert_eq!(changes.fresh, vec![new_cloud]);
        assert!(store.pending.contains(&server));
        assert!(!store.pending.contains(&public));
        assert_eq!(store.snoozed.len(), 1);
        assert!(!changes.snoozes_changed);
        // Recovery reporting no reviews clears the server and its snooze.
        let changes = store.reconcile_repositories(
            vec![],
            &[server.repository()].into_iter().collect(),
            &watched,
        );
        assert!(!store.pending.contains(&server));
        assert!(store.snoozed.is_empty());
        assert!(changes.snoozes_changed);
    }

    #[test]
    fn failed_host_caches_do_not_restore_disabled_or_removed_repositories() {
        let public = pr("owner/repo", 7, Some(T1));
        let server = PendingReview {
            host: "github.example.com".into(),
            ..public.clone()
        };
        let mut store = Store {
            pending: vec![public.clone(), server],
            ..Store::default()
        };
        let watched = [public.repository()].into_iter().collect();
        store.reconcile_repositories(vec![], &Default::default(), &watched);
        assert_eq!(store.pending, vec![public]);
    }
}
