//! Exercise the real render tree and click handlers without native tray startup,
//! network fetches, notifications, or writes to the user's persisted state.
use super::*;
use gpui::{Modifiers, TestAppContext, VisualTestContext};

fn review(number: u64) -> PendingReview {
    PendingReview {
        host: repository::default_host(),
        repo: "owner/repo".into(),
        number,
        title: format!("Review number {number}"),
        url: format!("https://github.com/owner/repo/pull/{number}"),
        author: "author".into(),
        is_draft: false,
        rereview: false,
        requested_at: None,
    }
}

fn fixture(window: &mut Window, cx: &mut Context<Octowatcher>) -> Octowatcher {
    bind_keys(cx);
    let keyboard = Keyboard::new(cx);
    window.focus(&keyboard.root);
    Octowatcher {
        keyboard,
        persist: false,
        store: Store {
            pending: vec![review(1), review(2)],
            roots: vec!["/test/root".into()],
            ..Store::default()
        },
        repos: Some(vec![LocalRepo {
            id: RepositoryId::new("github.com", "owner/repo"),
            paths: vec!["/test/repo".into()],
        }]),
        tab: Tab::Reviews,
        snooze_picker: None,
        last_checked: None,
        fetch_error: None,
        scan_error: None,
        tray_error: None,
        save_error: None,
        notification_error: None,
        announced_hosts: [repository::default_host()].into_iter().collect(),
        launch_summary: false,
        review_delivery: Delivery::default(),
        notification_tasks: HashMap::new(),
        tray: None,
        update: None,
        scan_task: None,
        // Occupied to prevent the enable-repository test from starting a fetch.
        fetch_task: Some(Task::ready(())),
        poll_task: None,
        wake_task: None,
        update_check: None,
        _startup_and_updates: Task::ready(()),
        appearance_subscription: None,
    }
}

fn focused(view: &Entity<Octowatcher>, cx: &mut VisualTestContext) -> Option<Control> {
    cx.update(|window, cx| view.read(cx).keyboard.focused(window))
}

fn focus(view: &Entity<Octowatcher>, key: Control, cx: &mut VisualTestContext) {
    cx.update(|window, cx| view.read(cx).keyboard.focus(&key, window));
    cx.run_until_parked();
}

// GPUI's simulate_keystrokes sends only key-down events; controls activate on
// key-up. Send the same full press/release sequence as a physical keyboard.
fn press(cx: &mut VisualTestContext, keys: &str) {
    for key in keys.split_whitespace() {
        let stroke = gpui::Keystroke::parse(key).unwrap();
        cx.simulate_event(KeyDownEvent {
            keystroke: stroke.clone(),
            is_held: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke: stroke });
    }
}

fn key(number: u64) -> (RepositoryId, u64) {
    (RepositoryId::new("github.com", "owner/repo"), number)
}

#[gpui::test]
fn tab_traversal_and_tab_arrows(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    for expected in [
        Control::Refresh,
        Control::Tab(Tab::Reviews),
        Control::Tab(Tab::Repositories),
        Control::Tab(Tab::Settings),
        Control::Review(key(1)),
        Control::Snooze(key(1)),
        Control::Review(key(2)),
        Control::Snooze(key(2)),
        Control::Refresh,
    ] {
        press(cx, "tab");
        assert_eq!(focused(&view, cx), Some(expected));
    }
    press(cx, "shift-tab");
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(2))));
    focus(&view, Control::Tab(Tab::Reviews), cx);
    press(cx, "right");
    assert_eq!(focused(&view, cx), Some(Control::Tab(Tab::Repositories)));
    cx.update(|_, cx| assert_eq!(view.read(cx).tab, Tab::Repositories));
    press(cx, "right");
    cx.update(|_, cx| assert_eq!(view.read(cx).tab, Tab::Settings));
    press(cx, "right left");
    cx.update(|_, cx| assert_eq!(view.read(cx).tab, Tab::Settings));
    focus(&view, Control::Tab(Tab::Reviews), cx);
    press(cx, "space");
    cx.update(|_, cx| assert_eq!(view.read(cx).tab, Tab::Reviews));
}

