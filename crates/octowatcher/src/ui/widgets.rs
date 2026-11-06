//! Small pieces the three tabs share: grouped sections, rows, chips, badges and empty states.

use rune::{El, Icon, TextStyle, col, el, icon, row, text};

use crate::theme::{Theme, blend, monogram as monogram_color};
use rune_kit::{Button, ControlSize, IconButton};

pub const TITLE: TextStyle = TextStyle::new(13.0, 600.0);
pub const BODY: TextStyle = TextStyle::new(12.5, 400.0);
pub const BODY_MEDIUM: TextStyle = TextStyle::new(13.0, 500.0);
pub const META: TextStyle = TextStyle::new(11.5, 400.0);
pub const META_MEDIUM: TextStyle = TextStyle::new(11.5, 520.0);
pub const SECTION: TextStyle = TextStyle::new(11.0, 600.0).tracking(0.2);
pub const BADGE: TextStyle = TextStyle::new(11.0, 560.0);
pub const HEADING: TextStyle = TextStyle::new(14.0, 620.0);

/// Space between a card's edge and its content.
pub const INSET: f32 = 14.0;
/// Every row's leading tile, and the gap after it: row text starts at `TEXT`.
pub const TILE: f32 = 28.0;
pub const TILE_GAP: f32 = 12.0;
pub const TEXT: f32 = INSET + TILE + TILE_GAP;
/// The one height every button, field and pop-up shares.
pub const CONTROL: f32 = 24.0;
/// The width of a row's pull-down, so a column of them lines up.
pub const POPUP: f32 = 112.0;

/// A kit button at the app's control height.
pub fn button(label: impl Into<String>) -> Button {
    Button::new(label).control_size(ControlSize::Regular)
}

/// A kit icon button at the app's control height.
pub fn icon_button(i: &'static Icon) -> IconButton {
    IconButton::new(i).control_size(ControlSize::Regular)
}

/// A titled card of rows split by hairlines, as macOS settings group them.
pub fn section(t: &Theme, key: &str, title: Option<&str>, rows: Vec<El>) -> El {
    section_inset(t, key, title, rows, TEXT)
}

/// A section whose separators start at `inset` (rows without leading tiles start at `INSET`).
pub fn section_inset(t: &Theme, key: &str, title: Option<&str>, rows: Vec<El>, inset: f32) -> El {
    let mut card = col()
        .key("card")
        .w_full()
        .fill(t.card)
        .radius(10.0)
        .border(t.hair(), t.card_border)
        .shadow(1.0, 2.0, t.shadow)
        .clip_rounded(10.0);
    let count = rows.len();
    for (ix, r) in rows.into_iter().enumerate() {
        card = card.child(r);
        if ix + 1 < count {
            card = card.child(separator(t, ix).ml(inset));
        }
    }
    let mut section = col().key(key).w_full().gap(7.0);
    if let Some(title) = title {
        section = section.child(
            text(title)
                .key("heading")
                .font(SECTION)
                .color(t.muted)
                .ml(INSET)
                .role("heading"),
        );
    }
    section.child(card)
}

/// A hairline between rows, inset to the rows' text column.
pub fn separator(t: &Theme, ix: usize) -> El {
    el().key(format!("sep{ix}"))
        .h(t.hair())
        .ml(TEXT)
        .fill(t.separator)
        .a11y_hidden()
}

/// A row in a section: a leading glyph, a title over an optional detail, and a trailing control.
pub struct Row {
    key: String,
    leading: Option<El>,
    title: El,
    detail: Option<El>,
    trailing: Option<El>,
    below: Option<El>,
}

