//! Review delivery and action routing, independent of the desktop UI.

use std::collections::HashSet;

use crate::{
    notifications::Response,
    store::{PendingReview, ReviewNotice, Store},
};

#[derive(Default)]
pub struct Delivery {
    in_flight: bool,
    /// Saved reviews require GitHub confirmation once in each app session.
    unchecked: HashSet<(String, u64)>,
}

pub struct Batch {
    pub notices: Vec<ReviewNotice>,
    pub reviews: Vec<PendingReview>,
}

impl Delivery {
    pub fn for_launch(store: &Store) -> Self {
        Self {
            unchecked: store.pending.iter().map(PendingReview::key).collect(),
            ..Self::default()
        }
    }

    /// Seed launch alerts only for saved requests confirmed by this check.
    /// Keep unchecked requests and failed deliveries persisted, but held.
    pub fn confirm(&mut self, store: &mut Store, confirmed: &[PendingReview]) {
        self.unchecked
            .retain(|key| store.pending.iter().any(|pr| pr.key() == *key));
        let newly_checked: Vec<_> = confirmed
            .iter()
            .filter_map(|fetched| {
                let current = store.pending.iter().find(|pr| pr.key() == fetched.key())?;
                // An older partial result cannot validate a newer saved request.
                if fetched.requested_at.is_some() && fetched.requested_at != current.requested_at {
                    return None;
                }
                if !self.unchecked.remove(&current.key())
                    || store.snooze_for(current).is_some()
                    || store
                        .notification_queue
                        .iter()
                        .any(|notice| notice.matches(current))
                {
                    return None;
                }
                Some(current.clone())
            })
            .collect();
        store.queue_notifications(&newly_checked);
    }

    /// Only one send can consume the persisted queue at a time. Responses
    /// don't keep this gate locked: delivery and interaction are separate.
    pub fn begin(&mut self, store: &Store) -> Option<Batch> {
        if self.in_flight {
            return None;
        }
        let due: Vec<_> = store
            .notifications_due()
            .into_iter()
            .filter(|(_, pr)| !self.unchecked.contains(&pr.key()))
            .collect();
        if due.is_empty() {
            return None;
        }
        self.in_flight = true;
        let (notices, reviews) = due.into_iter().unzip();
        Some(Batch { notices, reviews })
    }

    /// Drain reviews queued during a successful send without waiting for a
    /// poll or user action. Failures release the gate but never retry in a loop.
    pub fn complete(&mut self, store: &mut Store, batch: &Batch, delivered: bool) -> Option<Batch> {
        if delivered {
            store.mark_delivered(&batch.notices);
        }
        self.in_flight = false;
        if delivered { self.begin(store) } else { None }
    }

