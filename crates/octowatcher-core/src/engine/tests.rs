use super::*;
use crate::repository;

fn review(number: u64) -> PendingReview {
    PendingReview {
        host: repository::default_host(),
        account: "alice".into(),
        account_id: 1,
        repo: "owner/repo".into(),
        number,
        title: format!("Review {number}"),
        url: format!("https://github.com/owner/repo/pull/{number}"),
        author: "author".into(),
        is_draft: false,
        rereview: false,
        requested_at: None,
    }
}

/// A settled session watching owner/repo with two cached reviews. No
/// startup, GitHub, config-file or notification side effects.
fn engine() -> Engine {
    let mut engine = Engine::settled(Store {
        pending: vec![review(1), review(2)],
        sync_account: Some("test-viewer".into()),
        last_successful_sync: Some(Local::now().timestamp()),
        recovery_blocked: Some("test state must not be written".into()),
        available_accounts: BTreeSet::from(["alice".into()]),
        available_hosts: BTreeSet::from([repository::default_host()]),
        local_repos: BTreeSet::from(["owner/repo".into()]),
        ..Default::default()
    });
    engine.repos = Some(vec![LocalRepo {
        id: RepositoryId::new("github.com", "owner/repo"),
        paths: Vec::new(),
    }]);
    engine.health = SyncHealth {
        readiness: github::Readiness::Ready("alice".into()),
        verified: true,
        ..SyncHealth::default()
    };
    engine
}

/// The repositories each started GitHub check covers.
fn fetches(effects: &[Effect]) -> Vec<BTreeSet<RepositoryId>> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Start(Job::Fetch { watched, .. }) => Some(watched.clone()),
            _ => None,
        })
        .collect()
}

fn notify_slots(effects: &[Effect]) -> Vec<usize> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Start(Job::Notify { slot, .. }) => Some(*slot),
            _ => None,
        })
        .collect()
}

fn empty_poll() -> github::Poll {
    github::Poll {
        accounts: vec![],
        checks: vec![],
        access_cache: github::AccessCache::default(),
    }
}

fn account_poll(checks: Vec<github::AccountCheck>) -> github::Poll {
    github::Poll {
        accounts: checks
            .iter()
            .map(|check| github::Account {
                login: check.account.clone(),
                state: "success".into(),
                active: check.account == "alice",
                account_id: check
                    .reviews
                    .as_ref()
                    .ok()
                    .map(|reviews| reviews.account_id),
            })
            .collect(),
        checks,
        access_cache: Default::default(),
    }
}

fn healthy_account(account: &str, id: u64, pending: Vec<PendingReview>) -> github::AccountCheck {
    github::AccountCheck {
        account: account.into(),
        access_errors: vec![],
        reviews: Ok(github::AccountReviews {
            account_id: id,
            pending,
            unavailable_repos: Default::default(),
            completed_repos: ["owner/repo".into()].into(),
        }),
    }
}

#[test]
fn startup_scans_checks_permission_and_waits_for_the_first_poll() {
    let mut engine = Engine::new(Store::default()).without_persistence();
    let effects = engine.start();
    let keys: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Start(job) => Some(job.key()),
            _ => None,
        })
        .collect();
    assert!(keys.contains(&JobKey::Timer(Timer::Poll)));
    assert!(keys.contains(&JobKey::Scan));
    assert!(keys.contains(&JobKey::Permission));
    assert!(keys.contains(&JobKey::Timer(Timer::Health)));
    // Nothing is fetched before the first scan says what to check.
    assert!(fetches(&effects).is_empty());
    assert!(engine.scanning());
}

#[test]
fn failed_notification_send_keeps_the_alert_queued_and_frees_its_slot() {
    let mut engine = engine();
    engine.store.queue_notifications(&[review(1)]);
    engine.deliver_reviews();
    let slots = notify_slots(&engine.take_effects());
    assert_eq!(slots, vec![0]);
    let effects = engine.handle(Event::NotificationShown {
        slot: 0,
        result: Err("service unavailable".into()),
    });
    assert!(
        notify_slots(&effects).is_empty(),
        "no retry until the next poll"
    );
    assert!(engine.notifications.is_empty());
    assert_eq!(engine.store.notification_queue.len(), 1);
    assert!(
        engine
            .errors(None)
            .contains(&"could not send notification: service unavailable")
    );
}

