use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{
    repository::{PUBLIC_HOST, RepositoryId, default_host},
    theme::Appearance,
};

/// Everything that survives a restart, saved as JSON in the platform config dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    /// Folders scanned for local git clones.
    pub roots: Vec<PathBuf>,
    /// `owner/name` slugs (lowercase) the user switched off.
    pub disabled: BTreeSet<String>,
    /// Pull requests currently waiting on the user's review.
    pub pending: Vec<PendingReview>,
    /// Minutes between checks of GitHub for review requests.
    pub poll_minutes: u64,
    /// Default minutes a snoozed review stays hidden.
    pub snooze_minutes: u64,
    /// Reviews the user put aside for now.
    pub snoozed: Vec<Snooze>,
    /// Saved gh accounts seen before, including accounts since signed out.
    pub known_accounts: BTreeSet<String>,
    /// Verified login aliases used to migrate older name-based preferences.
    pub account_ids: BTreeMap<String, u64>,
    pub disabled_account_ids: BTreeSet<u64>,
    pub repo_account_ids: BTreeMap<String, BTreeSet<u64>>,
    /// Legacy preferences are bound to IDs as their accounts are verified.
    pub disabled_accounts: BTreeSet<String>,
    /// Missing entry means all enabled accounts; an empty set means none.
    pub repo_accounts: BTreeMap<String, BTreeSet<String>>,
    /// Successful checks in this session, never trusted across a restart.
    #[serde(skip)]
    pub available_accounts: BTreeSet<String>,
    /// Discovered repos in this session, never trusted across a restart.
    #[serde(skip)]
    pub local_repos: BTreeSet<String>,
    /// Failed repository checks keep their cache hidden in this session.
    #[serde(skip)]
    pub unavailable_repos: BTreeSet<(u64, String)>,
    /// Enterprise hosts validated in this session, gating cached alerts.
    #[serde(skip)]
    pub available_hosts: BTreeSet<String>,
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
            known_accounts: BTreeSet::new(),
            account_ids: BTreeMap::new(),
            disabled_account_ids: BTreeSet::new(),
            repo_account_ids: BTreeMap::new(),
            disabled_accounts: BTreeSet::new(),
            repo_accounts: BTreeMap::new(),
            available_accounts: BTreeSet::new(),
            local_repos: BTreeSet::new(),
            unavailable_repos: BTreeSet::new(),
            available_hosts: BTreeSet::new(),
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
    /// Receiving github.com login. Empty only when reading legacy state.
    #[serde(default)]
    pub account: String,
    /// Stable github.com user ID; a reused login must not inherit a snooze.
    #[serde(default)]
    pub account_id: u64,
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
    pub fn key(&self) -> ReviewKey {
        (self.account_id, self.repository().store_key(), self.number)
    }

    pub fn repository(&self) -> RepositoryId {
        RepositoryId::new(&self.host, &self.repo)
    }
    pub fn repo_label(&self) -> String {
        format!("{}/{}", self.host, self.repo)
    }
    pub fn request_label(&self) -> String {
        if self.account.is_empty() {
            self.repo_label()
        } else {
            format!("@{} · {}", self.account, self.repo_label())
        }
    }
}

/// Stable github.com user ID, normalized repository, and PR number.
pub type ReviewKey = (u64, String, u64);
/// A delivery event, independently persisted from the current review list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewNotice {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub account_id: u64,
    pub repo: String,
    pub number: u64,
    pub requested_at: Option<String>,
    pub sequence: u64,
}

impl ReviewNotice {
    pub fn matches(&self, pr: &PendingReview) -> bool {
        self.account_id == pr.account_id
            && RepositoryId::new(&self.host, &self.repo) == pr.repository()
            && self.number == pr.number
            && self.requested_at == pr.requested_at
    }
}

/// A pending review hidden from the list and the tray until `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snooze {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub account: String,
    /// Stable github.com user ID; a reused login must not inherit a snooze.
    #[serde(default)]
    pub account_id: u64,
    /// `owner/name`, lowercase.
    pub repo: String,
    pub number: u64,
    /// Unix seconds when the review comes back.
    pub until: i64,
    /// The request that was snoozed; a newer one ends the snooze.
    pub requested_at: Option<String>,
}

impl Snooze {
    pub fn key(&self) -> ReviewKey {
        (
            self.account_id,
            RepositoryId::new(&self.host, &self.repo).store_key(),
            self.number,
        )
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

    pub fn load() -> Self {
        let mut store = Self::path()
            .ok()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|json| Self::from_json(&json).ok())
            .unwrap_or_default();
        if store.roots.is_empty() {
            store.roots = default_roots();
        }
        store
    }

