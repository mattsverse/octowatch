use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::repository::{RepositoryId, default_host};

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
        true
    }

    /// Brings a PR back without treating it as an expired snooze.
    pub fn unsnooze(&mut self, key: &(RepositoryId, u64)) {
        self.snoozed.retain(|s| s.key() != *key);
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

    /// Successful hosts replace their cached reviews. Failed hosts keep theirs;
    /// disabled or no-longer-local repositories still leave the list.
    pub fn reconcile_hosts(
        &mut self,
        mut fetched: Vec<PendingReview>,
        successful_hosts: &BTreeSet<String>,
        watched: &HashSet<RepositoryId>,
    ) -> Reconciled {
        fetched.retain(|pr| successful_hosts.contains(&pr.host.to_ascii_lowercase()));
        fetched.extend(
            self.pending
                .iter()
                .filter(|pr| !successful_hosts.contains(&pr.host.to_ascii_lowercase()))
                .cloned(),
        );
        fetched.retain(|pr| watched.contains(&pr.repository()));
        self.reconcile(fetched)
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
    use crate::repository::{RepositoryId, default_host};

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
    fn key_lowercases_repo() {
        assert_eq!(
            pr("Owner/Repo", 7, None).key(),
            (RepositoryId::new("github.com", "owner/repo"), 7)
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
        let healthy = [public.host.clone(), cloud.host.clone()]
            .into_iter()
            .collect();
        let new_cloud = PendingReview {
            requested_at: Some(T2.into()),
            ..cloud
        };
        let changes = store.reconcile_hosts(vec![new_cloud.clone()], &healthy, &watched);
        assert_eq!(changes.fresh, vec![new_cloud]);
        assert!(store.pending.contains(&server));
        assert!(!store.pending.contains(&public));
        assert_eq!(store.snoozed.len(), 1);
        assert!(!changes.snoozes_changed);
        // Recovery reporting no reviews clears the server and its snooze.
        let changes = store.reconcile_hosts(
            vec![],
            &[server.host.clone()].into_iter().collect(),
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
        store.reconcile_hosts(vec![], &Default::default(), &watched);
        assert_eq!(store.pending, vec![public]);
    }
}