#[test]
fn account_switch_discards_queued_alerts_and_ignores_old_delivery_and_actions() {
    let mut app = engine();
    app.store.notifications_muted = true;
    app.store.notify_drafts = false;
    app.store.queue_notifications(&[review(1)]);
    app.store.notifications_muted = false;
    let old_generation = app.review_account_generation;
    let old_batch = app.review_delivery.begin(&app.store).unwrap();
    let old_target = Target::for_reviews(&old_batch.reviews);

    app.health.apply(
        github::Check {
            readiness: github::Readiness::Ready("another-viewer".into()),
            reviews: Err(anyhow::anyhow!("review query failed")),
        },
        &mut app.store,
        200,
    );
    app.reset_review_accounts(&[repository::default_host()].into());
    assert!(app.store.notification_queue.is_empty());
    assert!(app.store.available_hosts.is_empty());
    assert!(app.launch_summary);
    assert!(!app.store.notify_drafts);
    // The same PR can be requested by both accounts. Old callbacks must
    // neither snooze it nor unlock or consume the new account's batch.
    app.store.pending = vec![review(1)];
    app.store.available_hosts.insert(repository::default_host());
    app.store.queue_notifications(&[review(1)]);
    assert!(
        app.review_delivery.begin(&app.store).is_none(),
        "retain the in-flight send until its bounded acceptance callback"
    );
    let new_batch = app
        .complete_review_batch(old_generation, &old_batch, true)
        .unwrap();
    assert_eq!(app.store.notification_queue, new_batch.notices);
    assert!(app.review_delivery.begin(&app.store).is_none());
    assert_eq!(
        app.review_action(
            old_generation,
            &old_target,
            Response::Action("snooze".into())
        ),
        None
    );
    assert_eq!(
        app.review_action(
            app.review_account_generation,
            &old_target,
            Response::Action("snooze".into())
        ),
        Some(ReviewAction::Snooze(review(1)))
    );
    assert!(
        app.complete_review_batch(app.review_account_generation, &new_batch, true)
            .is_none()
    );
    assert!(app.store.notification_queue.is_empty());
}

#[test]
fn account_switch_keeps_unaffected_host_delivery_and_actions_working() {
    let mut app = engine();
    let enterprise = PendingReview {
        host: "github.example.com".into(),
        ..review(1)
    };
    app.store.pending.push(enterprise.clone());
    app.store.available_hosts.insert(enterprise.host.clone());
    app.store
        .queue_notifications(&[review(1), enterprise.clone()]);
    let generation = app.review_account_generation;
    let old_batch = app.review_delivery.begin(&app.store).unwrap();
    app.store
        .activate_host_account(&enterprise.host, "new-viewer");
    app.reset_review_accounts(&[enterprise.host.clone()].into());
    assert!(app.store.available_hosts.contains("github.com"));
    assert!(!app.store.available_hosts.contains(&enterprise.host));
    assert!(
        app.review_delivery.begin(&app.store).is_none(),
        "account switching must not duplicate a healthy host's in-flight alert"
    );
    assert!(
        app.complete_review_batch(generation, &old_batch, true)
            .is_none()
    );
    assert!(
        app.store.notification_queue.is_empty(),
        "accepted public alert is acknowledged"
    );
    assert_eq!(
        app.review_action(generation, &Target::Single(review(1)), Response::Clicked),
        Some(ReviewAction::OpenPr(review(1).url))
    );
    assert_eq!(
        app.review_action(generation, &Target::Single(enterprise), Response::Clicked),
        None
    );
}

#[test]
fn queued_delivery_waits_for_permission_startup_and_launch_validation() {
    let mut app = engine();
    app.store.queue_notifications(&[review(1)]);
    app.notifications_ready = false;
    app.deliver_reviews();
    assert!(app.notifications.is_empty());
    assert_eq!(app.store.notification_queue.len(), 1);
    app.notifications_ready = true;
    app.announced_launch = false;
    app.deliver_reviews();
    assert!(app.notifications.is_empty());
    assert_eq!(app.store.notification_queue.len(), 1);
    // Once this host validates, no gate was left acquired by startup.
    app.store.available_hosts.insert(repository::default_host());
    assert_eq!(
        app.review_delivery.begin(&app.store).unwrap().reviews,
        vec![review(1)]
    );
}

