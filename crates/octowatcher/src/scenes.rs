//! Named states over made-up reviews, for `--snap out.png --scene NAME [--theme light]`.

use chrono::Local;
use octowatcher_core::{
    demo,
    engine::{Prompt, Update},
    github::Readiness,
    review_filter::DraftFilter,
};
use rune::TextEdit;

use crate::app::{Octowatcher, Popup, Tab};

pub fn demo() -> Octowatcher {
    let mut s = Octowatcher::new(demo::engine(Local::now().timestamp()));
    s.jobs.record_only = true;
    s
}

pub const ALL: &[crate::Scene] = &[
    ("reviews", |_| {}),
    ("filters", |s| {
        s.filters_open = true;
        s.filters.draft = DraftFilter::Ready;
    }),
    ("search", |s| {
        s.search = TextEdit::new("checkout");
        s.search_edited();
    }),
    ("snooze-menu", |s| {
        let key = s.engine.store().pending[0].key();
        s.popup = Some(Popup::Snooze(key));
    }),
    ("no-results", |s| {
        s.search = TextEdit::new("kubernetes");
        s.search_edited();
    }),
    ("empty", |s| {
        s.engine.store_mut().pending.clear();
        s.engine.store_mut().snoozed.clear();
    }),
    ("update", |s| {
        s.engine
            .set_update(Some(Update::Ready(semver::Version::new(0, 7, 0))));
    }),
    ("dialog", |s| {
        s.prompt = Some(Prompt::Restart(semver::Version::new(0, 7, 0)));
    }),
    ("repositories", |s| s.tab = Tab::Repositories),
    ("settings", |s| s.tab = Tab::Settings),
    ("signed-out", |s| {
        s.tab = Tab::Settings;
        let health = s.engine.health_mut();
        health.readiness = Readiness::SignedOut;
        health
            .accounts
            .values_mut()
            .for_each(|ready| *ready = Readiness::SignedOut);
        s.engine.store_mut().available_accounts.clear();
        s.engine.store_mut().stale_accounts = ["sam".to_string(), "sam-work".to_string()].into();
    }),
];
