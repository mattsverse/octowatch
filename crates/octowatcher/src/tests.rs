//! The real view over made-up reviews, driven headlessly. No tray, gh, timers, notifications
//! or disk writes: jobs are recorded instead of run.

use chrono::Local;
use octowatcher_core::{demo, engine::JobKey, review_filter::DraftFilter};
use rune::{Env, Key, KeyEvent, Mods, Runtime};

use crate::app::{Octowatcher, Popup, Tab};
use crate::ui;

fn state() -> Octowatcher {
    let mut s = Octowatcher::new(demo::engine(Local::now().timestamp()));
    s.jobs.record_only = true;
    s
}

fn runtime(s: Octowatcher) -> Runtime<Octowatcher> {
    // Tall enough that every section of a tab is on screen without scrolling.
    let mut rt = Runtime::new(s, ui::view, (560.0, 1600.0), Env::new(true, 2.0));
    rt.on_key = Some(ui::on_key);
    rt.frame(0.0);
    rt
}

fn settle(rt: &mut Runtime<Octowatcher>) {
    for _ in 0..30 {
        rt.frame(1.0 / 60.0);
    }
}

/// The centre of the element keyed `key`, or `row > key` for one inside the element keyed `row`.
fn center(rt: &Runtime<Octowatcher>, key: &str) -> (f32, f32) {
    let (row, leaf) = key
        .rsplit_once(" > ")
        .map_or((None, key), |(row, leaf)| (Some(row), leaf));
    let within = row.map(|row| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(row))
            .unwrap_or_else(|| panic!("no element {row}"))
            .path
    });
    let e = rt
        .elements()
        .into_iter()
        .find(|e| {
            e.key.as_deref() == Some(leaf)
                && within
                    .as_ref()
                    .is_none_or(|p| e.path.starts_with(p.as_str()))
        })
        .unwrap_or_else(|| panic!("no element {key}"));
    let [x, y, w, h] = e.rect;
    (x + w / 2.0, y + h / 2.0)
}

fn click(rt: &mut Runtime<Octowatcher>, key: &str) {
    let (x, y) = center(rt, key);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    settle(rt);
}

fn press(rt: &mut Runtime<Octowatcher>, key: Key, cmd: bool) {
    rt.key(KeyEvent {
        key,
        mods: Mods {
            cmd,
            ..Mods::default()
        },
    });
    settle(rt);
}

fn type_text(rt: &mut Runtime<Octowatcher>, text: &str) {
    for c in text.chars() {
        rt.key(KeyEvent {
            key: Key::Text(c.to_string()),
            mods: Mods::default(),
        });
    }
    settle(rt);
}

fn review_rows(rt: &Runtime<Octowatcher>) -> usize {
    rt.elements()
        .iter()
        .filter(|e| e.key.as_deref().is_some_and(|k| k.starts_with("review:")))
        .count()
}

#[test]
fn every_visible_review_has_a_row_and_a_row_opens_its_pull_request() {
    let mut rt = runtime(state());
    assert_eq!(review_rows(&rt), 6);
    click(&mut rt, "review:github.com/northwind/billing#482:1");
    assert_eq!(
        rt.take_url_requests(),
        ["https://github.com/northwind/billing/pull/482"]
    );
}

#[test]
fn snoozing_from_the_menu_snoozes_without_opening_the_pull_request() {
    let mut rt = runtime(state());
    click(
        &mut rt,
        "review:github.com/northwind/billing#482:1 > snooze",
    );
    assert!(matches!(rt.state.popup, Some(Popup::Snooze(_))));
    assert!(
        rt.take_url_requests().is_empty(),
        "the snooze button doesn't open the PR"
    );
    let key = rt.state.engine.store().pending[0].key();
    // Thirty minutes is the demo's default, marked in the menu.
    let item = rt
        .elements()
        .into_iter()
        .find(|e| e.text.as_deref() == Some("1 hour"))
        .expect("the menu lists durations");
    let [x, y, w, h] = item.rect;
    rt.pointer_down(x + w / 2.0, y + h / 2.0);
    rt.pointer_up(x + w / 2.0, y + h / 2.0);
    settle(&mut rt);
    assert_eq!(rt.state.popup, None);
    let pr = rt
        .state
        .engine
        .store()
        .pending
        .iter()
        .find(|pr| pr.key() == key)
        .unwrap()
        .clone();
    assert!(rt.state.engine.store().snooze_for(&pr).is_some());
    assert!(rt.take_url_requests().is_empty());
}

#[test]
fn search_narrows_the_list_and_escape_resets_it() {
    let mut rt = runtime(state());
    click(&mut rt, "review-search");
    type_text(&mut rt, "checkout");
    assert_eq!(rt.state.filters.query, "checkout");
    assert_eq!(review_rows(&rt), 1);
    rt.update(|s, tx| {
        s.search.focused = false;
        tx.blur();
    });
    settle(&mut rt);
    press(&mut rt, Key::Escape, false);
    assert_eq!(rt.state.filters.query, "");
    assert_eq!(review_rows(&rt), 6);
}