#[test]
fn enabling_repo_during_fetch_checks_it_without_waiting_for_a_poll() {
    let old = RepositoryId::new("github.com", "owner/old");
    let new = RepositoryId::new("github.com", "owner/new");
    for fail_first in [false, true] {
        let mut app = engine();
        app.store.pending.clear();
        app.store.disabled.insert("owner/new".into());
        app.repos = Some(vec![
            LocalRepo {
                id: old.clone(),
                paths: vec![],
            },
            LocalRepo {
                id: new.clone(),
                paths: vec![],
            },
        ]);
        app.fetch_reviews();
        assert_eq!(
            fetches(&app.take_effects()),
            vec![BTreeSet::from([old.clone()])]
        );
        // Reproduce the enable action's refresh while the first check runs.
        app.store.disabled.remove("owner/new");
        app.fetch_reviews();
        // Several requests during one check should coalesce into one follow-up.
        app.fetch_reviews();
        assert!(fetches(&app.take_effects()).is_empty());
        let effects = app.handle(Done::Fetched {
            public: if fail_first {
                Err(anyhow::anyhow!("first check failed"))
            } else {
                Ok(empty_poll())
            },
            enterprise: Ok(github::FetchedReviews::default()),
        });
        assert_eq!(
            fetches(&effects),
            vec![BTreeSet::from([old.clone(), new.clone()])],
            "the newly enabled repo must be checked before another timer tick"
        );
        let effects = app.handle(Done::Fetched {
            public: Ok(empty_poll()),
            enterprise: Ok(github::FetchedReviews::default()),
        });
        assert!(fetches(&effects).is_empty());
        assert!(!app.fetching());
    }
}

#[test]
fn known_account_caches_stay_stale_offline_but_never_deliver_before_confirmation() {
    let mut app = engine();
    app.store.record_account("alice", 1);
    let mut saved = app.store.clone();
    saved.roots = vec!["/projects".into()];
    saved.discovered = vec![LocalRepo {
        id: review(1).repository(),
        paths: vec!["/projects/clone".into()],
    }];
    saved.queue_notifications(&[review(1)]);
    app.store = Store::from_json(&serde_json::to_string(&saved).unwrap()).unwrap();
    app.health = SyncHealth::default();
    app.review_delivery = Delivery::for_launch(&app.store);
    assert_eq!(app.tray_reviews(), vec![review(1), review(2)]);
    assert!(app.review_delivery.begin(&app.store).is_none());
    app.reconcile(
        account_poll(vec![github::AccountCheck {
            account: "alice".into(),
            reviews: Err(anyhow::anyhow!("dial tcp: network is unreachable")),
            access_errors: vec![],
        }]),
        Default::default(),
        None,
    );
    assert_eq!(app.tray_reviews(), vec![review(1), review(2)]);
    assert_eq!(app.review_status(), ReviewsStatus::Offline);
    assert!(app.review_delivery.begin(&app.store).is_none());
    app.reconcile(
        account_poll(vec![github::AccountCheck {
            account: "alice".into(),
            reviews: Err(anyhow::anyhow!("GitHub CLI timed out after 30 seconds")),
            access_errors: vec![],
        }]),
        Default::default(),
        None,
    );
    assert_eq!(app.tray_reviews(), vec![review(1), review(2)]);
    assert!(app.review_delivery.begin(&app.store).is_none());
    app.store.notifications_muted = true;
    app.reconcile(
        account_poll(vec![healthy_account(
            "alice",
            1,
            vec![review(1), review(2)],
        )]),
        Default::default(),
        None,
    );
    assert_eq!(app.review_status(), ReviewsStatus::Ready);
    assert!(app.store.available_accounts.contains("alice"));
    assert_eq!(app.store.notification_queue.len(), 2);
}