    pub fn deferred(&mut self) {
        self.in_flight = false;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewAction {
    OpenPr(String),
    OpenReviews,
    Snooze(PendingReview),
}

#[derive(Clone)]
pub enum Target {
    Single(PendingReview),
    Summary,
}

impl Target {
    pub fn for_reviews(reviews: &[PendingReview]) -> Self {
        match reviews {
            [pr] => Self::Single(pr.clone()),
            _ => Self::Summary,
        }
    }

    pub fn action(&self) -> crate::notifications::Action {
        match self {
            Self::Single(_) => ("snooze", "Snooze"),
            Self::Summary => ("open-reviews", "Open Reviews"),
        }
    }

    pub fn respond(&self, response: Response, store: &Store) -> Option<ReviewAction> {
        match (self, response) {
            (Self::Single(pr), Response::Clicked) => Some(ReviewAction::OpenPr(pr.url.clone())),
            // A late action must not snooze a withdrawn or newer re-review.
            (Self::Single(pr), Response::Action(id))
                if id == "snooze"
                    && store.pending.iter().any(|current| {
                        current.key() == pr.key() && current.requested_at == pr.requested_at
                    }) =>
            {
                Some(ReviewAction::Snooze(pr.clone()))
            }
            (Self::Summary, Response::Clicked) => Some(ReviewAction::OpenReviews),
            (Self::Summary, Response::Action(id)) if id == "open-reviews" => {
                Some(ReviewAction::OpenReviews)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Snooze;

    const T1: &str = "2026-01-01T00:00:00Z";
    const T2: &str = "2026-01-02T00:00:00Z";

    fn pr(number: u64, draft: bool) -> PendingReview {
        PendingReview {
            repo: "Owner/Repo".into(),
            number,
            title: format!("Review {number}"),
            url: format!("https://github.com/Owner/Repo/pull/{number}"),
            author: "someone".into(),
            is_draft: draft,
            rereview: false,
            requested_at: Some(T1.into()),
        }
    }

    fn snooze(pr: &PendingReview, until: i64) -> Snooze {
        Snooze {
            repo: pr.key().0,
            number: pr.number,
            requested_at: pr.requested_at.clone(),
            until,
        }
    }

    fn round_trip(store: &Store) -> Store {
        serde_json::from_str(&serde_json::to_string(store).unwrap()).unwrap()
    }

    #[test]
    fn partial_launch_delivers_only_confirmed_reviews_and_defers_saved_retries() {
        use std::collections::HashSet;

        for confirmed_in_same_repo in [false, true] {
            let cached = pr(1, false);
            let mut confirmed = pr(2, false);
            if !confirmed_in_same_repo {
                confirmed.repo = "Other/Repo".into();
            }
            let mut store = Store::default();
            store.reconcile(vec![cached.clone()]);
            // A failed delivery from the previous run must also wait for validation.
            store = round_trip(&store);
            let mut delivery = Delivery::for_launch(&store);
            let watched = HashSet::from([cached.key().0, confirmed.key().0]);
            store.reconcile_repositories(vec![confirmed.clone()], &HashSet::new(), &watched);
            delivery.confirm(&mut store, std::slice::from_ref(&confirmed));
            let batch = delivery.begin(&store).unwrap();
            assert_eq!(batch.reviews, vec![confirmed.clone()]);
            // A successful send must not drain the unchecked saved alert.
            assert!(delivery.complete(&mut store, &batch, true).is_none());
            assert_eq!(store.notification_queue.len(), 1);
            // Resume and expired snoozes cannot bypass the validation gate.
            store.notifications_muted = true;
            store.snooze(&cached.key(), 5, 0);
            store.take_expired(300);
            store.notifications_muted = false;
            assert!(delivery.begin(&store).is_none());
            // A later complete snapshot proves the cached review has gone.
            store.reconcile_repositories(vec![confirmed], &watched, &watched);
            assert!(delivery.begin(&store).is_none());
            assert!(store.notification_queue.is_empty());
        }
    }

    #[test]
    fn saved_launch_reviews_notify_once_when_later_confirmed() {
        use std::collections::HashSet;

        for failed_retry in [false, true] {
            let cached = pr(1, false);
            let confirmed = pr(2, false);
            let mut store = Store {
                pending: vec![cached.clone()],
                notify_drafts: false,
                ..Store::default()
            };
            if failed_retry {
                store.queue_notifications(std::slice::from_ref(&cached));
            }
            let original_notices = store.notification_queue.clone();
            let mut delivery = Delivery::for_launch(&store);
            let watched = HashSet::from([cached.key().0]);
            store.reconcile_repositories(vec![confirmed.clone()], &HashSet::new(), &watched);
            delivery.confirm(&mut store, std::slice::from_ref(&confirmed));
            let first = delivery.begin(&store).unwrap();
            assert_eq!(first.reviews, vec![confirmed.clone()]);
            assert!(delivery.complete(&mut store, &first, true).is_none());

            // Validation while suppressed still seeds an alert for later release.
            let draft = PendingReview {
                is_draft: true,
                ..cached.clone()
            };
            store.reconcile_repositories(vec![draft.clone()], &HashSet::new(), &watched);
            delivery.confirm(&mut store, std::slice::from_ref(&draft));
            assert!(delivery.begin(&store).is_none());
            if failed_retry {
                assert_eq!(store.notification_queue, original_notices);
            }
            store.reconcile_repositories(
                vec![cached.clone(), confirmed.clone()],
                &watched,
                &watched,
            );
            delivery.confirm(&mut store, &[cached.clone(), confirmed.clone()]);
            let retry = delivery.begin(&store).unwrap();
            assert_eq!(retry.reviews, vec![cached.clone()]);
            assert!(delivery.complete(&mut store, &retry, false).is_none());
            let retry = delivery.begin(&store).unwrap();
            assert!(delivery.complete(&mut store, &retry, true).is_none());
            delivery.confirm(&mut store, &[cached, confirmed]);
            assert!(delivery.begin(&store).is_none());
        }
    }

    #[test]
    fn older_partial_request_cannot_release_a_newer_saved_launch_alert() {
        use std::collections::HashSet;

        let older = pr(1, false);
        let newer = PendingReview {
            requested_at: Some(T2.into()),
            ..older.clone()
        };
        let mut store = Store::default();
        store.reconcile(vec![newer.clone()]);
        let mut delivery = Delivery::for_launch(&store);
        store.reconcile_repositories(
            vec![older.clone()],
            &HashSet::new(),
            &HashSet::from([older.key().0]),
        );
        delivery.confirm(&mut store, &[older]);
        assert!(delivery.begin(&store).is_none());
        assert_eq!(store.pending, vec![newer.clone()]);
        delivery.confirm(&mut store, std::slice::from_ref(&newer));
        assert_eq!(delivery.begin(&store).unwrap().reviews, vec![newer]);
    }

    #[test]
    fn failed_delivery_survives_polls_and_restart_then_recovers() {
        let review = pr(1, false);
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![review.clone()]);
        let failed = delivery.begin(&store).unwrap();
        // Both service errors and denied permission are failed deliveries.
        assert!(delivery.complete(&mut store, &failed, false).is_none());
        store = round_trip(&store);
        assert!(store.reconcile(vec![review.clone()]).fresh.is_empty());
        let retried = delivery.begin(&store).unwrap();
        assert_eq!(failed.notices, retried.notices);
        assert!(delivery.complete(&mut store, &retried, true).is_none());
        assert!(store.notification_queue.is_empty());
        store.reconcile(vec![review]);
        assert!(delivery.begin(&store).is_none());
    }

    #[test]
    fn missing_request_timestamp_preserves_failed_delivery_and_deduplication() {
        let review = pr(1, false);
        let without_timestamp = PendingReview {
            requested_at: None,
            ..review.clone()
        };
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![review.clone()]);
        let failed = delivery.begin(&store).unwrap();
        assert!(delivery.complete(&mut store, &failed, false).is_none());
        let result = store.reconcile(vec![without_timestamp.clone()]);
        assert!(result.fresh.is_empty());
        assert!(!result.notifications_changed);
        store = round_trip(&store);
        let retry = delivery.begin(&store).unwrap();
        assert_eq!(retry.notices, failed.notices);
        assert_eq!(retry.reviews, vec![review.clone()]);
        assert!(delivery.complete(&mut store, &retry, true).is_none());
        for fetched in [without_timestamp, review.clone()] {
            assert!(store.reconcile(vec![fetched]).fresh.is_empty());
            assert!(delivery.begin(&store).is_none());
        }
        // A genuinely later request must still get its own alert.
        let newer = PendingReview {
            requested_at: Some(T2.into()),
            ..review
        };
        assert_eq!(
            store.reconcile(vec![newer.clone()]).fresh,
            vec![newer.clone()]
        );
        assert_eq!(delivery.begin(&store).unwrap().reviews, vec![newer]);
    }

    #[test]
    fn successful_send_drains_reviews_queued_in_flight_without_another_poll() {
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, false)]);
        let first = delivery.begin(&store).unwrap();
        store.reconcile(vec![pr(1, false), pr(2, false)]);
        assert!(delivery.begin(&store).is_none());
        let next = delivery.complete(&mut store, &first, true).unwrap();
        assert_eq!(next.reviews, vec![pr(2, false)]);
        assert!(delivery.begin(&store).is_none()); // Next send owns the gate.
        assert!(delivery.complete(&mut store, &next, true).is_none());
        assert!(store.notification_queue.is_empty());
    }

