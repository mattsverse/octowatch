//! Focus identities belong to controls, never to their current row index.
//! GPUI 0.2.2 supplies Enter/Space click activation once a div tracks focus.
use std::{cell::Cell, collections::HashMap, path::PathBuf, rc::Rc};

use gpui::{
    App, Bounds, Div, FocusHandle, Pixels, ScrollHandle, Stateful, Window, canvas, div, prelude::*,
    px, rgb,
};

use crate::{
    Tab,
    theme::{Appearance, Palette},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Control {
    Refresh,
    Update,
    Tab(Tab),
    Review((String, u64)),
    Snooze((String, u64)),
    SnoozeDuration((String, u64), u64),
    CancelSnooze((String, u64)),
    RemoveRoot(PathBuf),
    AddRoot,
    Rescan,
    Repository(String),
    Appearance(Appearance),
    Poll(u64),
    SnoozeMinutes(u64),
    TestNotification,
}

struct FocusTarget {
    focus: FocusHandle,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

pub(crate) struct Keyboard {
    pub root: FocusHandle,
    pub scroll: ScrollHandle,
    order: Vec<Control>,
    targets: HashMap<Control, FocusTarget>,
    reveal_pending: Cell<bool>,
    palette: Palette,
}

impl Keyboard {
    pub fn new(cx: &mut App) -> Self {
        Self {
            root: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            order: Vec::new(),
            targets: HashMap::new(),
            reveal_pending: Cell::new(false),
            palette: Palette::LIGHT,
        }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Retain focus across reordering; move to a nearby control if it disappears.
    pub fn reconcile(&mut self, order: Vec<Control>, window: &mut Window, cx: &mut App) {
        let focused = self.focused(window);
        if focused.is_some() && self.order != order {
            self.request_reveal();
        }
        let replacement = focused
            .as_ref()
            .filter(|key| !order.contains(key))
            .map(|key| {
                if let Control::SnoozeDuration(review, _) | Control::CancelSnooze(review) = key {
                    let trigger = Control::Snooze(review.clone());
                    if order.contains(&trigger) {
                        return Some(trigger);
                    }
                }
                // Cards and Snooze buttons alternate in tab order. Recover by
                // position among controls of the same kind so a refresh cannot
                // silently change what Enter/Space will do.
                let same_kind = |candidate: &&Control| {
                    std::mem::discriminant(*candidate) == std::mem::discriminant(key)
                };
                let peers: Vec<_> = order.iter().filter(same_kind).collect();
                if !peers.is_empty() {
                    let index = self
                        .order
                        .iter()
                        .filter(same_kind)
                        .position(|old| old == key)
                        .unwrap_or(0);
                    return Some(peers[index.min(peers.len() - 1)].clone());
                }
                let index = self.order.iter().position(|old| old == key).unwrap_or(0);
                order.get(index.min(order.len().saturating_sub(1))).cloned()
            });
        self.targets.retain(|key, _| order.contains(key));
        for key in &order {
            self.targets
                .entry(key.clone())
                .or_insert_with(|| FocusTarget {
                    focus: cx.focus_handle().tab_stop(true),
                    bounds: Rc::new(Cell::new(None)),
                });
        }
        self.order = order;
        if let Some(replacement) = replacement {
            if let Some(key) = replacement {
                self.focus(&key, window);
            } else {
                window.focus(&self.root);
            }
        }
    }

    pub fn focused(&self, window: &Window) -> Option<Control> {
        self.order
            .iter()
            .find(|key| self.targets[*key].focus.is_focused(window))
            .cloned()
    }

    pub fn focus(&self, key: &Control, window: &mut Window) {
        if let Some(target) = self.targets.get(key) {
            window.focus(&target.focus);
            self.request_reveal();
        }
    }

    pub fn request_reveal(&self) {
        self.reveal_pending.set(true);
    }

    pub fn control(&self, key: Control) -> Stateful<Div> {
        let target = &self.targets[&key];
        let theme = self.palette;
        let bounds = target.bounds.clone();
        div()
            .id(gpui::SharedString::from(format!("{key:?}")))
            .relative()
            .track_focus(&target.focus)
            .tab_index(0)
            .border_1()
            .border_color(gpui::transparent_black())
            .focus(|style| {
                style
                    .border_color(rgb(theme.focus))
                    .bg(rgb(theme.surface_hover))
                    .text_color(rgb(theme.text))
                    .opacity(1.)
            })
            .child(
                canvas(move |rect, _, _| bounds.set(Some(rect)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
    }

    /// Reveal only the part outside the viewport, preserving the user's scroll.
    /// Called after layout too, so newly inserted/repositioned controls are visible.
    pub fn reveal(&self, window: &mut Window) {
        if !self.reveal_pending.replace(false) {
            return;
        }
        let Some(key) = self.focused(window) else {
            return;
        };
        if matches!(key, Control::Refresh | Control::Update | Control::Tab(_)) {
            return;
        }
        let Some(bounds) = self.targets[&key].bounds.get() else {
            return;
        };
        let viewport = self.scroll.bounds();
        let mut offset = self.scroll.offset();
        let adjustment = if bounds.top() < viewport.top() {
            viewport.top() - bounds.top()
        } else if bounds.bottom() > viewport.bottom() {
            viewport.bottom() - bounds.bottom()
        } else {
            px(0.)
        };
        if adjustment != px(0.) {
            offset.y += adjustment;
            self.scroll.set_offset(offset);
            window.refresh();
        }
    }

    #[cfg(test)]
    pub fn bounds(&self, key: &Control) -> Option<Bounds<Pixels>> {
        self.targets.get(key)?.bounds.get()
    }

    pub fn review_neighbor(&self, current: &Control, key: &str) -> Option<Control> {
        let snooze = matches!(current, Control::Snooze(_));
        if !snooze && !matches!(current, Control::Review(_)) {
            return None;
        }
        let rows: Vec<_> = self
            .order
            .iter()
            .filter(|control| {
                if snooze {
                    matches!(control, Control::Snooze(_))
                } else {
                    matches!(control, Control::Review(_))
                }
            })
            .collect();
        let index = rows.iter().position(|control| *control == current)?;
        let next = match key {
            "up" => index.saturating_sub(1),
            "down" => (index + 1).min(rows.len() - 1),
            "home" => 0,
            "end" => rows.len() - 1,
            _ => return None,
        };
        Some(rows[next].clone())
    }
}