#[test]
fn auth_failures_hide_only_that_account_and_reused_logins_do_not_inherit_caches() {
    let mut app = engine();
    let alice = review(1);
    let bob = PendingReview {
        account: "bob".into(),
        account_id: 2,
        ..alice.clone()
    };
    app.store.record_account("alice", 1);
    app.store.record_account("bob", 2);
    app.store.pending = vec![alice.clone(), bob.clone()];
    app.store.stale_accounts = ["alice".into(), "bob".into()].into();
    app.store.notifications_muted = true;
    let last_success = app.store.last_successful_sync;
    app.reconcile(
        account_poll(vec![
            github::AccountCheck {
                account: "alice".into(),
                reviews: Err(anyhow::anyhow!("Bad credentials (HTTP 401)")),
                access_errors: vec![],
            },
            healthy_account("bob", 2, vec![bob.clone()]),
        ]),
        Default::default(),
        None,
    );
    assert_eq!(app.tray_reviews(), vec![bob.clone()]);
    assert_eq!(app.store.pending.len(), 2);
    assert_eq!(app.store.last_successful_sync, last_success);
    assert_eq!(app.health.accounts["alice"], github::Readiness::SignedOut);
    assert!(!app.health.verified);
    let replacement = PendingReview {
        account_id: 99,
        ..alice.clone()
    };
    app.reconcile(
        account_poll(vec![
            healthy_account("alice", 99, vec![replacement.clone()]),
            healthy_account("bob", 2, vec![bob.clone()]),
        ]),
        Default::default(),
        None,
    );
    assert!(app.store.visible(&replacement));
    assert!(!app.store.visible(&alice));
    assert!(app.store.pending.contains(&alice));
    assert!(app.health.verified);
}

#[test]
fn offline_account_check_does_not_reveal_a_repository_with_known_access_failure() {
    let mut app = engine();
    app.store.record_account("alice", 1);
    app.store.notifications_muted = true;
    app.reconcile(
        account_poll(vec![github::AccountCheck {
            account: "alice".into(),
            access_errors: vec![("owner/repo".into(), "Forbidden (HTTP 403)".into())],
            reviews: Ok(github::AccountReviews {
                account_id: 1,
                pending: vec![],
                unavailable_repos: ["owner/repo".into()].into(),
                completed_repos: Default::default(),
            }),
        }]),
        Default::default(),
        None,
    );
    assert!(app.tray_reviews().is_empty());
    app.reconcile(
        account_poll(vec![github::AccountCheck {
            account: "alice".into(),
            reviews: Err(anyhow::anyhow!("dial tcp: network is unreachable")),
            access_errors: vec![],
        }]),
        Default::default(),
        None,
    );
    assert_eq!(app.store.pending.len(), 2);
    assert!(app.tray_reviews().is_empty());
    assert!(app.store.notifications_due().is_empty());
}

#[test]
fn confirmed_requests_can_finish_launch_announcement_during_a_partial_check() {
    let mut app = engine();
    app.announced_launch = false;
    app.store.pending = vec![review(1)];
    app.review_delivery = Delivery::for_launch(&app.store);
    app.store
        .snooze(&review(1).key(), 60, Local::now().timestamp());
    app.reconcile(
        empty_poll(),
        Default::default(),
        Some("GitHub unavailable".into()),
    );
    assert!(!app.announced_launch);
    app.reconcile(
        github::Poll {
            accounts: vec![github::Account {
                login: "alice".into(),
                state: "success".into(),
                active: true,
                account_id: Some(1),
            }],
            checks: vec![github::AccountCheck {
                account: "alice".into(),
                access_errors: vec![("owner/repo".into(), "unreadable team".into())],
                reviews: Ok(github::AccountReviews {
                    account_id: 1,
                    pending: vec![review(1)],
                    unavailable_repos: ["owner/repo".into()].into(),
                    completed_repos: Default::default(),
                }),
            }],
            access_cache: Default::default(),
        },
        Default::default(),
        None,
    );
    assert!(app.announced_launch);
}

