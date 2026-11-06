//! A settled session over made-up reviews, for screenshots and headless
//! scenes. Nothing here touches gh, the disk or the notification service.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::discovery::LocalRepo;
use crate::engine::Engine;
use crate::github::Readiness;
use crate::health::SyncHealth;
use crate::repository::{PUBLIC_HOST, RepositoryId};
use crate::store::{PendingReview, Snooze, Store};

const REPOS: [&str; 5] = [
    "northwind/billing",
    "northwind/web",
    "orbit-labs/telemetry",
    "orbit-labs/cli",
    "mira/design-tokens",
];

/// `now` is Unix seconds; snoozes and the last sync are relative to it.
pub fn engine(now: i64) -> Engine {
    let review = |account: &str, id: u64, repo: &str, number: u64, title: &str, author: &str| {
        PendingReview {
            host: PUBLIC_HOST.into(),
            account: account.into(),
            account_id: id,
            repo: repo.into(),
            number,
            title: title.into(),
            url: format!("https://github.com/{repo}/pull/{number}"),
            author: author.into(),
            is_draft: false,
            rereview: false,
            requested_at: None,
        }
    };
    let mut pending = vec![
        review(
            "sam",
            1,
            "northwind/billing",
            482,
            "Retry failed invoice webhooks with exponential backoff",
            "lena-k",
        ),
        PendingReview {
            rereview: true,
            ..review(
                "sam",
                1,
                "northwind/web",
                1290,
                "Move checkout to the new address autocomplete",
                "dmitri",
            )
        },
        review(
            "sam-work",
            2,
            "orbit-labs/telemetry",
            77,
            "Batch span exports and drop the legacy collector",
            "priya",
        ),
        PendingReview {
            is_draft: true,
            ..review(
                "sam",
                1,
                "orbit-labs/cli",
                341,
                "Add `orbit login --device` for headless machines",
                "jonas",
            )
        },
        review(
            "sam-work",
            2,
            "mira/design-tokens",
            58,
            "Ship dark-mode elevation tokens",
            "ama",
        ),
        review(
            "sam",
            1,
            "northwind/web",
            1284,
            "Fix focus trap in the account settings dialog",
            "grace-h",
        ),
    ];
    // Requests from 25 minutes to two days old.
    const AGES_MINUTES: [i64; 6] = [25, 140, 290, 1_500, 2_900, 3_100];
    pending
        .iter_mut()
        .zip(AGES_MINUTES)
        .for_each(|(pr, minutes)| {
            pr.requested_at =
                chrono::DateTime::from_timestamp(now - minutes * 60, 0).map(|at| at.to_rfc3339());
        });
    let last = pending.last().expect("demo reviews");
    let snoozed = vec![Snooze {
        host: PUBLIC_HOST.into(),
        account: last.account.clone(),
        account_id: last.account_id,
        repo: last.repo.clone(),
        number: last.number,
        until: now + 45 * 60,
        requested_at: last.requested_at.clone(),
    }];
    let home = dirs::home_dir().unwrap_or_default();
    let mut engine = Engine::settled(Store {
        roots: vec![home.join("Dev"), home.join("Work")],
        pending,
        snoozed,
        poll_minutes: 5,
        snooze_minutes: 30,
        sync_account: Some("sam".into()),
        last_successful_sync: Some(now - 2 * 60),
        recovery_blocked: Some("demo state is never written".into()),
        known_accounts: BTreeSet::from(["sam".into(), "sam-work".into()]),
        account_ids: BTreeMap::from([("sam".into(), 1), ("sam-work".into(), 2)]),
        available_accounts: BTreeSet::from(["sam".into(), "sam-work".into()]),
        local_repos: REPOS.iter().map(|repo| repo.to_string()).collect(),
        disabled: BTreeSet::from(["northwind/legacy-admin".into()]),
        repo_accounts: BTreeMap::from([(
            "mira/design-tokens".into(),
            BTreeSet::from(["sam-work".to_string()]),
        )]),
        ..Default::default()
    });
    let mut repos: Vec<_> = REPOS
        .iter()
        .chain(["northwind/legacy-admin"].iter())
        .map(|slug| LocalRepo {
            id: RepositoryId::new(PUBLIC_HOST, slug),
            paths: vec![demo_path(&home, slug)],
        })
        .collect();
    repos.sort_by(|a, b| a.id.cmp(&b.id));
    engine.set_repos(Some(repos));
    engine.set_login_state(crate::login::State::On);
    *engine.health_mut() = SyncHealth {
        readiness: Readiness::Ready("sam".into()),
        accounts: BTreeMap::from([
            ("sam".into(), Readiness::Ready("sam".into())),
            ("sam-work".into(), Readiness::Ready("sam-work".into())),
        ]),
        active_public: Some("sam".into()),
        verified: true,
        ..SyncHealth::default()
    };
    engine
}

fn demo_path(home: &std::path::Path, slug: &str) -> PathBuf {
    let (owner, name) = slug.split_once('/').unwrap_or(("", slug));
    let root = if owner == "orbit-labs" { "Work" } else { "Dev" };
    home.join(root).join(name)
}