impl Row {
    pub fn new(key: impl Into<String>, t: &Theme, title: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            leading: None,
            title: text(title)
                .key("title")
                .font(BODY_MEDIUM)
                .color(t.text)
                .truncate(),
            detail: None,
            trailing: None,
            below: None,
        }
    }

    pub fn title_el(mut self, title: El) -> Self {
        self.title = title;
        self
    }

    pub fn leading(mut self, e: El) -> Self {
        self.leading = Some(e);
        self
    }

    pub fn detail(mut self, t: &Theme, detail: impl Into<String>) -> Self {
        self.detail = Some(
            text(detail)
                .key("detail")
                .font(META)
                .color(t.muted)
                .max_lines(4),
        );
        self
    }

    pub fn detail_el(mut self, detail: El) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn trailing(mut self, e: El) -> Self {
        self.trailing = Some(e);
        self
    }

    pub fn trailing_if(self, show: bool, e: impl FnOnce() -> El) -> Self {
        if show { self.trailing(e()) } else { self }
    }

    /// Content under the row's text, aligned with it (account chips, errors).
    pub fn below(mut self, e: El) -> Self {
        self.below = Some(e);
        self
    }

    pub fn build(self) -> El {
        let tall = self.below.is_some();
        let mut body = col()
            .key("body")
            .grow(1.0)
            .min_w(0.0)
            .gap(2.0)
            .child(self.title);
        if let Some(detail) = self.detail {
            body = body.child(detail);
        }
        if let Some(below) = self.below {
            body = body.child(below.mt(8.0));
        }
        let mut r = row()
            .key(self.key)
            .w_full()
            .min_h(46.0)
            .px(INSET)
            .py(8.0)
            .gap(TILE_GAP);
        r = if tall { r.items_start() } else { r.center_y() };
        // Rows without a glyph still start their text on the shared column.
        r = r.child(
            self.leading
                .unwrap_or_else(|| el().key("glyph").size(TILE, TILE).a11y_hidden()),
        );
        r = r.child(body);
        if let Some(trailing) = self.trailing {
            r = r.child(trailing);
        }
        r
    }
}

/// A rounded square holding a line icon, leading a row.
pub fn glyph_tile(t: &Theme, i: &'static Icon, tint: rune::Color) -> El {
    let bg = blend(t.card, tint, if t.dark { 0.16 } else { 0.11 });
    el().key("glyph")
        .size(TILE, TILE)
        .radius(8.0)
        .fill(bg)
        .center()
        .a11y_hidden()
        .child(icon(i, 16.0).color(tint))
}

/// The owner's initial on a steady colour.
/// `quiet` greys it out (a snoozed review, an unwatched repository) while the initial stays
/// readable.
pub fn monogram(t: &Theme, owner: &str, slot: Option<usize>, quiet: bool) -> El {
    let initial = owner
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    let fill = if quiet {
        blend(t.card, t.muted, if t.dark { 0.55 } else { 0.75 })
    } else {
        monogram_color(owner, slot, t.dark)
    };
    el().key("monogram")
        .size(TILE, TILE)
        .radius(8.0)
        .fill(fill)
        .center()
        .a11y_hidden()
        .child(
            text(initial)
                .font(TextStyle::new(13.0, 650.0))
                .color(rune::Color::WHITE),
        )
}