#[test]
fn host_discovery_failure_retains_cache_but_honors_removed_roots() {
    let mut app = engine();
    app.repos = None;
    let public = review(1);
    let enterprise = PendingReview {
        host: "github.example.com".into(),
        ..public.clone()
    };
    app.store.pending = vec![public.clone(), enterprise.clone()];
    app.store
        .local_repos
        .insert(enterprise.repository().store_key());
    app.store.roots = vec!["/missing/kept".into()];
    app.store.discovered = vec![
        LocalRepo {
            id: public.repository(),
            paths: vec!["/missing/removed/clone".into()],
        },
        LocalRepo {
            id: enterprise.repository(),
            paths: vec!["/missing/kept/clone".into()],
        },
    ];
    app.store.queue_notifications(&app.store.pending.clone());
    app.store.snooze(&enterprise.key(), 120, 0);
    let snoozed = app.store.snoozed.clone();
    let changes = app
        .apply_discovery(Err(anyhow::anyhow!("host discovery failed")))
        .unwrap();
    assert!(changes.start_fetch);
    assert!(changes.cache_changed);
    assert!(changes.reviews.pending_changed && changes.reviews.notifications_changed);
    assert_eq!(app.store.pending, vec![enterprise.clone()]);
    assert_eq!(app.store.snoozed, snoozed);
    assert!(app.store.notification_queue.is_empty());
    assert_eq!(app.store.discovered[0].id, enterprise.repository());
    assert_eq!(app.repos.as_ref().unwrap(), &app.store.discovered);
    assert!(
        app.scan_error
            .as_deref()
            .unwrap()
            .contains("host discovery failed")
    );
}

#[test]
fn failed_discovery_still_checks_cached_hosts_in_both_completion_orders() {
    // Answers a started check with a healthy account that completed its repos.
    fn answer(watched: &BTreeSet<RepositoryId>) -> Done {
        let repos: Vec<_> = watched.iter().map(|repo| repo.slug.clone()).collect();
        Done::Fetched {
            public: Ok(github::Poll {
                accounts: vec![github::Account {
                    login: "alice".into(),
                    state: "success".into(),
                    active: true,
                    account_id: Some(1),
                }],
                checks: vec![github::AccountCheck {
                    account: "alice".into(),
                    access_errors: vec![],
                    reviews: Ok(github::AccountReviews {
                        account_id: 1,
                        pending: vec![],
                        unavailable_repos: Default::default(),
                        completed_repos: repos.into_iter().collect(),
                    }),
                }],
                access_cache: github::AccessCache::default(),
            }),
            enterprise: Ok(github::FetchedReviews::default()),
        }
    }
    let failed_scan = |app: &Engine, error: &str| Done::Scanned {
        roots: app.store.roots.clone(),
        result: Err(anyhow::anyhow!(error.to_string())),
    };

    for fetch_finishes_first in [true, false] {
        let cached = review(1).repository();
        let mut app = engine();
        app.store.pending.clear();
        app.store.last_successful_sync = None;
        app.store.roots = vec!["/missing/watched".into()];
        app.store.discovered = vec![LocalRepo {
            id: cached.clone(),
            paths: vec!["/missing/watched/clone".into()],
        }];
        app.repos = Some(app.store.discovered.clone());
        let mut calls = Vec::new();

        app.fetch_reviews();
        calls.extend(fetches(&app.take_effects()));
        // Hold discovery open while the first check completes.
        app.scanning = true;
        assert_eq!(calls.len(), 1);
        if fetch_finishes_first {
            let effects = app.handle(answer(&calls[0]));
            calls.extend(fetches(&effects));
            assert!(!app.fetching());
            assert!(app.store.last_successful_sync.is_none());
        }
        let scan = failed_scan(&app, "host discovery failed");
        calls.extend(fetches(&app.handle(scan)));
        if !fetch_finishes_first {
            assert_eq!(calls.len(), 1, "checks must not overlap");
            let effects = app.handle(answer(&calls[0]));
            calls.extend(fetches(&effects));
        }
        assert_eq!(
            calls,
            vec![BTreeSet::from([cached.clone()]); 2],
            "failed scan must request a replacement check for the cached repository"
        );
        let effects = app.handle(answer(&calls[1]));
        assert!(fetches(&effects).is_empty());
        assert!(app.store.last_successful_sync.is_some());
        assert!(!app.fetching());

        // A persistent discovery error must not block the next scheduled check.
        app.scanning = true;
        let scan = failed_scan(&app, "host discovery still failed");
        calls.extend(fetches(&app.handle(scan)));
        assert_eq!(calls, vec![BTreeSet::from([cached]); 3]);
        app.handle(answer(&calls[2]));
        assert!(!app.fetching());
        assert!(app.scan_error.is_some());
    }
}