#[gpui::test]
fn matching_reviews_and_repositories_on_different_hosts_keep_separate_focus(
    cx: &mut TestAppContext,
) {
    let (view, cx) = cx.add_window_view(fixture);
    let public = review(1);
    let enterprise = PendingReview {
        host: "github.example.com".into(),
        url: "https://github.example.com/owner/repo/pull/1".into(),
        ..public.clone()
    };
    let enterprise_key = enterprise.key();
    view.update(cx, |view, cx| {
        view.store.pending = vec![public.clone(), enterprise.clone()];
        view.repos = Some(vec![
            LocalRepo {
                id: public.repository(),
                paths: vec!["/test/public".into()],
            },
            LocalRepo {
                id: enterprise.repository(),
                paths: vec!["/test/enterprise".into()],
            },
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    focus(&view, Control::Review(public.key()), cx);
    press(cx, "down");
    assert_eq!(
        focused(&view, cx),
        Some(Control::Review(enterprise_key.clone()))
    );
    view.update(cx, |view, cx| {
        view.store.pending.reverse();
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        focused(&view, cx),
        Some(Control::Review(enterprise_key.clone()))
    );
    press(cx, "tab enter tab space");
    assert_eq!(
        focused(&view, cx),
        Some(Control::Snooze(enterprise_key.clone()))
    );
    cx.update(|_, cx| {
        let store = &view.read(cx).store;
        assert!(store.snooze_for(&enterprise).is_some());
        assert!(store.snooze_for(&public).is_none());
    });
    assert_eq!(cx.opened_url(), None);
    press(cx, "space");
    focus(&view, Control::Snooze(public.key()), cx);
    press(cx, "enter tab");
    view.update(cx, |view, cx| view.snooze(enterprise_key.clone(), cx));
    cx.run_until_parked();
    assert_eq!(
        focused(&view, cx),
        Some(Control::SnoozeDuration(public.key(), 5))
    );
    cx.update(|_, cx| assert_eq!(view.read(cx).snooze_picker, Some(public.key())));
    press(cx, "escape");
    focus(&view, Control::Review(public.key()), cx);
    press(cx, "enter");
    assert_eq!(cx.opened_url(), Some(public.url.clone()));
    focus(&view, Control::Review(enterprise_key.clone()), cx);
    press(cx, "space");
    assert_eq!(cx.opened_url(), Some(enterprise.url.clone()));

    focus(&view, Control::Tab(Tab::Repositories), cx);
    press(cx, "enter");
    let enterprise_repository = Control::Repository(enterprise.repository());
    focus(&view, enterprise_repository.clone(), cx);
    view.update(cx, |view, cx| {
        view.repos.as_mut().unwrap().reverse();
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(focused(&view, cx), Some(enterprise_repository));
    press(cx, "space");
    cx.update(|_, cx| {
        let store = &view.read(cx).store;
        assert!(!store.is_enabled(&enterprise.repository()));
        assert!(store.is_enabled(&public.repository()));
        assert_eq!(store.pending, vec![public.clone()]);
    });
    press(cx, "enter");
    focus(&view, Control::Repository(public.repository()), cx);
    press(cx, "space");
    cx.update(|_, cx| {
        let store = &view.read(cx).store;
        assert!(!store.is_enabled(&public.repository()));
        assert!(store.is_enabled(&enterprise.repository()));
    });
}

#[gpui::test]
fn snooze_activation_never_opens_parent_card(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    focus(&view, Control::Snooze(key(1)), cx);
    press(cx, "enter tab enter");
    cx.update(|_, cx| assert!(view.read(cx).store.snooze_for(&review(1)).is_some()));
    assert_eq!(cx.opened_url(), None);
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    press(cx, "ctrl-space");
    cx.update(|_, cx| assert!(view.read(cx).store.snooze_for(&review(1)).is_some()));
    press(cx, "space");
    cx.update(|_, cx| assert!(view.read(cx).store.snoozed.is_empty()));
    assert_eq!(cx.opened_url(), None);
    // The nested mouse target retains focus and propagation behavior too.
    let bounds = cx.update(|_, cx| {
        view.read(cx)
            .keyboard
            .bounds(&Control::Snooze(key(1)))
            .unwrap()
    });
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::none());
    cx.simulate_click(bounds.center(), Modifiers::none());
    assert_eq!(cx.opened_url(), None);
    let duration = cx.update(|_, cx| {
        view.read(cx)
            .keyboard
            .bounds(&Control::SnoozeDuration(key(1), 5))
            .unwrap()
    });
    cx.simulate_mouse_move(duration.center(), None, Modifiers::none());
    cx.simulate_click(duration.center(), Modifiers::none());
    cx.update(|_, cx| assert!(view.read(cx).store.snooze_for(&review(1)).is_some()));
    assert_eq!(cx.opened_url(), None);
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    focus(&view, Control::Review(key(1)), cx);
    press(cx, "enter");
    assert_eq!(cx.opened_url(), Some(review(1).url));
    focus(&view, Control::Review(key(2)), cx);
    press(cx, "space");
    assert_eq!(cx.opened_url(), Some(review(2).url));
}

#[gpui::test]
fn repository_and_settings_activation(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    focus(&view, Control::Tab(Tab::Repositories), cx);
    press(cx, "enter tab");
    assert_eq!(focused(&view, cx), Some(Control::Tab(Tab::Settings)));
    press(cx, "tab");
    assert_eq!(
        focused(&view, cx),
        Some(Control::RemoveRoot("/test/root".into()))
    );
    press(cx, "tab");
    assert_eq!(focused(&view, cx), Some(Control::AddRoot));
    press(cx, "tab tab");
    assert_eq!(focused(&view, cx), Some(Control::Repository(key(1).0)));
    press(cx, "space");
    cx.update(|_, cx| assert!(!view.read(cx).store.is_enabled(&key(1).0)));
    press(cx, "enter");
    cx.update(|_, cx| assert!(view.read(cx).store.is_enabled(&key(1).0)));
    focus(&view, Control::Tab(Tab::Settings), cx);
    press(cx, "space tab tab tab tab");
    cx.simulate_resize(size(px(560.), px(300.)));
    cx.run_until_parked();
    assert_eq!(focused(&view, cx), Some(Control::Poll(1)));
    press(cx, "enter");
    cx.update(|_, cx| assert_eq!(view.read(cx).store.poll_minutes, 1));
    for minutes in POLL_CHOICES.into_iter().skip(1) {
        press(cx, "tab");
        assert_eq!(focused(&view, cx), Some(Control::Poll(minutes)));
    }
    for minutes in SNOOZE_CHOICES {
        press(cx, "tab");
        assert_eq!(focused(&view, cx), Some(Control::SnoozeMinutes(minutes)));
    }
    press(cx, "space");
    cx.update(|_, cx| assert_eq!(view.read(cx).store.snooze_minutes, 120));
    press(cx, "tab");
    assert_eq!(focused(&view, cx), Some(Control::MuteNotifications));
    press(cx, "space");
    cx.update(|_, cx| assert!(view.read(cx).store.notifications_muted));
    press(cx, "enter");
    cx.update(|_, cx| assert!(!view.read(cx).store.notifications_muted));
    press(cx, "tab space");
    assert_eq!(focused(&view, cx), Some(Control::NotifyDrafts));
    cx.update(|_, cx| assert!(!view.read(cx).store.notify_drafts));
    press(cx, "enter");
    cx.update(|_, cx| assert!(view.read(cx).store.notify_drafts));
    press(cx, "tab");
    assert_eq!(focused(&view, cx), Some(Control::TestNotification));
    cx.update(|_, cx| {
        let keyboard = &view.read(cx).keyboard;
        let bounds = keyboard.bounds(&Control::TestNotification).unwrap();
        assert!(keyboard.scroll.offset().y < px(0.));
        assert!(bounds.top() >= keyboard.scroll.bounds().top());
        assert!(bounds.bottom() <= keyboard.scroll.bounds().bottom());
    });
}

#[gpui::test]
fn appearance_choices_preserve_focus_and_review_activation(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    focus(&view, Control::Tab(Tab::Settings), cx);
    press(cx, "enter tab");
    assert_eq!(
        focused(&view, cx),
        Some(Control::Appearance(Appearance::System))
    );
    for (appearance, activation) in [(Appearance::Light, "space"), (Appearance::Dark, "enter")] {
        press(cx, "tab");
        assert_eq!(focused(&view, cx), Some(Control::Appearance(appearance)));
        press(cx, activation);
        cx.update(|_, cx| assert_eq!(view.read(cx).store.appearance, appearance));
        assert_eq!(focused(&view, cx), Some(Control::Appearance(appearance)));

        // The same nested controls still work after each palette change.
        focus(&view, Control::Tab(Tab::Reviews), cx);
        press(cx, "enter");
        focus(&view, Control::Snooze(key(1)), cx);
        press(cx, "enter tab space");
        assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
        cx.update(|_, cx| assert!(view.read(cx).store.snooze_for(&review(1)).is_some()));
        assert_eq!(cx.opened_url(), None);
        press(cx, "enter");
        cx.update(|_, cx| assert!(view.read(cx).store.snoozed.is_empty()));
        focus(&view, Control::Tab(Tab::Settings), cx);
        press(cx, "enter");
        focus(&view, Control::Appearance(appearance), cx);
    }
    press(cx, "shift-tab shift-tab space");
    assert_eq!(
        focused(&view, cx),
        Some(Control::Appearance(Appearance::System))
    );
    cx.update(|_, cx| assert_eq!(view.read(cx).store.appearance, Appearance::System));
}

#[gpui::test]
fn review_navigation_scrolls_and_preserves_snooze_column(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    cx.simulate_resize(size(px(560.), px(680.)));
    view.update(cx, |view, cx| {
        view.store.pending = (1..=40).map(review).collect();
        cx.notify();
    });
    cx.run_until_parked();
    focus(&view, Control::Review(key(1)), cx);
    press(cx, "up down");
    assert_eq!(focused(&view, cx), Some(Control::Review(key(2))));
    press(cx, "end");
    assert_eq!(focused(&view, cx), Some(Control::Review(key(40))));
    cx.update(|_, cx| {
        let keyboard = &view.read(cx).keyboard;
        let bounds = keyboard.bounds(&Control::Review(key(40))).unwrap();
        assert!(keyboard.scroll.offset().y < px(0.));
        assert!(bounds.top() >= keyboard.scroll.bounds().top());
        assert!(bounds.bottom() <= keyboard.scroll.bounds().bottom());
    });
    press(cx, "home tab down end down");
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(40))));
    press(cx, "home up");
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    // Rerendering after a user scroll must not snap back to the focused row.
    cx.update(|_, cx| {
        view.read(cx)
            .keyboard
            .scroll
            .set_offset(gpui::point(px(0.), px(-200.)))
    });
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(view.read(cx).keyboard.scroll.offset().y, px(-200.)));
}

#[gpui::test]
fn focus_survives_reordering_and_recovers_from_removed_reviews(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    focus(&view, Control::Review(key(2)), cx);
    view.update(cx, |view, cx| {
        view.store.pending.reverse();
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(focused(&view, cx), Some(Control::Review(key(2))));
    view.update(cx, |view, cx| {
        view.store.pending.retain(|pr| pr.number != 2);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(focused(&view, cx), Some(Control::Review(key(1))));
    view.update(cx, |view, cx| {
        view.store.pending.clear();
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(focused(&view, cx), Some(Control::Tab(Tab::Settings)));
    // Header navigation works with no reviews while a check/scan is pending.
    for expected in [
        Control::Refresh,
        Control::Tab(Tab::Reviews),
        Control::Tab(Tab::Repositories),
        Control::Tab(Tab::Settings),
    ] {
        press(cx, "tab");
        assert_eq!(focused(&view, cx), Some(expected));
    }
    view.update(cx, |view, cx| {
        view.repos = None;
        view.scan_task = Some(Task::ready(()));
        cx.notify();
    });
    cx.run_until_parked();
    press(cx, "left tab tab tab tab");
    assert_eq!(focused(&view, cx), Some(Control::Rescan));
}

#[gpui::test]
fn folder_focus_and_window_reopening(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    focus(&view, Control::Tab(Tab::Repositories), cx);
    press(cx, "enter");
    focus(&view, Control::RemoveRoot("/test/root".into()), cx);
    // Simulate removal by another source; recovery must select Add folder.
    view.update(cx, |view, cx| {
        view.store.roots.clear();
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(focused(&view, cx), Some(Control::AddRoot));
    cx.set_global(MainView(view.clone()));
    cx.update(|window, _| window.remove_window());
    cx.cx.update(show_window);
    cx.run_until_parked();
    let window = cx.cx.read(|cx| *cx.windows().first().unwrap());
    let mut reopened = VisualTestContext::from_window(window, &cx.cx);
    press(&mut reopened, "tab");
    assert_eq!(focused(&view, &mut reopened), Some(Control::Refresh));
    press(&mut reopened, "tab tab tab tab");
    assert_eq!(focused(&view, &mut reopened), Some(Control::AddRoot));
}

#[gpui::test]
fn platform_shortcuts_dispatch_without_activating_a_card(cx: &mut TestAppContext) {
    use std::{cell::Cell, rc::Rc};
    let (view, cx) = cx.add_window_view(fixture);
    let refreshes = Rc::new(Cell::new(0));
    let quits = Rc::new(Cell::new(0));
    cx.update(|_, cx| {
        cx.on_action({
            let refreshes = refreshes.clone();
            move |_: &Refresh, _| refreshes.set(refreshes.get() + 1)
        });
        cx.on_action({
            let quits = quits.clone();
            move |_: &Quit, _| quits.set(quits.get() + 1)
        });
    });
    focus(&view, Control::Review(key(1)), cx);
    #[cfg(target_os = "macos")]
    press(cx, "cmd-r cmd-q");
    #[cfg(not(target_os = "macos"))]
    press(cx, "ctrl-r ctrl-q");
    assert_eq!(refreshes.get(), 1);
    assert_eq!(quits.get(), 1);
    assert_eq!(cx.opened_url(), None);
}

#[gpui::test]
fn bulk_removed_reviews_keep_activation_kind(cx: &mut TestAppContext) {
    for snooze in [true, false] {
        let (view, visual) = cx.add_window_view(fixture);
        view.update(visual, |view, cx| {
            view.store.pending = (1..=4).map(review).collect();
            cx.notify();
        });
        visual.run_until_parked();
        let original = if snooze {
            Control::Snooze(key(4))
        } else {
            Control::Review(key(4))
        };
        focus(&view, original, visual);
        // Removing rows before and at the focused row used to clamp a card's
        // old absolute index to the surviving row's Snooze button.
        view.update(visual, |view, cx| {
            view.store.pending.retain(|pr| pr.number == 3);
            cx.notify();
        });
        visual.run_until_parked();
        let expected = if snooze {
            Control::Snooze(key(3))
        } else {
            Control::Review(key(3))
        };
        assert_eq!(focused(&view, visual), Some(expected));
        press(visual, "enter");
        if snooze {
            press(visual, "tab enter");
            visual.update(|_, cx| assert!(view.read(cx).store.snooze_for(&review(3)).is_some()));
            assert_eq!(visual.opened_url(), None);
        } else {
            visual.update(|_, cx| assert!(view.read(cx).store.snoozed.is_empty()));
            assert_eq!(visual.opened_url(), Some(review(3).url));
        }
        visual.update(|window, _| window.remove_window());
    }
}

#[gpui::test]
fn notification_snooze_preserves_unrelated_picker_and_focus(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    focus(&view, Control::Snooze(key(1)), cx);
    press(cx, "enter tab tab");
    let duration = Control::SnoozeDuration(key(1), 10);
    assert_eq!(focused(&view, cx), Some(duration.clone()));

    // Exercise the same application handler as a native notification action.
    for number in [2, 99] {
        view.update(cx, |view, cx| view.snooze(key(number), cx));
        cx.run_until_parked();
        assert_eq!(focused(&view, cx), Some(duration.clone()));
        cx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.snooze_picker, Some(key(1)));
            assert!(view.store.snooze_for(&review(1)).is_none());
            assert!(view.store.snooze_for(&review(2)).is_some());
            assert_eq!(view.store.snoozed.len(), 1);
        });
    }
    let before = Local::now().timestamp();
    press(cx, "space");
    let after = Local::now().timestamp();
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    cx.update(|_, cx| {
        let view = view.read(cx);
        assert!(view.snooze_picker.is_none());
        let until = view.store.snooze_for(&review(1)).unwrap().until;
        assert!((before + 10 * 60..=after + 10 * 60).contains(&until));
        assert_eq!(view.store.snoozed.len(), 2);
    });
    assert_eq!(cx.opened_url(), None);

    // A notification for the picker’s own review must dismiss it and return
    // focus to the trigger, whose action has now become Unsnooze.
    press(cx, "enter space tab");
    assert_eq!(focused(&view, cx), Some(Control::SnoozeDuration(key(1), 5)));
    view.update(cx, |view, cx| view.snooze(key(1), cx));
    cx.run_until_parked();
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    cx.update(|_, cx| assert!(view.read(cx).snooze_picker.is_none()));
    press(cx, "space");
    cx.update(|_, cx| assert!(view.read(cx).store.snooze_for(&review(1)).is_none()));
    assert_eq!(cx.opened_url(), None);
}

#[gpui::test]
fn picker_keyboard_choices_cancel_escape_and_focus_return(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(fixture);
    cx.simulate_resize(size(px(560.), px(300.)));
    view.update(cx, |view, cx| {
        // Preserve access to an older saved default outside the usual presets.
        view.store.snooze_minutes = 7;
        cx.notify();
    });
    cx.run_until_parked();
    focus(&view, Control::Snooze(key(1)), cx);
    press(cx, "enter");
    cx.update(|_, cx| {
        assert_eq!(view.read(cx).snooze_picker, Some(key(1)));
        assert!(view.read(cx).store.snoozed.is_empty());
    });
    for minutes in SNOOZE_CHOICES.into_iter().chain([7]) {
        press(cx, "tab");
        let control = Control::SnoozeDuration(key(1), minutes);
        assert_eq!(focused(&view, cx), Some(control.clone()));
        cx.update(|_, cx| {
            let keyboard = &view.read(cx).keyboard;
            let bounds = keyboard.bounds(&control).unwrap();
            assert!(bounds.top() >= keyboard.scroll.bounds().top());
            assert!(bounds.bottom() <= keyboard.scroll.bounds().bottom());
        });
    }
    press(cx, "tab");
    assert_eq!(focused(&view, cx), Some(Control::CancelSnooze(key(1))));
    press(cx, "space");
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    cx.update(|_, cx| assert!(view.read(cx).snooze_picker.is_none()));
    press(cx, "enter tab escape");
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    cx.update(|_, cx| assert!(view.read(cx).snooze_picker.is_none()));
    let before = Local::now().timestamp();
    press(cx, "enter tab tab tab space");
    let after = Local::now().timestamp();
    assert_eq!(focused(&view, cx), Some(Control::Snooze(key(1))));
    cx.update(|_, cx| {
        let view = view.read(cx);
        let until = view.store.snooze_for(&review(1)).unwrap().until;
        assert!((before + 15 * 60..=after + 15 * 60).contains(&until));
        assert_eq!(view.store.snooze_minutes, 7);
        assert!(view.snooze_picker.is_none());
    });
    assert_eq!(cx.opened_url(), None);
    press(cx, "space enter");
    focus(&view, Control::Tab(Tab::Reviews), cx);
    press(cx, "right");
    cx.update(|_, cx| {
        assert_eq!(view.read(cx).tab, Tab::Repositories);
        assert!(view.read(cx).snooze_picker.is_none());
    });
}