/// A person: a neutral circle with their initial, so accounts never read as repository owners.
pub fn person(t: &Theme, login: &str, quiet: bool) -> El {
    // "sam-work" reads "SW", so accounts that share a first letter still differ.
    let initial: String = login
        .split(['-', '_', '.'])
        .filter_map(|part| part.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    el().key("avatar")
        .size(TILE, TILE)
        .radius(TILE / 2.0)
        // A graphite circle of their own: accounts never borrow an organisation's colour.
        // An accent tint of their own: never an organisation's colour, never "inactive" grey.
        .fill(if quiet {
            blend(t.card, t.text, 0.10)
        } else {
            blend(t.card, t.accent, if t.dark { 0.30 } else { 0.16 })
        })
        .center()
        .a11y_hidden()
        .child(
            text(initial)
                .font(TextStyle::new(
                    if login.contains(['-', '_', '.']) {
                        10.5
                    } else {
                        12.5
                    },
                    650.0,
                ))
                .color(if quiet {
                    t.muted
                } else if t.dark {
                    t.text
                } else {
                    t.accent
                }),
        )
}

/// A small coloured label (Draft, Re-review, Snoozed).
pub fn badge(key: &str, label: impl Into<String>, fg: rune::Color, bg: rune::Color) -> El {
    row()
        .key(key)
        .h(19.0)
        .px(7.0)
        .radius(5.0)
        .fill(bg)
        .center_y()
        .child(text(label).font(BADGE).color(fg))
}

pub fn badge_icon(
    key: &str,
    i: &'static Icon,
    label: impl Into<String>,
    fg: rune::Color,
    bg: rune::Color,
) -> El {
    row()
        .key(key)
        .h(19.0)
        .pl(5.0)
        .pr(7.0)
        .gap(4.0)
        .radius(5.0)
        .fill(bg)
        .center_y()
        .child(icon(i, 12.0).color(fg).a11y_hidden())
        .child(text(label).font(BADGE).color(fg))
}

/// A dot in a status colour.
pub fn dot(color: rune::Color) -> El {
    el().key("dot")
        .size(7.0, 7.0)
        .radius(3.5)
        .fill(color)
        .a11y_hidden()
}

/// A centred icon, title and explanation for a list with nothing in it.
pub fn empty_state(
    t: &Theme,
    key: &str,
    i: &'static Icon,
    tint: rune::Color,
    title: &str,
    body: &str,
) -> El {
    col()
        .key(key)
        .w_full()
        .items_center()
        .gap(6.0)
        .pt(140.0)
        .pb(24.0)
        .px(32.0)
        .child(
            el().key("art")
                .size(52.0, 52.0)
                .radius(14.0)
                .fill(t.card)
                .border(t.hair(), t.card_border)
                .shadow(1.0, 3.0, t.shadow)
                .center()
                .mb(10.0)
                .a11y_hidden()
                .child(icon(i, 24.0).color(tint)),
        )
        .child(
            text(title)
                .key("title")
                .font(HEADING)
                .color(t.text)
                .role("heading"),
        )
        .child(
            text(body)
                .key("body")
                .font(BODY)
                .color(t.muted)
                .max_w(340.0)
                .align(rune::Align::Center),
        )
}

/// A tinted strip with an icon and a message (setup warnings, update offers).
/// A card-coloured strip with a tinted glyph and a message (setup warnings, update offers).
/// The colour stays in the glyph, so a banner never outweighs the list under it.
pub fn banner(
    t: &Theme,
    key: &str,
    i: &'static Icon,
    tint: rune::Color,
    message: impl Into<String>,
    action: Option<El>,
) -> El {
    banner_with(
        t,
        key,
        i,
        tint,
        text(message)
            .key("message")
            .font(BODY)
            .color(t.text)
            .max_lines(3),
        action,
    )
}

/// A banner whose message is any element (a title over a subtitle).
pub fn banner_with(
    t: &Theme,
    key: &str,
    i: &'static Icon,
    tint: rune::Color,
    message: El,
    action: Option<El>,
) -> El {
    row()
        .key(key)
        .w_full()
        .min_h(52.0)
        .px(INSET)
        .py(10.0)
        .gap(TILE_GAP)
        .radius(10.0)
        .fill(t.card)
        .border(t.hair(), t.card_border)
        .shadow(1.0, 2.0, t.shadow)
        .center_y()
        .role("status")
        .child(glyph_tile(t, i, tint))
        .child(message.grow(1.0).min_w(0.0))
        .children(action)
}

/// A segmented control: a recessed track with a raised pill under the chosen segment, which
/// slides between segments.
pub fn segmented<S: 'static>(
    t: &Theme,
    key: &str,
    label: &str,
    options: &[&str],
    selected: usize,
    width: f32,
    pick: impl Fn(&mut S, usize, &mut rune::Tx) + Clone + 'static,
) -> El {
    let n = options.len().max(1) as f32;
    let inner = width - 4.0;
    let seg = inner / n;
    let track = if t.dark {
        blend(t.card, t.text, 0.10)
    } else {
        blend(t.bg, t.text, 0.06)
    };
    let pill = if t.dark {
        blend(t.card, t.text, 0.34)
    } else {
        t.raised
    };
    let mut root = row()
        .key(key)
        .role("radiogroup")
        .label(label)
        .focus_group()
        .w(width)
        .h(CONTROL)
        .padding(2.0)
        .radius(7.0)
        .fill(track)
        .child(
            el().key("indicator")
                .absolute(2.0 + seg * selected as f32, 2.0)
                .w(seg)
                .h(CONTROL - 4.0)
                .radius(5.0)
                .fill(pill)
                .border(
                    t.hair(),
                    if t.dark {
                        t.text.alpha(0.08)
                    } else {
                        t.text.alpha(0.06)
                    },
                )
                .shadow(
                    1.0,
                    2.0,
                    rune::Color::BLACK.alpha(if t.dark { 0.35 } else { 0.10 }),
                )
                .motion(rune::tok::motion::SNAPPY)
                .a11y_hidden(),
        );
    for (ix, option) in options.iter().enumerate() {
        let on = ix == selected;
        let pick = pick.clone();
        root = root.child(
            row()
                .key(format!("segment{ix}"))
                .role("radio")
                .label(option.to_string())
                .checked(on)
                .position_in_set(ix + 1, options.len())
                .focusable(true)
                .focus_selected(on)
                .focus_ring(2.0, 1.0, t.k.c.border_focus)
                .w(seg)
                .h(CONTROL - 4.0)
                .radius(5.0)
                .center()
                .cursor(rune::Cursor::Pointer)
                .hover_group()
                .on_press_tx(move |s: &mut S, tx| pick(s, ix, tx))
                .child(
                    text(option.to_string())
                        .font(if on {
                            TextStyle::new(12.0, 590.0)
                        } else {
                            TextStyle::new(12.0, 450.0)
                        })
                        .color(if on {
                            t.text
                        } else {
                            blend(t.text, t.card, 0.28)
                        })
                        .group_color(t.text)
                        .truncate()
                        .a11y_hidden(),
                ),
        );
    }
    root
}