#[test]
fn stale_cache_regressions_keep_tray_reviews_after_failed_startup_and_rescan() {
    let mut app = engine();
    // A restarted cache has not been verified in this process.
    app.health = SyncHealth::default();
    app.health.apply(
        github::Check {
            readiness: github::Readiness::Offline,
            reviews: Err(anyhow::anyhow!("network is unreachable")),
        },
        &mut app.store,
        100,
    );
    assert_eq!(app.review_status(), ReviewsStatus::Offline);
    assert_eq!(app.tray_reviews(), app.store.pending);
    // Beginning a rescan invalidates freshness, not cached visibility.
    app.health.verified = false;
    assert_eq!(app.tray_reviews().len(), 2);
    // A confirmed different account still clears the old cache.
    app.health.apply(
        github::Check {
            readiness: github::Readiness::Ready("another-viewer".into()),
            reviews: Err(anyhow::anyhow!("review query failed")),
        },
        &mut app.store,
        200,
    );
    assert!(app.tray_reviews().is_empty());
}

#[test]
fn stale_cache_regressions_snooze_expiry_during_rescan_produces_one_alert() {
    let mut app = engine();
    app.store
        .snooze(&review(1).key(), 5, Local::now().timestamp() - 301);
    app.health.verified = false;
    assert_eq!(app.wake(), vec![review(1)]);
    assert!(app.store.snoozed.is_empty());
    assert_eq!(app.tray_reviews().len(), 2);
    assert!(app.wake().is_empty());
    let batch = app.review_delivery.begin(&app.store).unwrap();
    assert_eq!(batch.reviews, vec![review(1)]);
    // A failed native send retains the reminder for recovery.
    assert!(
        app.review_delivery
            .complete(&mut app.store, &batch, false)
            .is_none()
    );
    assert_eq!(app.store.notification_queue, batch.notices);
}

#[test]
fn snooze_expiry_timer_wakes_and_delivers_the_reminder() {
    let mut app = engine();
    app.store
        .snooze(&review(1).key(), 5, Local::now().timestamp() - 301);
    let effects = app.handle(Event::Timer(Timer::Wake));
    assert!(app.store.snoozed.is_empty());
    assert_eq!(notify_slots(&effects), vec![0]);
}

#[test]
fn first_host_discovery_failure_preserves_legacy_reviews_and_snoozes() {
    let mut app = engine();
    app.repos = None;
    app.store.snooze(&review(1).key(), 5, 0);
    let pending = app.store.pending.clone();
    let snoozed = app.store.snoozed.clone();
    assert!(
        app.apply_discovery(Err(anyhow::anyhow!("host discovery failed")))
            .is_none()
    );
    assert_eq!(app.store.pending, pending);
    assert_eq!(app.store.snoozed, snoozed);
    assert!(app.repos.is_none());
}

#[test]
fn stale_cache_regressions_permission_wait_preserves_expired_snooze() {
    let mut app = engine();
    app.store
        .snooze(&review(1).key(), 5, Local::now().timestamp() - 301);
    app.notifications_ready = false;
    assert!(app.wake().is_empty());
    assert_eq!(app.store.snoozed.len(), 1);
    app.notifications_ready = true;
    assert_eq!(app.wake(), vec![review(1)]);
    assert!(app.wake().is_empty());
}

#[test]
fn review_notifications_label_receiving_accounts_for_single_and_grouped_requests() {
    let store = Store::from_json(include_str!(
        "../../tests/fixtures/multiple-account-state.json"
    ))
    .unwrap();
    let (_, single) = notification_text(&store.pending[..1]);
    assert!(single.contains("@alice · github.com/Owner/Repo#7"));
    let (summary, grouped) = notification_text(&store.pending);
    assert_eq!(summary, "2 pull requests need your review");
    assert!(grouped.contains("@alice · github.com/Owner/Repo#7"));
    assert!(grouped.contains("@bob · github.com/Owner/Repo#7"));
}