    /// Legacy entries have no trustworthy receiving identity. Keep preferences,
    /// but never guess an account for an old cached request or snooze.
    pub fn from_json(json: &str) -> Result<Self> {
        let mut store: Self = serde_json::from_str(json)?;
        for review in &mut store.pending {
            review.host.make_ascii_lowercase();
        }
        for snooze in &mut store.snoozed {
            snooze.host.make_ascii_lowercase();
        }
        for notice in &mut store.notification_queue {
            notice.host.make_ascii_lowercase();
        }
        store
            .pending
            .retain(|pr| pr.host != PUBLIC_HOST || (!pr.account.is_empty() && pr.account_id != 0));
        store
            .snoozed
            .retain(|s| s.host != PUBLIC_HOST || (!s.account.is_empty() && s.account_id != 0));
        // Existing account-aware state already carries verified user IDs.
        for (login, id) in store
            .pending
            .iter()
            .filter(|pr| pr.host == PUBLIC_HOST)
            .map(|pr| (&pr.account, pr.account_id))
            .chain(
                store
                    .snoozed
                    .iter()
                    .filter(|s| s.host == PUBLIC_HOST)
                    .map(|s| (&s.account, s.account_id)),
            )
        {
            store.account_ids.entry(login.to_lowercase()).or_insert(id);
        }
        store.disabled = store
            .disabled
            .into_iter()
            .map(|key| {
                key.strip_prefix("github.com/")
                    .unwrap_or(&key)
                    .to_lowercase()
            })
            .collect();
        store.repo_accounts = store
            .repo_accounts
            .into_iter()
            .map(|(key, names)| {
                (
                    key.strip_prefix("github.com/")
                        .unwrap_or(&key)
                        .to_lowercase(),
                    names,
                )
            })
            .collect();
        store.repo_account_ids = store
            .repo_account_ids
            .into_iter()
            .map(|(key, ids)| {
                (
                    key.strip_prefix("github.com/")
                        .unwrap_or(&key)
                        .to_lowercase(),
                    ids,
                )
            })
            .collect();
        store.migrate_account_preferences();
        store.prune_notifications();
        Ok(store)
    }

