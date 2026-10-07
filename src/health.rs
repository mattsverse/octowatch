//! Presentation policy shared by setup, the header, and review empty states.

#[cfg(test)]
use crate::github::Check;
use crate::github::{FetchedReviews, Readiness};
#[cfg(test)]
use crate::store::PendingReview;
use crate::store::Store;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct SyncHealth {
    pub readiness: Readiness,
    pub hosts: BTreeMap<String, Readiness>,
    pub error: Option<String>,
    /// Disk caches and results against a previous folder list are unverified.
    pub verified: bool,
    pub connection_failed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewsStatus {
    Loading,
    Setup,
    Unavailable,
    Offline,
    Stale,
    Ready,
}

impl ReviewsStatus {
    pub fn tray_message(&self) -> Option<&'static str> {
        match self {
            Self::Loading => Some("Checking setup and reviews…"),
            Self::Setup => Some("Setup needs attention · open Octowatcher"),
            Self::Unavailable => Some("Sync failed · reviews may be stale"),
            Self::Offline => Some("GitHub unreachable · reviews may be stale"),
            Self::Stale => Some("Reviews may be stale · Refresh Now"),
            Self::Ready => None,
        }
    }
    pub fn empty_message(&self) -> &'static str {
        match self {
            Self::Loading => "Checking setup and reviews…",
            Self::Setup => "Choose a folder with GitHub clones and enable a repository to watch.",
            Self::Unavailable => {
                "Cannot confirm whether reviews are waiting. Check setup and connection, then Refresh."
            }
            Self::Offline => {
                "GitHub cannot be reached. Check your connection, then Refresh to confirm whether reviews are waiting."
            }
            Self::Stale => "No cached reviews. Refresh to confirm whether reviews are waiting.",
            Self::Ready => "Nothing waiting on your review.",
        }
    }
}

impl SyncHealth {
    /// Reconcile identity per host even when its review query fails. Aggregate
    /// success advances only when every enabled host completed this sync.
    pub fn apply_hosts(
        &mut self,
        fetched: &FetchedReviews,
        store: &mut Store,
        now: i64,
    ) -> BTreeSet<String> {
        let mut changed = BTreeSet::new();
        for (host, readiness) in &fetched.readiness {
            if let Readiness::Ready(login) = readiness
                && store.activate_host_account(host, login)
            {
                changed.insert(host.clone());
            }
        }
        self.hosts = fetched.readiness.clone();
        self.readiness = self
            .hosts
            .get("github.com")
            .or_else(|| self.hosts.values().next())
            .cloned()
            .unwrap_or_default();
        self.error = (!fetched.errors.is_empty()).then(|| fetched.errors.join("\n"));
        self.connection_failed =
            self.hosts
                .values()
                .any(|ready| matches!(ready, Readiness::Offline))
                || fetched.errors.iter().any(|error| {
                    crate::github::is_connection_error(&anyhow::anyhow!(error.clone()))
                });
        self.verified = !self.hosts.is_empty()
            && self.error.is_none()
            && fetched.successful_hosts.len() == self.hosts.len();
        if self.verified {
            store.last_successful_sync = Some(now);
        }
        changed
    }

    /// Only a complete review result advances success. Account invalidation
    /// happens before the result, so a failed query cannot leave another
    /// account's cache (or snoozes) active.
    #[cfg(test)]
    pub fn apply(
        &mut self,
        check: Check,
        store: &mut Store,
        now: i64,
    ) -> (Option<Vec<PendingReview>>, bool) {
        let mut results = FetchedReviews::default();
        results
            .readiness
            .insert("github.com".into(), check.readiness);
        let fetched = match check.reviews {
            Ok(reviews) => {
                results.successful_hosts.insert("github.com".into());
                results.pending = reviews.clone();
                Some(reviews)
            }
            Err(error) => {
                results.errors.push(format!("{error:#}"));
                None
            }
        };
        let changed = self.apply_hosts(&results, store, now);
        (fetched, changed.contains("github.com"))
    }

