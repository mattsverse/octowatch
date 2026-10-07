//! Review delivery and action routing, independent of the desktop UI.

use crate::{
    notifications::Response,
    store::{PendingReview, ReviewNotice, Store},
};

#[derive(Default)]
pub struct Delivery {
    in_flight: bool,
}

pub struct Batch {
    pub notices: Vec<ReviewNotice>,
    pub reviews: Vec<PendingReview>,
}

impl Delivery {
    /// Only one send can consume the persisted queue at a time. Responses
    /// don't keep this gate locked: delivery and interaction are separate.
    pub fn begin(&mut self, store: &Store) -> Option<Batch> {
        if self.in_flight {
            return None;
        }
        let due = store.notifications_due();
        if due.is_empty() {
            return None;
        }
        self.in_flight = true;
        let (notices, reviews) = due.into_iter().unzip();
        Some(Batch { notices, reviews })
    }

    pub fn complete(&mut self, store: &mut Store, batch: &Batch, delivered: bool) {
        if delivered {
            store.mark_delivered(&batch.notices);
        }
        self.in_flight = false;
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
    fn failed_delivery_survives_polls_and_restart_then_recovers() {
        let review = pr(1, false);
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![review.clone()]);
        let failed = delivery.begin(&store).unwrap();
        // Both service errors and denied permission are failed deliveries.
        delivery.complete(&mut store, &failed, false);
        store = round_trip(&store);
        assert!(store.reconcile(vec![review.clone()]).fresh.is_empty());
        let retried = delivery.begin(&store).unwrap();
        assert_eq!(failed.notices, retried.notices);
        delivery.complete(&mut store, &retried, true);
        assert!(store.notification_queue.is_empty());
        store.reconcile(vec![review]);
        assert!(delivery.begin(&store).is_none());
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
        delivery.complete(&mut store, &batch, true);
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
        delivery.complete(&mut store, &batch, true);
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
        delivery.complete(&mut store, &batch, true);
        store.reconcile(vec![pr(1, false)]);
        assert!(delivery.begin(&store).is_none());
    }

    #[test]
    fn enabled_drafts_notify_once_and_enabling_releases_suppressed_drafts() {
        let mut store = Store::default();
        let mut delivery = Delivery::default();
        store.reconcile(vec![pr(1, true)]);
        let batch = delivery.begin(&store).unwrap();
        delivery.complete(&mut store, &batch, true);
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
        let mut delivery = Delivery::default();
        assert!(store.reconcile(store.pending.clone()).fresh.is_empty());
        store.queue_startup_notifications();
        let batch = delivery.begin(&store).unwrap();
        assert_eq!(batch.reviews, vec![pr(1, false), pr(2, false)]);
        assert!(matches!(
            Target::for_reviews(&batch.reviews),
            Target::Summary
        ));
        delivery.complete(&mut store, &batch, false);
        let retry = delivery.begin(&store).unwrap();
        assert_eq!(batch.notices, retry.notices);
        delivery.complete(&mut store, &retry, true);
        assert!(delivery.begin(&store).is_none());
        // Existing behavior: each new app launch summarizes the waiting reviews.
        store = round_trip(&store);
        store.queue_startup_notifications();
        assert_eq!(delivery.begin(&store).unwrap().reviews.len(), 2);
    }

    #[test]
    fn startup_muted_queue_survives_restart_and_prunes_withdrawn_requests() {
        let mut store = Store {
            pending: vec![pr(1, false), pr(2, false)],
            notifications_muted: true,
            ..Store::default()
        };
        let mut delivery = Delivery::default();
        store.queue_startup_notifications();
        assert!(delivery.begin(&store).is_none());
        store = round_trip(&store);
        store.reconcile(vec![pr(1, false)]);
        store.queue_startup_notifications();
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
        delivery.complete(&mut store, &old, true);
        let new = delivery.begin(&store).unwrap();
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
        delivery.complete(&mut store, &old, true);
        let reminder = delivery.begin(&store).unwrap();
        assert_ne!(old.notices, reminder.notices);
        delivery.complete(&mut store, &reminder, true);
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
        store.snoozed.push(snooze(&review, 100));
        assert!(delivery.begin(&store).is_none());
        // Unsnooze reveals the review and cancels any undelivered alert.
        store.snoozed.clear();
        store.discard_notification(&review.key());
        store.reconcile(vec![review]);
        assert_eq!(store.awake().len(), 1);
        assert!(delivery.begin(&store).is_none());
        assert!(store.take_expired(100).is_empty());
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