/// A bordered trigger with a chevron, for a control that opens a menu.
pub fn popup_button<S: 'static>(
    t: &Theme,
    emphasised: bool,
    key: &str,
    label: impl Into<String>,
    help: &str,
    open: bool,
    press: impl Fn(&mut S) + 'static,
) -> El {
    let label = label.into();
    row()
        .key(key)
        .h(CONTROL)
        .w(POPUP)
        .pl(8.0)
        .pr(5.0)
        .gap(4.0)
        .radius(6.0)
        // The same frame as the Settings pop-ups, so every pick-one control matches.
        .fill(if open {
            blend(t.raised, t.text, 0.06)
        } else {
            t.raised
        })
        .border(t.hair(), t.text.alpha(0.14))
        .shadow(
            0.5,
            1.0,
            rune::Color::BLACK.alpha(if t.dark { 0.25 } else { 0.05 }),
        )
        .center_y()
        .role("button")
        .label(label.clone())
        .help(help.to_string())
        .expanded(open)
        .focusable(true)
        .focus_ring(2.0, 1.0, t.k.c.border_focus)
        .hover(t.hover)
        .press(t.pressed)
        .cursor(rune::Cursor::Pointer)
        .on_press(press)
        // A narrowed scope (an override of the default) reads in medium weight.
        .child(
            text(label)
                .font(if emphasised { META_MEDIUM } else { META })
                .color(t.text)
                .truncate()
                .grow(1.0)
                .min_w(0.0)
                .a11y_hidden(),
        )
        .child(
            icon(&super::icons::CHEVRON_UP_DOWN, 13.0)
                .color(t.muted)
                .a11y_hidden(),
        )
}

/// A macOS-proportioned switch: a 38×22 capsule with a round knob that travels its length.
/// The knob carries a shadow and a hairline, so it reads on white cards in light mode.
pub fn switch<S: 'static>(
    t: &Theme,
    key: &str,
    label: impl Into<String>,
    on: bool,
    enabled: bool,
    press: impl Fn(&mut S, &mut rune::Tx) + 'static,
) -> El {
    switch_paused(t, key, label, on, enabled, false, press)
}