    pub fn status(
        &self,
        store: &Store,
        now: i64,
        scanning: bool,
        checking: bool,
        watching: usize,
        folder_issues: bool,
    ) -> ReviewsStatus {
        if scanning {
            return ReviewsStatus::Loading;
        }
        if watching == 0 || folder_issues {
            return ReviewsStatus::Setup;
        }
        if self.connection_failed {
            return ReviewsStatus::Offline;
        }
        if self.error.is_some()
            || matches!(
                self.readiness,
                Readiness::Missing
                    | Readiness::NotRunnable
                    | Readiness::SignedOut
                    | Readiness::Offline
                    | Readiness::Unavailable
            )
        {
            return ReviewsStatus::Unavailable;
        }
        if !self.verified {
            return if checking {
                ReviewsStatus::Loading
            } else {
                ReviewsStatus::Stale
            };
        }
        let stale_after = store
            .poll_minutes
            .max(1)
            .saturating_mul(120)
            .saturating_add(60);
        if store
            .last_successful_sync
            .is_none_or(|at| now.saturating_sub(at) as u64 > stale_after)
        {
            ReviewsStatus::Stale
        } else {
            ReviewsStatus::Ready
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_store() -> Store {
        let mut store: Store = serde_json::from_str(r#"{
            "roots":[], "sync_account":"alice", "last_successful_sync":100,
            "pending":[{"repo":"o/r","number":1,"title":"Review","url":"https://github.com/o/r/pull/1","author":"other","is_draft":false,"rereview":false,"requested_at":null}],
            "snoozed":[{"repo":"o/r","number":1,"until":10000,"requested_at":null}]
        }"#).unwrap();
        store.poll_minutes = 2;
        store
    }

    #[test]
    fn partial_host_failure_keeps_cache_and_success_time_until_all_hosts_recover() {
        let mut store = cached_store();
        let public = store.pending[0].clone();
        let enterprise = PendingReview {
            host: "github.example.com".into(),
            ..public.clone()
        };
        store.activate_host_account(&enterprise.host, "enterprise-viewer");
        store.pending.push(enterprise.clone());
        store.last_successful_sync = Some(100);
        let mut health = SyncHealth::default();
        let partial = FetchedReviews {
            readiness: [
                ("github.com".into(), Readiness::Ready("alice".into())),
                (enterprise.host.clone(), Readiness::Offline),
            ]
            .into(),
            successful_hosts: ["github.com".into()].into(),
            pending: Vec::new(),
            errors: vec!["github.example.com: dial tcp: network is unreachable".into()],
            ..FetchedReviews::default()
        };
        assert!(health.apply_hosts(&partial, &mut store, 200).is_empty());
        store.reconcile_repositories(
            partial.pending,
            &[public.repository()].into(),
            &[public.repository(), enterprise.repository()].into(),
        );
        assert_eq!(store.pending, vec![enterprise.clone()]);
        assert_eq!(store.last_successful_sync, Some(100));
        assert!(!health.verified);
        let recovered = FetchedReviews {
            readiness: [
                ("github.com".into(), Readiness::Ready("alice".into())),
                (
                    enterprise.host.clone(),
                    Readiness::Ready("enterprise-viewer".into()),
                ),
            ]
            .into(),
            successful_hosts: ["github.com".into(), enterprise.host].into(),
            ..FetchedReviews::default()
        };
        assert!(health.apply_hosts(&recovered, &mut store, 300).is_empty());
        assert_eq!(store.last_successful_sync, Some(300));
        assert!(health.verified);
        assert!(health.error.is_none());
    }

    #[test]
    fn enterprise_account_change_preserves_public_cache_and_legacy_identity() {
        let mut store = cached_store();
        let public = store.pending[0].clone();
        let enterprise = PendingReview {
            host: "github.example.com".into(),
            ..public.clone()
        };
        store.activate_host_account(&enterprise.host, "old-viewer");
        store.pending.push(enterprise.clone());
        store.queue_notifications(&[public.clone(), enterprise.clone()]);
        let fetched = FetchedReviews {
            readiness: [(
                enterprise.host.clone(),
                Readiness::Ready("new-viewer".into()),
            )]
            .into(),
            errors: vec!["github.example.com: review query failed".into()],
            ..FetchedReviews::default()
        };
        let changed = SyncHealth::default().apply_hosts(&fetched, &mut store, 200);
        assert_eq!(changed, [enterprise.host.clone()].into());
        assert_eq!(store.pending, vec![public]);
        assert_eq!(store.snoozed.len(), 1);
        assert_eq!(store.notification_queue.len(), 1);
        assert_eq!(store.sync_account.as_deref(), Some("alice"));
        let restored: Store =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(restored.sync_accounts[&enterprise.host], "new-viewer");
        assert_eq!(restored.sync_account.as_deref(), Some("alice"));
    }

    #[test]
    fn failure_keeps_cache_and_success_time_then_recovers() {
        let mut store = cached_store();
        let mut health = SyncHealth {
            verified: true,
            ..SyncHealth::default()
        };
        let (reviews, changed) = health.apply(
            Check {
                readiness: Readiness::Unavailable,
                reviews: Err(anyhow::anyhow!("offline")),
            },
            &mut store,
            200,
        );
        assert!(reviews.is_none());
        assert!(!changed);
        assert_eq!(store.pending.len(), 1);
        assert_eq!(store.snoozed.len(), 1);
        assert_eq!(store.last_successful_sync, Some(100));
        assert_eq!(
            health.status(&store, 200, false, false, 1, false),
            ReviewsStatus::Unavailable
        );
        let (reviews, changed) = health.apply(
            Check {
                readiness: Readiness::Ready("alice".into()),
                reviews: Ok(Vec::new()),
            },
            &mut store,
            300,
        );
        assert!(!changed);
        store.reconcile(reviews.unwrap());
        assert_eq!(store.last_successful_sync, Some(300));
        assert!(store.pending.is_empty());
        assert!(health.error.is_none());
        assert_eq!(
            health.status(&store, 300, false, false, 1, false),
            ReviewsStatus::Ready
        );
    }

    #[test]
    fn account_change_clears_cache_even_when_reviews_fail() {
        let mut store = cached_store();
        let mut health = SyncHealth::default();
        let (reviews, changed) = health.apply(
            Check {
                readiness: Readiness::Ready("bob".into()),
                reviews: Err(anyhow::anyhow!("query failed")),
            },
            &mut store,
            200,
        );
        assert!(changed);
        assert!(reviews.is_none());
        assert!(store.pending.is_empty());
        assert!(store.snoozed.is_empty());
        assert_eq!(store.sync_account.as_deref(), Some("bob"));
        assert_eq!(store.last_successful_sync, None);
        assert!(!health.verified);
    }

    #[test]
    fn legacy_cache_cannot_be_assigned_to_an_unverified_account() {
        let mut store = cached_store();
        store.sync_account = None;
        assert!(store.activate_account("alice"));
        assert!(store.pending.is_empty());
        assert!(store.snoozed.is_empty());
        assert_eq!(store.last_successful_sync, None);
    }

    #[test]
    fn network_failure_after_identity_lookup_is_unreachable_then_recovers() {
        let mut store = cached_store();
        let mut health = SyncHealth::default();
        health.apply(
            Check {
                readiness: Readiness::Ready("alice".into()),
                reviews: Err(anyhow::anyhow!("dial tcp: network is unreachable")),
            },
            &mut store,
            200,
        );
        assert_eq!(
            health.status(&store, 200, false, false, 1, false),
            ReviewsStatus::Offline
        );
        assert_eq!(store.last_successful_sync, Some(100));
        assert_eq!(store.pending.len(), 1);
        health.apply(
            Check {
                readiness: Readiness::Ready("alice".into()),
                reviews: Ok(Vec::new()),
            },
            &mut store,
            300,
        );
        assert_eq!(
            health.status(&store, 300, false, false, 1, false),
            ReviewsStatus::Ready
        );
        assert!(!health.connection_failed);
    }

    #[test]
    fn empty_states_do_not_claim_success_for_setup_loading_or_stale_data() {
        let store = cached_store();
        let health = SyncHealth::default();
        assert_eq!(
            health.status(&store, 110, true, false, 0, false),
            ReviewsStatus::Loading
        );
        assert_eq!(
            health.status(&store, 110, false, false, 0, false),
            ReviewsStatus::Setup
        );
        assert_eq!(
            health.status(&store, 110, false, true, 1, false),
            ReviewsStatus::Loading
        );
        assert_eq!(
            health.status(&store, 110, false, false, 1, false),
            ReviewsStatus::Stale
        );
        let health = SyncHealth {
            verified: true,
            readiness: Readiness::Ready("alice".into()),
            error: None,
            ..SyncHealth::default()
        };
        assert_eq!(
            health.status(&store, 400, false, false, 1, false),
            ReviewsStatus::Ready
        );
        assert_eq!(
            health.status(&store, 401, false, false, 1, false),
            ReviewsStatus::Stale
        );
        assert_eq!(
            health.status(&store, 110, false, false, 1, true),
            ReviewsStatus::Setup
        );
        for status in [
            ReviewsStatus::Loading,
            ReviewsStatus::Setup,
            ReviewsStatus::Unavailable,
            ReviewsStatus::Offline,
            ReviewsStatus::Stale,
        ] {
            assert_ne!(status.empty_message(), "Nothing waiting on your review.");
        }
    }
}