    fn migrate_account_preferences(&mut self) {
        self.disabled_accounts.retain(|login| {
            if let Some(id) = self.account_ids.get(login) {
                self.disabled_account_ids.insert(*id);
                false
            } else {
                true
            }
        });
        for (repo, names) in &mut self.repo_accounts {
            let ids = self.repo_account_ids.entry(repo.clone()).or_default();
            names.retain(|login| {
                if let Some(id) = self.account_ids.get(login) {
                    ids.insert(*id);
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Record a verified identity before applying any monitoring choices.
    /// Keep aliases for stale gh config names, but display the current login.
    pub fn record_account(&mut self, login: &str, id: u64) {
        let login = login.to_lowercase();
        self.migrate_account_preferences();
        self.account_ids.insert(login.clone(), id);
        self.migrate_account_preferences();
        self.known_accounts
            .retain(|name| name == &login || self.account_ids.get(name) != Some(&id));
        self.known_accounts.insert(login.clone());
        for pr in self
            .pending
            .iter_mut()
            .filter(|pr| pr.host == PUBLIC_HOST && pr.account_id == id)
        {
            pr.account = login.clone();
        }
        for snooze in self
            .snoozed
            .iter_mut()
            .filter(|s| s.host == PUBLIC_HOST && s.account_id == id)
        {
            snooze.account = login.clone();
        }
    }

    pub fn account_enabled(&self, account: &str) -> bool {
        let account = account.to_lowercase();
        !self.disabled_accounts.contains(&account)
            && self
                .account_ids
                .get(&account)
                .is_none_or(|id| !self.disabled_account_ids.contains(id))
    }

    pub fn toggle_account(&mut self, account: &str) {
        let account = account.to_lowercase();
        if let Some(id) = self.account_ids.get(&account) {
            if !self.disabled_account_ids.remove(id) {
                self.disabled_account_ids.insert(*id);
            }
            self.disabled_accounts.remove(&account);
        } else if !self.disabled_accounts.remove(&account) {
            self.disabled_accounts.insert(account);
        }
    }

    pub fn all_repo_accounts(&self, repo: &str) -> bool {
        let repo = repo.to_lowercase();
        !self.repo_accounts.contains_key(&repo) && !self.repo_account_ids.contains_key(&repo)
    }

    pub fn repo_account_selected(&self, repo: &str, account: &str) -> bool {
        let repo = repo.to_lowercase();
        let account = account.to_lowercase();
        self.all_repo_accounts(&repo)
            || self
                .repo_accounts
                .get(&repo)
                .is_some_and(|names| names.contains(&account))
            || self.account_ids.get(&account).is_some_and(|id| {
                self.repo_account_ids
                    .get(&repo)
                    .is_some_and(|ids| ids.contains(id))
            })
    }

    pub fn toggle_repo_account(&mut self, repo: &str, account: &str) {
        let repo = repo.to_lowercase();
        let account = account.to_lowercase();
        if self.all_repo_accounts(&repo) {
            let ids = self
                .known_accounts
                .iter()
                .filter_map(|name| self.account_ids.get(name).copied())
                .collect();
            let names = self
                .known_accounts
                .iter()
                .filter(|name| !self.account_ids.contains_key(*name))
                .cloned()
                .collect();
            self.repo_account_ids.insert(repo.clone(), ids);
            self.repo_accounts.insert(repo.clone(), names);
        }
        if let Some(id) = self.account_ids.get(&account) {
            let allowed = self.repo_account_ids.entry(repo).or_default();
            if !allowed.remove(id) {
                allowed.insert(*id);
            }
        } else {
            let allowed = self.repo_accounts.entry(repo).or_default();
            if !allowed.remove(&account) {
                allowed.insert(account);
            }
        }
    }

    pub fn reset_repo_accounts(&mut self, repo: &str) {
        self.repo_accounts.remove(&repo.to_lowercase());
        self.repo_account_ids.remove(&repo.to_lowercase());
    }

    pub fn monitors(&self, account: &str, repo: &str) -> bool {
        self.account_enabled(account)
            && self.is_enabled(repo)
            && self.repo_account_selected(repo, account)
    }

    pub fn visible(&self, pr: &PendingReview) -> bool {
        let key = pr.repository().store_key();
        self.local_repos.contains(&key)
            && self.is_enabled(&key)
            && (pr.host != PUBLIC_HOST
                || (!self.unavailable_repos.contains(&(pr.account_id, key))
                    && self.available_accounts.contains(&pr.account.to_lowercase())
                    && self.monitors(&pr.account, &pr.repo)))
    }

    #[cfg(test)]
    pub fn visible_pending(&self) -> Vec<PendingReview> {
        self.pending
            .iter()
            .filter(|pr| self.visible(pr))
            .cloned()
            .collect()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Write then rename so a crash mid-write never leaves a truncated file.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn is_enabled(&self, slug: &str) -> bool {
        !self.disabled.contains(&slug.to_lowercase())
    }

    pub fn snooze_for(&self, pr: &PendingReview) -> Option<&Snooze> {
        let key = pr.key();
        self.snoozed.iter().find(|snooze| snooze.key() == key)
    }

    /// Snoozes only this PR for the chosen duration, leaving the default alone.
    pub fn snooze(&mut self, key: &ReviewKey, minutes: u64, now: i64) -> bool {
        let Some(pr) = self
            .pending
            .iter()
            .find(|pr| pr.key() == *key && self.visible(pr))
        else {
            return false;
        };
        let snooze = Snooze {
            host: pr.host.clone(),
            account: pr.account.clone(),
            account_id: key.0,
            repo: pr.repo.to_lowercase(),
            number: key.2,
            until: now + minutes as i64 * 60,
            requested_at: pr.requested_at.clone(),
        };
        self.snoozed.retain(|s| s.key() != *key);
        self.snoozed.push(snooze);
        self.discard_notification(key);
        true
    }

    /// Brings a PR back without treating it as an expired snooze.
    pub fn unsnooze(&mut self, key: &ReviewKey) {
        self.snoozed.retain(|s| s.key() != *key);
        self.discard_notification(key);
    }

    pub fn next_snooze_until(&self) -> Option<i64> {
        self.snoozed
            .iter()
            .filter(|s| {
                self.pending
                    .iter()
                    .any(|pr| pr.key() == s.key() && self.visible(pr))
            })
            .map(|s| s.until)
            .min()
    }

    /// Pending reviews that aren't snoozed.
    pub fn awake(&self) -> Vec<PendingReview> {
        self.pending
            .iter()
            .filter(|pr| self.visible(pr) && self.snooze_for(pr).is_none())
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
                host: pr.host.clone(),
                account_id: pr.account_id,
                repo: pr.repo.to_lowercase(),
                number: pr.number,
                requested_at: pr.requested_at.clone(),
                sequence: self.notification_sequence,
            });
        }
    }

    /// Seed the first successful poll after launch, including suppressed drafts.
    pub fn queue_startup_notifications(&mut self, accounts: &BTreeSet<u64>) {
        let reviews = self
            .awake()
            .into_iter()
            .filter(|pr| pr.host == PUBLIC_HOST && accounts.contains(&pr.account_id))
            .collect::<Vec<_>>();
        self.queue_notifications(&reviews);
    }

    pub fn queue_startup_host_notifications(&mut self, hosts: &BTreeSet<String>) {
        let reviews = self
            .awake()
            .into_iter()
            .filter(|pr| hosts.contains(&pr.host))
            .collect::<Vec<_>>();
        self.queue_notifications(&reviews);
    }

    pub fn discard_notification(&mut self, key: &ReviewKey) {
        self.notification_queue.retain(|notice| {
            (
                notice.account_id,
                RepositoryId::new(&notice.host, &notice.repo).store_key(),
                notice.number,
            ) != *key
        });
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
                if !self.visible(pr)
                    || (pr.host != PUBLIC_HOST && !self.available_hosts.contains(&pr.host))
                    || self.snooze_for(pr).is_some()
                    || (pr.is_draft && !self.notify_drafts)
                {
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
    #[cfg(test)]
    pub fn reconcile(
        &mut self,
        fetched: Vec<PendingReview>,
        checked_accounts: &BTreeMap<String, u64>,
    ) -> Reconciled {
        self.reconcile_scopes(
            fetched,
            checked_accounts,
            &BTreeSet::new(),
            &self.local_repos.clone(),
        )
    }

    pub fn reconcile_scopes(
        &mut self,
        mut fetched: Vec<PendingReview>,
        checked_accounts: &BTreeMap<String, u64>,
        successful_hosts: &BTreeSet<String>,
        watched: &BTreeSet<String>,
    ) -> Reconciled {
        fetched
            .retain(|pr| pr.host == PUBLIC_HOST || watched.contains(&pr.repository().store_key()));
        // Failed, disabled, or signed-out accounts retain their own cache.
        fetched.extend(
            self.pending
                .iter()
                .filter(|pr| {
                    if pr.host != PUBLIC_HOST {
                        return watched.contains(&pr.repository().store_key())
                            && !successful_hosts.contains(&pr.host);
                    }
                    self.unavailable_repos
                        .contains(&(pr.account_id, pr.repo.to_lowercase()))
                        || (!checked_accounts.contains_key(&pr.account.to_lowercase())
                            && !checked_accounts.values().any(|id| *id == pr.account_id))
                })
                .cloned(),
        );
        fetched.sort_by(|a, b| b.requested_at.cmp(&a.requested_at));

        // A request is new when the PR wasn't listed, or when it was asked
        // again after the request already on file.
        let known: HashMap<_, _> = self
            .pending
            .iter()
            .map(|pr| (pr.key(), pr.requested_at.clone()))
            .collect();
        // GitHub returns only a bounded timeline of request events. A missing
        // timestamp on a still-pending PR is not evidence of a new request;
        // keep its known identity, undelivered notice, and snooze deadline.
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
                None => {
                    if pr.host == PUBLIC_HOST {
                        checked_accounts.contains_key(&pr.account.to_lowercase())
                    } else {
                        successful_hosts.contains(&pr.host)
                    }
                }
                Some(previous) => {
                    (if pr.host == PUBLIC_HOST {
                        checked_accounts.contains_key(&pr.account.to_lowercase())
                    } else {
                        successful_hosts.contains(&pr.host)
                    }) && pr.requested_at > *previous
                }
            })
            .cloned()
            .collect();

        // A snooze ends when its PR leaves the list or is requested again.
        let previous_snoozes = self.snoozed.clone();
        self.snoozed.retain_mut(|snooze| {
            if let Some(pr) = fetched
                .iter()
                .find(|pr| pr.key() == snooze.key() && pr.requested_at == snooze.requested_at)
            {
                // A renamed login still represents the same receiving user.
                snooze.account = pr.account.clone();
                true
            } else {
                false
            }
        });
        let snoozes_changed = self.snoozed != previous_snoozes;

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

    /// Ends every snooze that ran out by `now`, and returns the pending
    /// reviews they hid.
    pub fn take_expired(&mut self, now: i64) -> Vec<PendingReview> {
        let (expired, remaining): (Vec<Snooze>, Vec<Snooze>) = std::mem::take(&mut self.snoozed)
            .into_iter()
            .partition(|s| {
                s.until <= now
                    && (s.host != PUBLIC_HOST
                        || self.available_accounts.contains(&s.account.to_lowercase()))
                    && !self
                        .unavailable_repos
                        .contains(&(s.account_id, s.repo.to_lowercase()))
            });
        self.snoozed = remaining;
        let woken: Vec<_> = self
            .pending
            .iter()
            .filter(|pr| self.visible(pr) && expired.iter().any(|s| s.key() == pr.key()))
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
    use super::{PendingReview, Snooze, Store};
    use crate::repository::default_host;
    use crate::theme::Appearance;

    #[test]
    fn legacy_settings_default_to_system_without_losing_state() {
        let original = Store {
            roots: vec!["/projects".into()],
            disabled: ["o/off".into()].into(),
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
            disabled: ["o/off".into()].into(),
            pending: vec![pr("o/r", 1, Some(T1)), pr("o/r", 2, Some(T1))],
            snoozed: vec![snooze("o/r", 1, Some(T1))],
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

    fn pr(repo: &str, number: u64, requested_at: Option<&str>) -> PendingReview {
        PendingReview {
            host: default_host(),
            account: "alice".into(),
            account_id: 1,
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
            account: "alice".into(),
            account_id: 1,
            repo: repo.to_string(),
            number,
            until: 100,
            requested_at: requested_at.map(str::to_string),
        }
    }

    const T1: &str = "2026-01-01T00:00:00Z";
    const T2: &str = "2026-01-02T00:00:00Z";

    #[test]
    fn enterprise_state_migration_keeps_host_state_and_discards_unowned_public_state() {
        let mut json = serde_json::to_value(fixture_store()).unwrap();
        json["disabled"] =
            serde_json::json!(["github.com/owner/off", "github.example.com/owner/off"]);
        let mut legacy = json["pending"][0].clone();
        legacy.as_object_mut().unwrap().remove("account");
        legacy.as_object_mut().unwrap().remove("account_id");
        legacy["host"] = serde_json::json!("github.example.com");
        json["pending"].as_array_mut().unwrap().push(legacy.clone());
        legacy["host"] = serde_json::json!("github.com");
        json["pending"].as_array_mut().unwrap().push(legacy);
        let mut snooze = json["snoozed"][0].clone();
        snooze.as_object_mut().unwrap().remove("account");
        snooze.as_object_mut().unwrap().remove("account_id");
        snooze["host"] = serde_json::json!("github.example.com");
        json["snoozed"].as_array_mut().unwrap().push(snooze);
        json["notification_queue"] = serde_json::json!([
            {"host":"github.example.com","repo":"owner/repo","number":7,"requested_at":"2026-10-07T10:00:00Z","sequence":42},
            {"repo":"owner/repo","number":7,"requested_at":"2026-10-07T10:00:00Z","sequence":43}
        ]);
        let store = Store::from_json(&json.to_string()).unwrap();
        assert_eq!(store.appearance, Appearance::System);
        assert_eq!(store.pending.len(), 3);
        assert_eq!(store.snoozed.len(), 2);
        assert_eq!(store.notification_queue.len(), 1);
        assert_eq!(store.notification_queue[0].host, "github.example.com");
        assert!(!store.is_enabled("owner/off"));
        assert!(!store.is_enabled("github.example.com/owner/off"));
        assert!(store.is_enabled("acme.ghe.com/owner/off"));
        assert_eq!(store.account_ids.len(), 2);
        let restarted = Store::from_json(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(restarted.pending, store.pending);
        assert_eq!(restarted.snoozed, store.snoozed);
    }

    #[test]
    fn chosen_durations_and_unsnooze_are_independent_across_hosts() {
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
            local_repos: [
                "owner/repo".into(),
                "github.example.com/owner/repo".into(),
                "acme.ghe.com/owner/repo".into(),
            ]
            .into(),
            available_accounts: ["alice".into()].into(),
            ..Store::default()
        };
        assert_ne!(public.key(), server.key());
        assert_ne!(server.key(), cloud.key());
        assert!(store.snooze(&public.key(), 5, 1000));
        assert!(store.snooze(&server.key(), 30, 1000));
        assert!(store.snooze(&cloud.key(), 120, 1000));
        assert_eq!(store.snooze_for(&public).unwrap().until, 1300);
        assert_eq!(store.snooze_for(&server).unwrap().until, 2800);
        store.unsnooze(&public.key());
        assert_eq!(store.snoozed.len(), 2);
        assert_eq!(store.take_expired(2800), vec![server]);
        assert_eq!(store.snooze_for(&cloud).unwrap().until, 8200);
    }

    #[test]
    fn failed_hosts_keep_cache_and_snoozes_while_healthy_accounts_reconcile() {
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
        let watched = [
            "owner/repo".into(),
            "github.example.com/owner/repo".into(),
            "acme.ghe.com/owner/repo".into(),
        ]
        .into();
        let new_cloud = PendingReview {
            requested_at: Some(T2.into()),
            ..cloud
        };
        let changes = store.reconcile_scopes(
            vec![new_cloud.clone()],
            &[("alice".into(), 1)].into(),
            &["acme.ghe.com".into()].into(),
            &watched,
        );
        assert_eq!(changes.fresh, vec![new_cloud]);
        assert!(store.pending.contains(&server));
        assert!(!store.pending.contains(&public));
        assert_eq!(store.snoozed.len(), 1);
        store.reconcile_scopes(vec![], &Default::default(), &[server.host].into(), &watched);
        assert!(store.snoozed.is_empty());
        // Removing a cached Enterprise repo clears it even if that host is down.
        store.reconcile_scopes(
            vec![],
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        assert!(store.pending.is_empty());
    }

    #[test]
    fn independent_durations_survive_restart_and_expire_in_deadline_order() {
        let long = pr("Owner/Repo", 1, Some(T1));
        let short = pr("Owner/Repo", 2, Some(T1));
        let mut store = Store {
            pending: vec![long.clone(), short.clone()],
            snooze_minutes: 15,
            available_accounts: std::collections::BTreeSet::from(["alice".into()]),
            local_repos: std::collections::BTreeSet::from(["owner/repo".into(), "o/r".into()]),
            ..Store::default()
        };
        assert!(store.snooze(&long.key(), 120, 1_000));
        assert!(store.snooze(&short.key(), 5, 1_000));
        assert_eq!(store.snooze_minutes, 15);
        assert!(store.awake().is_empty());
        assert_eq!(store.next_snooze_until(), Some(1_300));

        // The production save/load format retains absolute deadlines and requests.
        let json = serde_json::to_vec(&store).unwrap();
        let mut restarted = Store::from_json(std::str::from_utf8(&json).unwrap()).unwrap();
        restarted.available_accounts = store.available_accounts.clone();
        restarted.local_repos = store.local_repos.clone();
        assert_eq!(restarted.snoozed, store.snoozed);
        assert_eq!(restarted.snooze_minutes, 15);
        assert!(
            !restarted
                .reconcile(
                    store.pending.clone(),
                    &std::collections::BTreeMap::from([("alice".into(), 1)])
                )
                .snoozes_changed
        );
        assert!(restarted.take_expired(1_299).is_empty());
        assert_eq!(restarted.take_expired(1_300), vec![short.clone()]);
        assert_eq!(restarted.awake(), vec![short]);
        assert_eq!(restarted.next_snooze_until(), Some(8_200));
        assert!(restarted.take_expired(8_199).is_empty());
        assert_eq!(restarted.take_expired(8_200), vec![long]);
        assert_eq!(restarted.next_snooze_until(), None);

        // Starting after both deadlines also wakes both reviews in one batch.
        let mut overdue = Store::from_json(std::str::from_utf8(&json).unwrap()).unwrap();
        overdue.available_accounts = store.available_accounts.clone();
        overdue.local_repos = store.local_repos.clone();
        assert_eq!(overdue.take_expired(9_000), store.pending);
        assert!(overdue.snoozed.is_empty());
    }

    #[test]
    fn resnooze_replaces_deadline_and_unsnooze_does_not_wake_it_again() {
        let review = pr("o/r", 1, Some(T1));
        let mut store = Store {
            pending: vec![review.clone()],
            available_accounts: std::collections::BTreeSet::from(["alice".into()]),
            local_repos: std::collections::BTreeSet::from(["owner/repo".into(), "o/r".into()]),
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
        assert!(!store.snooze(&(1, "o/r".into(), 2), 5, 1_000));
        assert!(store.snoozed.is_empty());
    }

    #[test]
    fn rerequest_cancels_only_its_snooze_and_retargets_the_next_wake() {
        let mut store = Store {
            pending: vec![pr("o/r", 1, Some(T1)), pr("o/r", 2, Some(T1))],
            available_accounts: std::collections::BTreeSet::from(["alice".into()]),
            local_repos: std::collections::BTreeSet::from(["owner/repo".into(), "o/r".into()]),
            ..Store::default()
        };
        assert!(store.snooze(&(1, "o/r".into(), 1), 5, 1_000));
        assert!(store.snooze(&(1, "o/r".into(), 2), 60, 1_000));
        let fresh = pr("o/r", 1, Some(T2));
        let result = store.reconcile(
            vec![fresh.clone(), pr("o/r", 2, Some(T1))],
            &std::collections::BTreeMap::from([("alice".into(), 1)]),
        );
        assert!(result.snoozes_changed);
        assert_eq!(result.fresh, vec![fresh.clone()]);
        assert_eq!(store.awake(), vec![fresh]);
        assert_eq!(store.next_snooze_until(), Some(4_600));
        assert!(store.take_expired(1_300).is_empty());
        assert_eq!(store.take_expired(4_600), vec![pr("o/r", 2, Some(T1))]);
    }

    #[test]
    fn legacy_snooze_migration_preserves_missing_settings_defaults() {
        let store = Store::from_json(
            r#"{"snoozed":[{"repo":"o/r","number":1,"until":1300,"requested_at":null}]}"#,
        )
        .unwrap();
        assert_eq!(store.snooze_minutes, 5);
        // Legacy snoozes lack a receiving identity and cannot be reassigned.
        assert!(store.snoozed.is_empty());
        assert_eq!(store.next_snooze_until(), None);
    }

    #[test]
    fn missing_request_timestamp_preserves_snooze_until_newer_request_or_withdrawal() {
        let review = pr("Owner/Repo", 1, Some(T1));
        let mut store = Store {
            pending: vec![review.clone()],
            available_accounts: std::collections::BTreeSet::from(["alice".into()]),
            local_repos: std::collections::BTreeSet::from(["owner/repo".into()]),
            ..Store::default()
        };
        store.snooze(&review.key(), 5, 1_000);
        let deadline = store.snoozed[0].clone();
        let result = store.reconcile(
            vec![pr("Owner/Repo", 1, None)],
            &std::collections::BTreeMap::from([("alice".into(), 1)]),
        );
        assert!(result.fresh.is_empty());
        assert!(!result.pending_changed);
        assert!(!result.snoozes_changed);
        assert_eq!(store.pending, vec![review.clone()]);
        assert_eq!(store.snoozed, vec![deadline]);
        let newer = pr("Owner/Repo", 1, Some(T2));
        assert_eq!(
            store
                .reconcile(
                    vec![newer.clone()],
                    &std::collections::BTreeMap::from([("alice".into(), 1)])
                )
                .fresh,
            vec![newer.clone()]
        );
        assert!(store.snoozed.is_empty());
        store.snooze(&newer.key(), 5, 1_000);
        store.reconcile(
            vec![],
            &std::collections::BTreeMap::from([("alice".into(), 1)]),
        );
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
            let reconciled = store.reconcile(
                case.fetched.clone(),
                &std::collections::BTreeMap::from([("alice".into(), 1)]),
            );
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
        let reconciled = store.reconcile(
            vec![
                pr("o/r", 1, Some(T1)),
                pr("o/r", 2, None),
                pr("o/r", 3, Some(T2)),
            ],
            &std::collections::BTreeMap::from([("alice".into(), 1)]),
        );
        let order: Vec<u64> = store.pending.iter().map(|pr| pr.number).collect();
        assert_eq!(order, vec![3, 1, 2]);
        let fresh: Vec<u64> = reconciled.fresh.iter().map(|pr| pr.number).collect();
        assert_eq!(fresh, vec![3, 1, 2]);
    }

    #[test]
    fn key_lowercases_repo() {
        assert_eq!(
            pr("Owner/Repo", 7, None).key(),
            (1, "owner/repo".to_string(), 7)
        );
    }

    #[test]
    fn take_expired_wakes_snoozes_that_ran_out() {
        let mut store = Store {
            pending: vec![pr("o/r", 1, Some(T1)), pr("o/r", 2, Some(T1))],
            available_accounts: std::collections::BTreeSet::from(["alice".into()]),
            local_repos: std::collections::BTreeSet::from(["o/r".into()]),
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

    fn fixture_store() -> Store {
        Store::from_json(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap()
    }

    #[test]
    fn legacy_migration_keeps_preferences_without_guessing_an_identity() {
        let store = Store::from_json(include_str!("../tests/fixtures/legacy-state.json")).unwrap();
        assert_eq!(store.roots, vec![std::path::PathBuf::from("/fixtures/dev")]);
        assert!(store.disabled.contains("owner/off"));
        assert_eq!((store.poll_minutes, store.snooze_minutes), (15, 30));
        assert!(store.pending.is_empty());
        assert!(store.snoozed.is_empty());
    }

    #[test]
    fn login_preferences_migrate_to_ids_and_do_not_follow_a_reused_login() {
        let mut json: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/multiple-account-state.json"
        ))
        .unwrap();
        json["disabled_accounts"] = serde_json::json!(["alice"]);
        json["repo_accounts"] = serde_json::json!({"owner/repo": ["alice"], "owner/paused": []});
        let mut store = Store::from_json(&json.to_string()).unwrap();
        assert!(store.disabled_account_ids.contains(&1));
        assert!(store.disabled_accounts.is_empty());
        store.record_account("alice-renamed", 1);
        store.record_account("alice", 99);
        assert!(!store.account_enabled("alice-renamed"));
        assert!(store.account_enabled("alice"));
        assert!(store.repo_account_selected("owner/repo", "alice-renamed"));
        assert!(!store.repo_account_selected("owner/repo", "alice"));
        assert!(!store.all_repo_accounts("owner/paused"));
        store.toggle_account("alice-renamed");
        assert!(store.account_enabled("alice-renamed"));
        store.toggle_repo_account("owner/repo", "alice-renamed");
        assert!(!store.repo_account_selected("owner/repo", "alice-renamed"));
        store.reset_repo_accounts("owner/repo");
        assert!(store.all_repo_accounts("owner/repo"));
    }

    #[test]
    fn unverified_preferences_bind_only_when_an_account_is_verified() {
        let mut store = Store::from_json(
            r#"{
            "known_accounts":["alice","bob"], "disabled_accounts":["alice"],
            "repo_accounts":{"owner/repo":["alice"]}
        }"#,
        )
        .unwrap();
        assert!(!store.account_enabled("alice"));
        assert!(store.disabled_account_ids.is_empty());
        store.record_account("alice", 1);
        store.record_account("bob", 2);
        store.record_account("alice-renamed", 1);
        assert!(!store.account_enabled("alice-renamed"));
        assert!(store.repo_account_selected("owner/repo", "alice-renamed"));
        assert!(!store.repo_account_selected("owner/repo", "bob"));
        store.reset_repo_accounts("owner/repo");
        store.toggle_repo_account("owner/repo", "bob");
        assert!(store.repo_account_selected("owner/repo", "alice-renamed"));
        assert!(!store.repo_account_selected("owner/repo", "bob"));
    }

    #[test]
    fn invalid_appearance_keeps_account_state_through_production_migration() {
        let mut original = fixture_store();
        original.toggle_account("alice");
        original.toggle_repo_account("owner/repo", "bob");
        let mut json = serde_json::to_value(&original).unwrap();
        json["appearance"] = serde_json::json!({"unsupported": true});
        let restored = Store::from_json(&json.to_string()).unwrap();
        assert_eq!(restored.appearance, Appearance::System);
        assert_eq!(restored.pending, original.pending);
        assert_eq!(restored.snoozed, original.snoozed);
        assert_eq!(restored.account_ids, original.account_ids);
        assert_eq!(restored.disabled_account_ids, original.disabled_account_ids);
        assert_eq!(restored.repo_account_ids, original.repo_account_ids);
        assert!(!restored.account_enabled("alice"));
        assert!(restored.repo_account_selected("owner/repo", "alice"));
        assert!(!restored.repo_account_selected("owner/repo", "bob"));
    }

    #[test]
    fn persisted_accounts_start_hidden_and_do_not_persist_session_availability() {
        let mut store = fixture_store();
        assert_eq!(store.pending.len(), 2);
        assert!(store.visible_pending().is_empty());
        store.local_repos.insert("owner/repo".into());
        store
            .available_accounts
            .extend(["alice".into(), "bob".into()]);
        assert_eq!(store.visible_pending().len(), 2);
        let saved = serde_json::to_string(&store).unwrap();
        assert!(!saved.contains("available_accounts"));
        assert!(!saved.contains("local_repos"));
        let reloaded = Store::from_json(&saved).unwrap();
        assert!(reloaded.awake().is_empty());
        assert_eq!(reloaded.pending, store.pending);
        assert_eq!(reloaded.snoozed, store.snoozed);
    }

    #[test]
    fn same_pr_under_two_accounts_has_independent_snoozes_and_freshness() {
        let mut store = fixture_store();
        store.local_repos.insert("owner/repo".into());
        store
            .available_accounts
            .extend(["alice".into(), "bob".into()]);
        assert_ne!(store.pending[0].key(), store.pending[1].key());
        assert_eq!(
            store
                .awake()
                .iter()
                .map(|pr| pr.account.as_str())
                .collect::<Vec<_>>(),
            vec!["bob"]
        );
        let mut fetched = store.pending.clone();
        fetched[1].requested_at = Some("2026-10-07T11:00:00Z".into());
        let changes = store.reconcile(
            fetched,
            &std::collections::BTreeMap::from([("alice".into(), 1), ("bob".into(), 2)]),
        );
        assert_eq!(changes.fresh.len(), 1);
        assert_eq!(changes.fresh[0].account, "bob");
        assert_eq!(store.snoozed.len(), 1);
        assert_eq!(store.snoozed[0].account, "alice");
    }

    #[test]
    fn unavailable_account_retains_cache_and_expired_snooze_until_recovery() {
        let mut store = fixture_store();
        store.local_repos.insert("owner/repo".into());
        store.available_accounts.insert("bob".into());
        store.snoozed[0].until = 50;
        let bob = store.pending[1].clone();
        let changes = store.reconcile(
            vec![bob],
            &std::collections::BTreeMap::from([("bob".into(), 2)]),
        );
        assert!(changes.fresh.is_empty());
        assert_eq!(store.pending.len(), 2);
        assert_eq!(store.visible_pending().len(), 1);
        assert!(store.take_expired(100).is_empty());
        assert_eq!(store.snoozed.len(), 1);
        store.available_accounts.insert("alice".into());
        let fetched = store.pending.clone();
        store.reconcile(
            fetched,
            &std::collections::BTreeMap::from([("alice".into(), 1), ("bob".into(), 2)]),
        );
        let woken = store.take_expired(100);
        assert_eq!(woken.len(), 1);
        assert_eq!(woken[0].account, "alice");
        assert!(store.snoozed.is_empty());
    }

    #[test]
    fn account_and_repository_controls_hide_without_crossing_partitions() {
        let mut store = fixture_store();
        store.local_repos.insert("owner/repo".into());
        store
            .available_accounts
            .extend(["alice".into(), "bob".into()]);
        store.disabled_accounts.insert("alice".into());
        assert_eq!(store.visible_pending()[0].account, "bob");
        store.disabled_accounts.clear();
        store.repo_accounts.insert(
            "owner/repo".into(),
            std::collections::BTreeSet::from(["alice".into()]),
        );
        assert_eq!(store.visible_pending().len(), 1);
        assert_eq!(store.visible_pending()[0].account, "alice");
        assert!(store.awake().is_empty());
        store.repo_accounts.get_mut("owner/repo").unwrap().clear();
        assert!(store.visible_pending().is_empty());
        store.repo_accounts.clear();
        assert_eq!(store.visible_pending().len(), 2);
        store.disabled.insert("owner/repo".into());
        assert!(store.visible_pending().is_empty());
        assert_eq!(store.snoozed.len(), 1);
    }

    #[test]
    fn reused_login_cannot_inherit_another_users_snooze() {
        let mut store = fixture_store();
        let mut new_user = store.pending[0].clone();
        new_user.account_id = 99;
        let changes = store.reconcile(
            vec![new_user],
            &std::collections::BTreeMap::from([("alice".into(), 99)]),
        );
        assert_eq!(changes.fresh.len(), 1);
        assert!(store.snoozed.is_empty());
        assert_eq!(
            store
                .pending
                .iter()
                .filter(|pr| pr.account == "alice")
                .count(),
            1
        );
    }

    #[test]
    fn renamed_login_keeps_identity_without_duplicate_cached_requests() {
        let mut store = fixture_store();
        let mut renamed = store.pending[0].clone();
        renamed.account = "alice-renamed".into();
        let changes = store.reconcile(
            vec![renamed],
            &std::collections::BTreeMap::from([("alice-renamed".into(), 1)]),
        );
        assert!(changes.fresh.is_empty());
        assert_eq!(
            store.pending.iter().filter(|pr| pr.account_id == 1).count(),
            1
        );
        assert_eq!(store.snoozed[0].account, "alice-renamed");
        assert!(changes.snoozes_changed);
    }

    #[test]
    fn per_request_durations_and_wake_scheduling_stay_independent_across_accounts() {
        let mut store = fixture_store();
        store.snoozed.clear();
        store.local_repos.insert("owner/repo".into());
        store
            .available_accounts
            .extend(["alice".into(), "bob".into()]);
        let alice = store.pending[0].clone();
        let bob = store.pending[1].clone();
        let default_minutes = store.snooze_minutes;
        assert!(store.snooze(&alice.key(), 120, 1_000));
        assert!(store.snooze(&bob.key(), 5, 1_000));
        assert_eq!(store.snooze_minutes, default_minutes);
        assert_eq!(store.next_snooze_until(), Some(1_300));
        assert_eq!(store.snoozed.len(), 2);

        store.available_accounts.remove("bob");
        assert_eq!(store.next_snooze_until(), Some(8_200));
        assert!(!store.snooze(&bob.key(), 30, 1_100));
        assert!(store.take_expired(1_500).is_empty());
        assert_eq!(store.snooze_for(&bob).unwrap().until, 1_300);
        store.available_accounts.insert("bob".into());
        assert_eq!(store.take_expired(1_500), vec![bob]);
        assert_eq!(store.next_snooze_until(), Some(8_200));
        store.unsnooze(&alice.key());
        assert_eq!(store.snoozed.len(), 0);
        assert_eq!(store.next_snooze_until(), None);
    }
}