/// A switch that's on but `paused`: its knob stays on, over a neutral track.
pub fn switch_paused<S: 'static>(
    t: &Theme,
    key: &str,
    label: impl Into<String>,
    on: bool,
    enabled: bool,
    paused: bool,
    press: impl Fn(&mut S, &mut rune::Tx) + 'static,
) -> El {
    let (w, h, knob) = (34.0, 20.0, 16.0);
    let inset = (h - knob) / 2.0;
    // Paused is the on-state at half strength with a full white knob, in both appearances.
    let track = if on && paused {
        blend(t.card, t.fill, 0.45)
    } else if on {
        t.fill
    } else if t.dark {
        blend(t.card, t.text, 0.18)
    } else {
        blend(t.card, t.text, 0.14)
    };
    let mut e = el()
        .key(key)
        .role("switch")
        .label(label)
        .checked(on)
        .disabled(!enabled)
        .size(w, h)
        .radius(h / 2.0)
        .fill(track)
        .motion(rune::tok::motion::SNAPPY)
        .focusable(enabled)
        .focus_ring(2.0, 2.0, t.k.c.border_focus)
        .child(
            el().key("knob")
                .size(knob, knob)
                .absolute(if on { w - knob - inset } else { inset }, inset)
                .radius(knob / 2.0)
                .fill(rune::Color::WHITE)
                .border(
                    t.hair(),
                    rune::Color::BLACK.alpha(if t.dark { 0.0 } else { 0.06 }),
                )
                .shadow(
                    1.0,
                    2.5,
                    rune::Color::BLACK.alpha(if t.dark { 0.45 } else { 0.25 }),
                )
                .shadow(0.0, 0.5, rune::Color::BLACK.alpha(0.12))
                .motion(rune::tok::motion::SNAPPY)
                .a11y_hidden(),
        );
    if enabled {
        e = e.cursor(rune::Cursor::Pointer).on_press_tx(press);
    } else {
        if !paused {
            e = e.opacity(0.45);
        }
    }
    e
}

/// A small square icon control with a hairline ring: reads as a button at rest without the
/// weight of a filled bezel. Pressed while `open` (its menu shows).
pub fn ring_button<S: 'static>(
    t: &Theme,
    key: &str,
    i: &'static Icon,
    label: &str,
    open: bool,
    press: impl Fn(&mut S, &mut rune::Tx) + 'static,
) -> El {
    el().key(key)
        .size(CONTROL + 4.0, CONTROL)
        .radius(6.0)
        .fill(if open { t.pressed } else { t.card })
        .center()
        .role("button")
        .label(label.to_string())
        .help(label.trim_end_matches('…').to_string())
        // The ring is faint at rest and comes up with its row's hover, so a column of these
        // stays quiet while each still reads as a control.
        .child(
            el().key("ring")
                .absolute(0.0, 0.0)
                .size(CONTROL + 4.0, CONTROL)
                .radius(6.0)
                .border(
                    t.hair(),
                    if t.dark {
                        t.text.alpha(0.16)
                    } else {
                        t.text.alpha(0.14)
                    },
                )
                .group_opacity(if open { 1.0 } else { 0.0 }, 1.0)
                .a11y_hidden(),
        )
        .expanded(open)
        .focusable(true)
        .focus_ring(2.0, 1.0, t.k.c.border_focus)
        .hover(t.hover)
        .press(t.pressed)
        .cursor(rune::Cursor::Pointer)
        .on_press_tx(press)
        .child(
            icon(i, 15.0)
                .color(if open {
                    t.text
                } else {
                    blend(t.muted, t.card, 0.35)
                })
                .group_color(t.muted)
                .a11y_hidden(),
        )
}

/// An accent text action, flush to the trailing edge: the app's one quiet-action style
/// (Clear Filters, a passing check's Manage or Details).
pub fn link<S: 'static>(
    t: &Theme,
    key: &str,
    label: &str,
    press: impl Fn(&mut S, &mut rune::Tx) + 'static,
) -> El {
    text(label.to_string())
        .key(key)
        .font(META_MEDIUM)
        .color(t.accent)
        .role("button")
        .label(label.to_string())
        .focusable(true)
        .focus_ring(2.0, 2.0, t.k.c.border_focus)
        .cursor(rune::Cursor::Pointer)
        .on_press_tx(press)
}
