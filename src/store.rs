use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::discovery::LocalRepo;

/// Everything that survives a restart, saved as JSON in the platform config dir.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    /// Folders scanned for local git clones.
    pub roots: Vec<PathBuf>,
    /// Last discovered checkouts, retained when their folders are unavailable.
    pub discovered: Vec<LocalRepo>,
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
}

impl Default for Store {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            discovered: Vec::new(),
            disabled: BTreeSet::new(),
            pending: Vec::new(),
            poll_minutes: 2,
            snooze_minutes: 5,
            snoozed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
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
    pub fn key(&self) -> (String, u64) {
        (self.repo.to_lowercase(), self.number)
    }
}

/// A pending review hidden from the list and the tray until `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snooze {
    /// `owner/name`, lowercase.
    pub repo: String,
    pub number: u64,
    /// Unix seconds when the review comes back.
    pub until: i64,
    /// The request that was snoozed; a newer one ends the snooze.
    pub requested_at: Option<String>,
}

impl Snooze {
    pub fn key(&self) -> (String, u64) {
        (self.repo.clone(), self.number)
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
            .and_then(|json| serde_json::from_str::<Self>(&json).ok())
            .unwrap_or_default();
        if store.roots.is_empty() {
            store.roots = default_roots();
        }
        store
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
            .filter(|pr| self.snooze_for(pr).is_none())
            .cloned()
            .collect()
    }

    /// Replaces the pending list with what GitHub reports now, newest first.
    /// A PR that drops out (reviewed, request removed, closed) is gone, and
    /// so is its snooze.
    pub fn reconcile(&mut self, mut fetched: Vec<PendingReview>) -> Reconciled {
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
        Reconciled {
            fresh,
            pending_changed,
            snoozes_changed,
        }
    }

    /// Drop reviews and snoozes for repos no longer watched, even when GitHub
    /// cannot be reached. Local discovery alone never produces fresh requests.
    pub fn retain_watched(&mut self, watched: &HashSet<String>) -> Reconciled {
        let retained = self
            .pending
            .iter()
            .filter(|pr| watched.contains(&pr.repo.to_lowercase()))
            .cloned()
            .collect();
        self.reconcile(retained)
    }

    /// Ends every snooze that ran out by `now`, and returns the pending
    /// reviews they hid.
    pub fn take_expired(&mut self, now: i64) -> Vec<PendingReview> {
        let (expired, remaining): (Vec<Snooze>, Vec<Snooze>) = std::mem::take(&mut self.snoozed)
            .into_iter()
            .partition(|s| s.until <= now);
        self.snoozed = remaining;
        self.pending
            .iter()
            .filter(|pr| expired.iter().any(|s| s.key() == pr.key()))
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
    fn discovery_cache_is_compatible_with_old_state_and_preserves_toggles() {
        use crate::discovery::{LocalRepo, discover};
        let old = r#"{"roots":["/missing/watched"],"disabled":["owner/repo"],"poll_minutes":10,"snooze_minutes":15}"#;
        let mut store: Store = serde_json::from_str(old).unwrap();
        assert!(store.discovered.is_empty());
        assert_eq!(store.poll_minutes, 10);
        assert_eq!(store.snooze_minutes, 15);
        store.discovered.push(LocalRepo {
            slug: "Owner/Repo".into(),
            paths: vec!["/missing/watched/clone".into()],
        });
        let saved = serde_json::to_string(&store).unwrap();
        let restored: Store = serde_json::from_str(&saved).unwrap();
        let mut scan = discover(&restored.roots);
        scan.retain_unavailable(&restored.discovered);
        assert_eq!(scan.repos, store.discovered);
        assert!(!restored.is_enabled(&scan.repos[0].slug));
        assert_eq!(restored.disabled, store.disabled);
    }

    #[test]
    fn local_scan_removes_reviews_and_snoozes_without_new_notifications() {
        let mut store = Store {
            pending: vec![pr("Owner/Kept", 1, Some(T1)), pr("o/removed", 2, Some(T2))],
            snoozed: vec![
                snooze("owner/kept", 1, Some(T1)),
                snooze("o/removed", 2, Some(T2)),
            ],
            ..Store::default()
        };
        let changed = store.retain_watched(&["owner/kept".into()].into());
        assert!(changed.fresh.is_empty());
        assert!(changed.pending_changed);
        assert!(changed.snoozes_changed);
        assert_eq!(store.pending, vec![pr("Owner/Kept", 1, Some(T1))]);
        assert_eq!(store.snoozed, vec![snooze("owner/kept", 1, Some(T1))]);
        let unchanged = store.retain_watched(&["owner/kept".into()].into());
        assert!(!unchanged.pending_changed);
        assert!(!unchanged.snoozes_changed);
    }

    #[test]
    fn key_lowercases_repo() {
        assert_eq!(
            pr("Owner/Repo", 7, None).key(),
            ("owner/repo".to_string(), 7)
        );
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
}
