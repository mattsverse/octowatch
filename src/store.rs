use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Everything that survives a restart, saved as JSON in the platform config dir.
#[derive(Debug, Serialize, Deserialize)]
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
    /// Minutes a snoozed review stays hidden.
    pub snooze_minutes: u64,
    /// Reviews the user put aside for now.
    pub snoozed: Vec<Snooze>,
    /// Saved gh accounts seen before, including accounts since signed out.
    pub known_accounts: BTreeSet<String>,
    pub disabled_accounts: BTreeSet<String>,
    /// Missing entry means all enabled accounts; an empty set means none.
    pub repo_accounts: BTreeMap<String, BTreeSet<String>>,
    /// Successful checks in this session, never trusted across a restart.
    #[serde(skip)]
    pub available_accounts: BTreeSet<String>,
    /// Discovered repos in this session, never trusted across a restart.
    #[serde(skip)]
    pub local_repos: BTreeSet<String>,
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
            disabled_accounts: BTreeSet::new(),
            repo_accounts: BTreeMap::new(),
            available_accounts: BTreeSet::new(),
            local_repos: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
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
        (self.account_id, self.repo.to_lowercase(), self.number)
    }
}

/// Stable github.com user ID, normalized repository, and PR number.
pub type ReviewKey = (u64, String, u64);

/// A pending review hidden from the list and the tray until `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snooze {
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
        (self.account_id, self.repo.to_lowercase(), self.number)
    }
}

/// What `Store::reconcile` changed, so the caller knows which side effects to run.
#[derive(Debug, PartialEq, Eq)]
pub struct Reconciled {
    /// Reviews requested for the first time, or requested again.
    pub fresh: Vec<PendingReview>,
    pub pending_changed: bool,
    pub snoozes_changed: bool,
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
        store
            .pending
            .retain(|pr| !pr.account.is_empty() && pr.account_id != 0);
        store
            .snoozed
            .retain(|s| !s.account.is_empty() && s.account_id != 0);
        Ok(store)
    }

    pub fn account_enabled(&self, account: &str) -> bool {
        !self.disabled_accounts.contains(&account.to_lowercase())
    }

    pub fn monitors(&self, account: &str, repo: &str) -> bool {
        self.account_enabled(account)
            && self.is_enabled(repo)
            && self
                .repo_accounts
                .get(&repo.to_lowercase())
                .is_none_or(|accounts| accounts.contains(&account.to_lowercase()))
    }

    pub fn visible(&self, pr: &PendingReview) -> bool {
        self.available_accounts.contains(&pr.account.to_lowercase())
            && self.local_repos.contains(&pr.repo.to_lowercase())
            && self.monitors(&pr.account, &pr.repo)
    }

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

    /// Pending reviews that aren't snoozed.
    pub fn awake(&self) -> Vec<PendingReview> {
        self.pending
            .iter()
            .filter(|pr| self.visible(pr) && self.snooze_for(pr).is_none())
            .cloned()
            .collect()
    }

    /// Replaces the pending list with what GitHub reports now, newest first.
    /// A PR that drops out (reviewed, request removed, closed) is gone, and
    /// so is its snooze.
    pub fn reconcile(
        &mut self,
        mut fetched: Vec<PendingReview>,
        checked_accounts: &BTreeMap<String, u64>,
    ) -> Reconciled {
        // Failed, disabled, or signed-out accounts retain their own cache.
        fetched.extend(
            self.pending
                .iter()
                .filter(|pr| {
                    !checked_accounts.contains_key(&pr.account.to_lowercase())
                        && !checked_accounts.values().any(|id| *id == pr.account_id)
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
        let fresh: Vec<PendingReview> = fetched
            .iter()
            .filter(|pr| match known.get(&pr.key()) {
                None => checked_accounts.contains_key(&pr.account.to_lowercase()),
                Some(previous) => {
                    checked_accounts.contains_key(&pr.account.to_lowercase())
                        && pr.requested_at > *previous
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
        Reconciled {
            fresh,
            pending_changed,
            snoozes_changed,
        }
    }

    /// Ends every snooze that ran out by `now`, and returns the pending
    /// reviews they hid.
    pub fn take_expired(&mut self, now: i64) -> Vec<PendingReview> {
        let (expired, remaining): (Vec<Snooze>, Vec<Snooze>) = std::mem::take(&mut self.snoozed)
            .into_iter()
            .partition(|s| {
                s.until <= now && self.available_accounts.contains(&s.account.to_lowercase())
            });
        self.snoozed = remaining;
        self.pending
            .iter()
            .filter(|pr| self.visible(pr) && expired.iter().any(|s| s.key() == pr.key()))
            .cloned()
            .collect()
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

    fn pr(repo: &str, number: u64, requested_at: Option<&str>) -> PendingReview {
        PendingReview {
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

    /// Pins how `reconcile` behaves today. Some rows (`None` to `Some`, and
    /// `Some` to `None`) record current behaviour, not necessarily intended.
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
                // Freshness compares with `>`, the snooze retain with `==`.
                name: "Some to None is not fresh but ends the snooze",
                pending: vec![pr("o/r", 1, Some(T1))],
                snoozed: vec![snooze("o/r", 1, Some(T1))],
                fetched: vec![pr("o/r", 1, None)],
                fresh: vec![],
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
}