#[test]
fn a_search_with_no_matches_offers_a_reset() {
    let mut s = state();
    s.search = rune::TextEdit::new("kubernetes");
    s.search_edited();
    let mut rt = runtime(s);
    assert_eq!(review_rows(&rt), 0);
    click(&mut rt, "no-results-reset");
    assert_eq!(rt.state.filters.active_count(), 0);
    assert_eq!(review_rows(&rt), 6);
}

#[test]
fn filters_open_in_a_panel_and_keep_their_choice_while_closed() {
    let mut rt = runtime(state());
    assert!(rt.find("review-filter-controls").is_none());
    click(&mut rt, "toggle-review-filters");
    assert!(rt.find("review-filter-controls").is_some());
    rt.update(|s, _| {
        s.filters.draft = DraftFilter::Draft;
        s.filters_changed();
    });
    settle(&mut rt);
    assert_eq!(review_rows(&rt), 1);
    click(&mut rt, "toggle-review-filters");
    assert!(rt.find("review-filter-controls").is_none());
    assert_eq!(review_rows(&rt), 1, "closing the panel keeps the filters");
}

#[test]
fn shortcuts_switch_tabs_focus_search_and_refresh() {
    let mut rt = runtime(state());
    press(&mut rt, Key::Command('2'), true);
    assert_eq!(rt.state.tab, Tab::Repositories);
    press(&mut rt, Key::Command(','), true);
    assert_eq!(rt.state.tab, Tab::Settings);
    press(&mut rt, Key::Command('f'), true);
    assert_eq!(rt.state.tab, Tab::Reviews);
    assert!(rt.state.filters_open);
    assert!(rt.state.search.focused);
    rt.update(|s, tx| {
        s.jobs.started.clear();
        s.search.focused = false;
        tx.blur();
    });
    settle(&mut rt);
    press(&mut rt, Key::Command('r'), true);
    // A forced refresh rescans first, then fetches.
    assert!(
        rt.state.jobs.started.contains(&JobKey::Scan),
        "{:?}",
        rt.state.jobs.started
    );
}

#[test]
fn a_click_outside_an_open_menu_closes_it_and_the_next_switches_tabs() {
    let mut s = state();
    s.popup = Some(Popup::Snooze(s.engine.store().pending[0].key()));
    let mut rt = runtime(s);
    // As with a native menu, the first click outside only dismisses it.
    click(&mut rt, "tab-repositories");
    assert_eq!(rt.state.popup, None);
    assert_eq!(rt.state.tab, Tab::Reviews);
    click(&mut rt, "tab-repositories");
    assert_eq!(rt.state.tab, Tab::Repositories);
    assert!(rt.find("repos").is_some());
}

#[test]
fn a_repository_switch_stops_watching_it() {
    let mut s = state();
    s.tab = Tab::Repositories;
    let mut rt = runtime(s);
    assert!(rt.state.engine.store().is_enabled("northwind/billing"));
    click(&mut rt, "repo-1 > watch");
    assert!(!rt.state.engine.store().is_enabled("northwind/billing"));
}

#[test]
fn settings_switches_change_notification_preferences() {
    let mut s = state();
    s.tab = Tab::Settings;
    let mut rt = runtime(s);
    let drafts = rt.state.engine.store().notify_drafts;
    click(&mut rt, "notify-drafts");
    assert_ne!(rt.state.engine.store().notify_drafts, drafts);
    let muted = rt.state.engine.store().notifications_muted;
    click(&mut rt, "mute-notifications");
    assert_ne!(rt.state.engine.store().notifications_muted, muted);
    // Draft alerts depend on review alerts: muted, their switch does nothing.
    let drafts = rt.state.engine.store().notify_drafts;
    click(&mut rt, "notify-drafts");
    assert_eq!(rt.state.engine.store().notify_drafts, drafts);
}

#[test]
fn a_snooze_popup_closes_when_its_review_leaves_the_queue() {
    let mut s = state();
    let key = s.engine.store().pending[0].key();
    s.popup = Some(Popup::Snooze(key.clone()));
    let mut rt = runtime(s);
    rt.update(|s, _| s.engine.store_mut().pending.retain(|pr| pr.key() != key));
    // Any engine event re-checks the popup against the queue.
    click(&mut rt, "refresh");
    assert_eq!(rt.state.popup, None);
}

#[test]
fn the_restart_prompt_answers_later_without_restarting() {
    let mut s = state();
    s.prompt = Some(octowatcher_core::engine::Prompt::Restart(
        semver::Version::new(9, 0, 0),
    ));
    let mut rt = runtime(s);
    assert!(rt.find("prompt-accept").is_some());
    click(&mut rt, "prompt-cancel");
    assert!(rt.state.prompt.is_none());
}