#[test]
fn update_notification_responses_show_the_window_or_open_the_release() {
    let url = "https://github.com/mattsverse/octowatch/releases";
    let mut app = engine();
    app.respond_to_update(Response::Clicked, url.into());
    assert!(matches!(app.take_effects()[..], [Effect::ShowWindow]));
    app.respond_to_update(Response::Action("download".into()), url.into());
    assert!(matches!(&app.take_effects()[..], [Effect::OpenUrl(opened)] if opened == url));
    app.respond_to_update(Response::Dismissed, url.into());
    assert!(app.take_effects().is_empty());
}

#[test]
fn manual_update_checks_answer_in_a_dialog_and_scheduled_ones_stay_quiet() {
    let mut app = engine();
    let effects = app.handle(Command::CheckForUpdates { manual: true });
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Start(Job::CheckUpdate)))
    );
    assert!(app.checking_update());
    // A second request while checking doesn't start another check.
    let effects = app.handle(Command::CheckForUpdates { manual: false });
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Start(Job::CheckUpdate)))
    );
    let effects = app.handle(Done::UpdateChecked(Ok(None)));
    assert!(!app.checking_update());
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Prompt(Prompt::UpToDate)))
    );

    app.handle(Command::CheckForUpdates { manual: false });
    let effects = app.handle(Done::UpdateChecked(Err(anyhow::anyhow!("offline"))));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Prompt(_)))
    );
}

/// Snapshots of each started save.
fn saves(effects: &[Effect]) -> Vec<&Store> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Start(Job::Save { store, .. }) => Some(&**store),
            _ => None,
        })
        .collect()
}

#[test]
fn saves_run_one_at_a_time_and_coalesce_changes_made_meanwhile() {
    let mut app = engine();
    app.persist = true;
    let effects = app.handle(Command::SetPollMinutes(5));
    assert_eq!(saves(&effects).len(), 1, "the first change saves at once");
    let effects = app.handle(Command::SetSnoozeMinutes(30));
    assert!(saves(&effects).is_empty());
    let effects = app.handle(Command::SetAppearance(Appearance::Dark));
    assert!(saves(&effects).is_empty());
    let effects = app.handle(Done::Saved(Ok(())));
    let follow_up = saves(&effects);
    assert_eq!(follow_up.len(), 1, "changes during a save coalesce");
    assert_eq!(follow_up[0].snooze_minutes, 30);
    assert_eq!(follow_up[0].appearance, Appearance::Dark);
    let effects = app.handle(Done::Saved(Ok(())));
    assert!(saves(&effects).is_empty(), "nothing left to save");
}

#[test]
fn save_errors_are_reported_until_a_later_save_succeeds() {
    let mut app = engine();
    app.persist = true;
    app.handle(Command::SetPollMinutes(5));
    app.handle(Done::Saved(Err(anyhow::anyhow!("disk full"))));
    assert!(
        app.errors(None)
            .contains(&"could not save state: disk full")
    );
    app.handle(Command::SetPollMinutes(10));
    app.handle(Done::Saved(Ok(())));
    assert!(app.errors(None).iter().all(|error| !error.contains("save")));
}

#[test]
fn an_older_snapshot_never_replaces_a_newer_one() {
    let gate = SaveGate::default();
    let mut writes = Vec::new();
    gate.write(2, || {
        writes.push(2);
        Ok(())
    })
    .unwrap();
    gate.write(1, || {
        writes.push(1);
        Ok(())
    })
    .unwrap();
    assert_eq!(writes, vec![2]);
    // A failed write doesn't count as written.
    assert!(gate.write(3, || Err(anyhow::anyhow!("disk full"))).is_err());
    assert_eq!(gate.written(), 2);
}

#[test]
fn flush_writes_unsaved_state_and_skips_when_up_to_date() {
    let mut app = engine();
    app.persist = true;
    // Already on disk: nothing to write.
    app.flush();
    assert!(app.errors(None).iter().all(|error| !error.contains("save")));
    // A pending change is written now. The fixture blocks real writes, so
    // the attempt shows up as an error instead of touching the user's state.
    app.handle(Command::SetPollMinutes(5));
    app.flush();
    assert!(
        app.errors(None)
            .iter()
            .any(|error| error.contains("could not save state"))
    );
}
