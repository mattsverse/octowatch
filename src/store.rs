use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::PathBuf,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::theme::Appearance;

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
    /// Default minutes a snoozed review stays hidden.
    pub snooze_minutes: u64,
    /// Reviews the user put aside for now.
    pub snoozed: Vec<Snooze>,
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

/// A delivery event, independently persisted from the current review list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewNotice {
    pub repo: String,
    pub number: u64,
    pub requested_at: Option<String>,
    pub sequence: u64,
}

impl ReviewNotice {
    pub fn matches(&self, pr: &PendingReview) -> bool {
        self.repo == pr.repo.to_lowercase()
            && self.number == pr.number
            && self.requested_at == pr.requested_at
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

    /// Snoozes only this PR for the chosen duration, leaving the default alone.
    pub fn snooze(&mut self, key: &(String, u64), minutes: u64, now: i64) -> bool {
        let Some(pr) = self.pending.iter().find(|pr| pr.key() == *key) else {
            return false;
        };
        let snooze = Snooze {
            repo: key.0.clone(),
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
    pub fn unsnooze(&mut self, key: &(String, u64)) {
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
                repo: pr.repo.to_lowercase(),
                number: pr.number,
                requested_at: pr.requested_at.clone(),
                sequence: self.notification_sequence,
            });
        }
    }

    /// Seed the first successful poll after launch, including suppressed drafts.
    pub fn queue_startup_notifications(&mut self) {
        self.queue_notifications(&self.awake());
    }

    pub fn discard_notification(&mut self, key: &(String, u64)) {
        self.notification_queue
            .retain(|notice| (&notice.repo, notice.number) != (&key.0, key.1));
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
    use super::{PendingReview, Snooze, Store};
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
        assert!(!store.snooze(&("o/r".into(), 2), 5, 1_000));
        assert!(store.snoozed.is_empty());
    }

    #[test]
    fn rerequest_cancels_only_its_snooze_and_retargets_the_next_wake() {
        let mut store = Store {
            pending: vec![pr("o/r", 1, Some(T1)), pr("o/r", 2, Some(T1))],
            ..Store::default()
        };
        assert!(store.snooze(&("o/r".into(), 1), 5, 1_000));
        assert!(store.snooze(&("o/r".into(), 2), 60, 1_000));
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