    #[test]
    fn failed_send_does_not_immediately_retry_reviews_queued_in_flight() {
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, false)]);
        let first = delivery.begin(&store).unwrap();
        store.reconcile(vec![pr(1, false), pr(2, false)]);
        assert!(delivery.complete(&mut store, &first, false).is_none());
        assert_eq!(store.notification_queue.len(), 2);
        store.reconcile(store.pending.clone());
        let retry = delivery.begin(&store).unwrap();
        assert_eq!(retry.reviews, vec![pr(1, false), pr(2, false)]);
        assert!(delivery.complete(&mut store, &retry, true).is_none());
    }

    #[test]
    fn completion_respects_mute_drafts_and_snoozes_changed_during_send() {
        let mut store = Store {
            notify_drafts: false,
            ..Store::default()
        };
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, false)]);
        let first = delivery.begin(&store).unwrap();
        store.reconcile(vec![pr(1, false), pr(2, false), pr(3, true), pr(4, false)]);
        store.snooze(&pr(4, false).key(), 5, 0);
        store.notifications_muted = true;
        assert!(delivery.complete(&mut store, &first, true).is_none());
        store.notifications_muted = false;
        let catch_up = delivery.begin(&store).unwrap();
        assert_eq!(catch_up.reviews, vec![pr(2, false)]);
        assert!(delivery.complete(&mut store, &catch_up, true).is_none());
        assert_eq!(store.notification_queue.len(), 1); // Suppressed draft.
    }

    #[test]
    fn repeated_polls_do_not_duplicate_in_flight_or_accepted_delivery() {
        let review = pr(1, false);
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![review.clone()]);
        let batch = delivery.begin(&store).unwrap();
        for _ in 0..5 {
            store.reconcile(vec![review.clone()]);
            assert!(delivery.begin(&store).is_none());
            assert_eq!(store.notification_queue.len(), 1);
        }
        // No click, dismiss, or Snooze response is needed to acknowledge delivery.
        assert!(delivery.complete(&mut store, &batch, true).is_none());
        let target = Target::for_reviews(&batch.reviews);
        assert!(target.respond(Response::Dismissed, &store).is_none());
        store.reconcile(vec![review]);
        assert!(delivery.begin(&store).is_none());
    }

    #[test]
    fn mute_keeps_queue_current_and_resume_batches_only_eligible_requests() {
        let mut store = Store {
            notifications_muted: true,
            notify_drafts: false,
            ..Store::default()
        };
        let mut delivery = Delivery::default();
        store.reconcile(vec![
            pr(1, false),
            pr(2, false),
            pr(3, true),
            pr(4, false),
            pr(5, false),
        ]);
        store.snoozed.push(snooze(&pr(4, false), 100));
        assert!(delivery.begin(&store).is_none());
        assert_eq!(store.pending.len(), 5);
        assert_eq!(store.awake().len(), 4); // Includes the draft.
        store = round_trip(&store);
        assert!(store.notifications_muted);
        // #5 was resolved while muted and must not appear on resume.
        store.reconcile(vec![pr(1, false), pr(2, false), pr(3, true), pr(4, false)]);
        store.notifications_muted = false;
        let batch = delivery.begin(&store).unwrap();
        assert_eq!(
            batch.reviews.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(delivery.complete(&mut store, &batch, true).is_none());
        assert!(delivery.begin(&store).is_none());
        assert_eq!(store.notification_queue.len(), 2); // Suppressed draft and snooze.
    }

    #[test]
    fn draft_suppression_defers_until_ready_without_a_new_request_timestamp() {
        let mut store = Store {
            notify_drafts: false,
            ..Store::default()
        };
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, true)]);
        assert!(delivery.begin(&store).is_none());
        assert_eq!(store.awake().len(), 1);
        store.reconcile(vec![pr(1, true)]);
        assert_eq!(store.notification_queue.len(), 1);
        store = round_trip(&store);
        assert!(!store.notify_drafts);
        assert!(store.reconcile(vec![pr(1, false)]).fresh.is_empty());
        let batch = delivery.begin(&store).unwrap();
        assert!(!batch.reviews[0].is_draft);
        assert!(delivery.complete(&mut store, &batch, true).is_none());
        store.reconcile(vec![pr(1, false)]);
        assert!(delivery.begin(&store).is_none());
    }

    #[test]
    fn enabled_drafts_notify_once_and_enabling_releases_suppressed_drafts() {
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, true)]);
        let batch = delivery.begin(&store).unwrap();
        assert!(delivery.complete(&mut store, &batch, true).is_none());
        store.reconcile(vec![pr(1, false)]);
        assert!(delivery.begin(&store).is_none());
        store.notify_drafts = false;
        store.reconcile(vec![pr(1, false), pr(2, true)]);
        assert!(delivery.begin(&store).is_none());
        store.notify_drafts = true;
        assert_eq!(delivery.begin(&store).unwrap().reviews, vec![pr(2, true)]);
    }

    #[test]
    fn startup_summary_waits_for_delivery_and_filters_drafts_and_snoozes() {
        let mut store = Store {
            pending: vec![pr(1, false), pr(2, false), pr(3, true), pr(4, false)],
            snoozed: vec![snooze(&pr(4, false), 100)],
            notify_drafts: false,
            ..Store::default()
        };
        let mut delivery = Delivery::for_launch(&store);
        assert!(store.reconcile(store.pending.clone()).fresh.is_empty());
        delivery.confirm(
            &mut store,
            &[pr(1, false), pr(2, false), pr(3, true), pr(4, false)],
        );
        let batch = delivery.begin(&store).unwrap();
        assert_eq!(batch.reviews, vec![pr(1, false), pr(2, false)]);
        assert!(matches!(
            Target::for_reviews(&batch.reviews),
            Target::Summary
        ));
        assert!(delivery.complete(&mut store, &batch, false).is_none());
        let retry = delivery.begin(&store).unwrap();
        assert_eq!(batch.notices, retry.notices);
        assert!(delivery.complete(&mut store, &retry, true).is_none());
        assert!(delivery.begin(&store).is_none());
        // Existing behavior: each new app launch summarizes the waiting reviews.
        store = round_trip(&store);
        delivery = Delivery::for_launch(&store);
        delivery.confirm(
            &mut store,
            &[pr(1, false), pr(2, false), pr(3, true), pr(4, false)],
        );
        assert_eq!(delivery.begin(&store).unwrap().reviews.len(), 2);
    }

    #[test]
    fn startup_muted_queue_survives_restart_and_prunes_withdrawn_requests() {
        let mut store = Store {
            pending: vec![pr(1, false), pr(2, false)],
            notifications_muted: true,
            ..Store::default()
        };
        let mut delivery = Delivery::for_launch(&store);
        delivery.confirm(&mut store, &[pr(1, false), pr(2, false)]);
        assert!(delivery.begin(&store).is_none());
        store = round_trip(&store);
        delivery = Delivery::for_launch(&store);
        store.reconcile(vec![pr(1, false)]);
        delivery.confirm(&mut store, &[pr(1, false)]);
        store.notifications_muted = false;
        assert_eq!(delivery.begin(&store).unwrap().reviews, vec![pr(1, false)]);
    }

    #[test]
    fn late_delivery_cannot_consume_a_newer_request() {
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, false)]);
        let old = delivery.begin(&store).unwrap();
        let newer = PendingReview {
            requested_at: Some(T2.into()),
            rereview: true,
            ..pr(1, false)
        };
        store.reconcile(vec![newer.clone()]);
        assert_eq!(store.notification_queue.len(), 1);
        let new = delivery.complete(&mut store, &old, true).unwrap();
        assert_eq!(new.reviews, vec![newer]);
        assert_ne!(old.notices, new.notices);
    }

    #[test]
    fn snooze_expiration_during_send_keeps_its_own_reminder() {
        let review = pr(1, false);
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![review.clone()]);
        let old = delivery.begin(&store).unwrap();
        store.discard_notification(&review.key());
        store.snoozed.push(snooze(&review, 100));
        assert!(store.notifications_due().is_empty());
        assert!(store.take_expired(99).is_empty());
        assert_eq!(store.take_expired(100), vec![review]);
        let reminder = delivery.complete(&mut store, &old, true).unwrap();
        assert_ne!(old.notices, reminder.notices);
        assert!(delivery.complete(&mut store, &reminder, true).is_none());
        assert!(store.take_expired(101).is_empty());
        assert!(delivery.begin(&store).is_none());
    }

    #[test]
    fn snooze_expiration_while_muted_is_delivered_on_resume() {
        let mut store = Store {
            pending: vec![pr(1, false)],
            notifications_muted: true,
            snoozed: vec![snooze(&pr(1, false), 100)],
            ..Store::default()
        };
        let mut delivery = Delivery::default();
        store.take_expired(100);
        assert_eq!(store.awake().len(), 1);
        assert!(delivery.begin(&store).is_none());
        store = round_trip(&store);
        store.notifications_muted = false;
        assert_eq!(delivery.begin(&store).unwrap().reviews, vec![pr(1, false)]);
    }

    #[test]
    fn outdated_and_disabled_requests_are_removed_from_delivery_queue() {
        let mut store = Store::default();
        store.reconcile(vec![pr(1, false), pr(2, true)]);
        store.pending.retain(|pr| pr.number != 1); // App's disable-repository seam.
        store.prune_notifications();
        assert_eq!(store.notification_queue.len(), 1);
        store.reconcile(vec![]);
        assert!(store.notification_queue.is_empty());
    }

    #[test]
    fn capacity_deferral_keeps_the_delivery_queue_retryable() {
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, false)]);
        let deferred = delivery.begin(&store).unwrap();
        delivery.deferred();
        assert_eq!(delivery.begin(&store).unwrap().notices, deferred.notices);
    }

    #[test]
    fn body_clicks_and_buttons_route_to_the_correct_destination() {
        let review = pr(1, false);
        let store = Store {
            pending: vec![review.clone(), pr(2, false)],
            ..Store::default()
        };
        let single = Target::for_reviews(std::slice::from_ref(&review));
        assert_eq!(single.action(), ("snooze", "Snooze"));
        assert_eq!(
            single.respond(Response::Clicked, &store),
            Some(ReviewAction::OpenPr(review.url.clone()))
        );
        assert_eq!(
            single.respond(Response::Action("snooze".into()), &store),
            Some(ReviewAction::Snooze(review))
        );
        let summary = Target::for_reviews(&[pr(1, false), pr(2, false)]);
        assert_eq!(summary.action(), ("open-reviews", "Open Reviews"));
        assert_eq!(
            summary.respond(Response::Clicked, &store),
            Some(ReviewAction::OpenReviews)
        );
        assert_eq!(
            summary.respond(Response::Action("open-reviews".into()), &store),
            Some(ReviewAction::OpenReviews)
        );
        assert_eq!(
            summary.respond(Response::Action("snooze".into()), &store),
            None
        );
        for target in [single, summary] {
            assert_eq!(target.respond(Response::Dismissed, &store), None);
            assert_eq!(
                target.respond(Response::Action("unknown".into()), &store),
                None
            );
        }
    }

    #[test]
    fn late_snooze_actions_cannot_hide_a_newer_or_withdrawn_request() {
        let old = pr(1, false);
        let target = Target::Single(old.clone());
        let mut store = Store::default();
        assert_eq!(
            target.respond(Response::Action("snooze".into()), &store),
            None
        );
        store.reconcile(vec![old.clone()]);
        assert_eq!(
            target.respond(Response::Action("snooze".into()), &store),
            Some(ReviewAction::Snooze(old.clone()))
        );
        store.reconcile(vec![PendingReview {
            requested_at: Some(T2.into()),
            ..old
        }]);
        assert_eq!(
            target.respond(Response::Action("snooze".into()), &store),
            None
        );
    }

    #[test]
    fn manual_unsnooze_stays_silent_after_repeated_polls() {
        let review = pr(1, false);
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![review.clone()]);
        assert!(store.snooze(&review.key(), 5, 0));
        assert!(delivery.begin(&store).is_none());
        // Unsnooze reveals the review and cancels any undelivered alert.
        store.unsnooze(&review.key());
        store.reconcile(vec![review]);
        assert_eq!(store.awake().len(), 1);
        assert!(delivery.begin(&store).is_none());
        assert!(store.take_expired(100).is_empty());
    }

    #[test]
    fn per_review_snoozes_cancel_queued_alerts_and_resume_only_expired_reminders() {
        let short = pr(1, false);
        let long = pr(2, false);
        let mut store = Store {
            notifications_muted: true,
            ..Store::default()
        };
        let mut delivery = Delivery::default();
        store.reconcile(vec![short.clone(), long.clone()]);
        assert_eq!(store.notification_queue.len(), 2);
        assert!(store.snooze(&short.key(), 5, 1_000));
        assert!(store.snooze(&long.key(), 120, 1_000));
        assert!(store.notification_queue.is_empty());
        store = round_trip(&store);
        assert_eq!(store.take_expired(1_300), vec![short.clone()]);
        assert!(delivery.begin(&store).is_none());
        store.notifications_muted = false;
        let batch = delivery.begin(&store).unwrap();
        assert_eq!(batch.reviews, vec![short]);
        assert!(delivery.complete(&mut store, &batch, true).is_none());
        assert!(delivery.begin(&store).is_none());
        assert_eq!(store.take_expired(8_200), vec![long.clone()]);
        let batch = delivery.begin(&store).unwrap();
        assert_eq!(batch.reviews, vec![long]);
        assert!(delivery.complete(&mut store, &batch, true).is_none());
        assert!(store.take_expired(9_000).is_empty());
        assert!(delivery.begin(&store).is_none());
        assert_eq!(store.snooze_minutes, 5);
    }

    #[test]
    fn old_settings_and_snoozes_keep_their_defaults_and_round_trip() {
        let old = serde_json::json!({
            "pending": [pr(1, true)],
            "snoozed": [snooze(&pr(1, true), 100)],
            "poll_minutes": 10,
            "snooze_minutes": 30,
            "disabled": ["other/repo"]
        });
        let store: Store = serde_json::from_value(old).unwrap();
        assert!(store.notify_drafts);
        assert!(!store.notifications_muted);
        assert!(store.notification_queue.is_empty());
        let restored = round_trip(&store);
        assert_eq!(restored.pending, store.pending);
        assert_eq!(restored.snoozed, store.snoozed);
        assert_eq!(restored.disabled, store.disabled);
        assert_eq!(restored.poll_minutes, 10);
        assert_eq!(restored.snooze_minutes, 30);
    }
}
