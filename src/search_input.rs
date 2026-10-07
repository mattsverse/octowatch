//! A single-line native GPUI input, including UTF-16 platform input and IME
//! composition. Editing positions are UTF-8 byte offsets at character boundaries.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementInputHandler, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, KeyBinding, Pixels, Point, ShapedLine, SharedString,
    TextRun, UTF16Selection, UnderlineStyle, Window, actions, canvas, div, fill, point, prelude::*,
    px, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::Palette;

actions!(
    review_search_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Copy,
        Cut
    ]
);

pub struct Changed(pub String);

#[derive(Default)]
struct EditBuffer {
    text: String,
    anchor: usize,
    cursor: usize,
    marked: Option<Range<usize>>,
}

impl EditBuffer {
    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    fn utf8_offset(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (byte, ch) in self.text.char_indices() {
            if units >= utf16 {
                return byte;
            }
            units += ch.len_utf16();
        }
        self.text.len()
    }

    fn utf16_offset(&self, byte: usize) -> usize {
        self.text[..byte].encode_utf16().count()
    }

    fn range_to_utf8(&self, range: Range<usize>) -> Range<usize> {
        let start = self.utf8_offset(range.start);
        start..self.utf8_offset(range.end).max(start)
    }

    fn range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.utf16_offset(range.start)..self.utf16_offset(range.end)
    }

    fn previous(&self) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < self.cursor).then_some(ix))
            .unwrap_or(0)
    }

    fn next(&self) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > self.cursor).then_some(ix))
            .unwrap_or(self.text.len())
    }

    fn replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        composing: bool,
    ) {
        let range = range
            .map(|r| self.range_to_utf8(r))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.selection());
        // Search is single-line even when the clipboard/platform supplies newlines.
        let text = text.replace(['\n', '\r', '\t'], " ");
        self.text.replace_range(range.clone(), &text);
        self.marked =
            (composing && !text.is_empty()).then_some(range.start..range.start + text.len());
        let selected = selection
            .map(|r| {
                let composing_text = EditBuffer {
                    text: text.clone(),
                    ..Default::default()
                };
                let r = composing_text.range_to_utf8(r);
                range.start + r.start..range.start + r.end
            })
            .unwrap_or(range.start + text.len()..range.start + text.len());
        self.anchor = selected.start;
        self.cursor = selected.end;
    }
}

pub struct SearchInput {
    palette: Palette,
    focus: FocusHandle,
    buffer: EditBuffer,
    layout: Option<ShapedLine>,
    bounds: Option<Bounds<Pixels>>,
    scroll_x: Pixels,
    selecting: bool,
}

impl EventEmitter<Changed> for SearchInput {}

impl SearchInput {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            palette: Palette::DARK,
            focus: cx.focus_handle().tab_stop(true),
            buffer: EditBuffer::default(),
            layout: None,
            bounds: None,
            scroll_x: px(0.),
            selecting: false,
        }
    }

    pub fn palette(&self) -> Palette {
        self.palette
    }

    pub fn set_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        if self.palette != palette {
            self.palette = palette;
            cx.notify();
        }
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.buffer = EditBuffer::default();
        self.scroll_x = px(0.);
        self.changed(cx);
    }

    fn changed(&self, cx: &mut Context<Self>) {
        cx.emit(Changed(self.buffer.text.clone()));
        cx.notify();
    }

    fn move_cursor(&mut self, to: usize, extend: bool, cx: &mut Context<Self>) {
        self.buffer.cursor = to;
        if !extend {
            self.buffer.anchor = to;
        }
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.buffer.selection().is_empty() {
            self.buffer.previous()
        } else {
            self.buffer.selection().start
        };
        self.move_cursor(to, false, cx);
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.buffer.selection().is_empty() {
            self.buffer.next()
        } else {
            self.buffer.selection().end
        };
        self.move_cursor(to, false, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(self.buffer.previous(), true, cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(self.buffer.next(), true, cx);
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(0, false, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(self.buffer.text.len(), false, cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.anchor = 0;
        self.move_cursor(self.buffer.text.len(), true, cx);
    }
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.selection().is_empty() {
            self.buffer.anchor = self.buffer.previous();
        }
        self.replace_text_in_range(None, "", window, cx);
    }
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.selection().is_empty() {
            self.buffer.anchor = self.buffer.next();
        }
        self.replace_text_in_range(None, "", window, cx);
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.buffer.selection();
        if !range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.buffer.text[range].to_owned(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.buffer.selection().is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn index_at(&self, position: Point<Pixels>) -> usize {
        if self.buffer.text.is_empty() {
            return 0;
        }
        match (&self.layout, self.bounds) {
            (Some(line), Some(bounds)) => {
                let mut ix = line
                    .closest_index_for_x(position.x - bounds.left() + self.scroll_x)
                    .min(self.buffer.text.len());
                // Platform input can replace text before the next paint.
                while !self.buffer.text.is_char_boundary(ix) {
                    ix -= 1;
                }
                ix
            }
            _ => 0,
        }
    }

    pub fn bind_keys(cx: &mut App) {
        let context = Some("ReviewSearchInput");
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
            KeyBinding::new("left", Left, context),
            KeyBinding::new("right", Right, context),
            KeyBinding::new("shift-left", SelectLeft, context),
            KeyBinding::new("shift-right", SelectRight, context),
            KeyBinding::new("home", Home, context),
            KeyBinding::new("end", End, context),
        ]);
        let modifier = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        cx.bind_keys([
            KeyBinding::new(&format!("{modifier}-a"), SelectAll, context),
            KeyBinding::new(&format!("{modifier}-v"), Paste, context),
            KeyBinding::new(&format!("{modifier}-c"), Copy, context),
            KeyBinding::new(&format!("{modifier}-x"), Cut, context),
        ]);
    }
}

impl Focusable for SearchInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for SearchInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.buffer.range_to_utf8(range);
        *actual = Some(self.buffer.range_to_utf16(range.clone()));
        Some(self.buffer.text[range].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.range_to_utf16(self.buffer.selection()),
            reversed: self.buffer.cursor < self.buffer.anchor,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.buffer
            .marked
            .clone()
            .map(|r| self.buffer.range_to_utf16(r))
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.marked = None;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.replace(range, text, None, false);
        self.changed(cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.replace(range, text, selection, true);
        self.changed(cx);
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let bounds = self.bounds?;
        let line = self.layout.as_ref()?;
        let range = self.buffer.range_to_utf8(range);
        Some(Bounds::from_corners(
            point(
                bounds.left() + line.x_for_index(range.start) - self.scroll_x,
                bounds.top(),
            ),
            point(
                bounds.left() + line.x_for_index(range.end) - self.scroll_x,
                bounds.bottom(),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.bounds?.localize(&position)?;
        Some(self.buffer.utf16_offset(self.index_at(position)))
    }
}

impl Render for SearchInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.palette;
        let input = cx.entity();
        let painted_input = input.clone();
        div()
            .id("review-search-input")
            .w_full()
            .min_w_0()
            .key_context("ReviewSearchInput")
            .track_focus(&self.focus)
            .tab_stop(true)
            .cursor(CursorStyle::IBeam)
            .border_1()
            .border_color(rgb(theme.border))
            .rounded_md()
            .bg(rgb(theme.surface))
            .px_2()
            .py_2()
            .focus(|s| s.border_color(rgb(theme.focus)))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    window.focus(&this.focus);
                    this.selecting = true;
                    this.move_cursor(this.index_at(event.position), event.modifiers.shift, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.selecting {
                    this.move_cursor(this.index_at(event.position), true, cx);
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        input.update(cx, |input, _| {
                            let style = window.text_style();
                            let empty = input.buffer.text.is_empty();
                            let text: SharedString = if empty {
                                "Search title, repository, author, #number…".into()
                            } else {
                                input.buffer.text.clone().into()
                            };
                            let run = TextRun {
                                len: text.len(),
                                font: style.font(),
                                color: if empty {
                                    rgb(theme.secondary_text).into()
                                } else {
                                    style.color
                                },
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            let runs = if let Some(marked) = &input.buffer.marked {
                                vec![
                                    TextRun {
                                        len: marked.start,
                                        ..run.clone()
                                    },
                                    TextRun {
                                        len: marked.len(),
                                        underline: Some(UnderlineStyle {
                                            color: None,
                                            thickness: px(1.),
                                            wavy: false,
                                        }),
                                        ..run.clone()
                                    },
                                    TextRun {
                                        len: text.len() - marked.end,
                                        ..run
                                    },
                                ]
                                .into_iter()
                                .filter(|r| r.len > 0)
                                .collect()
                            } else {
                                vec![run]
                            };
                            let line = window.text_system().shape_line(
                                text,
                                style.font_size.to_pixels(window.rem_size()),
                                &runs,
                                None,
                            );
                            let cursor = line.x_for_index(input.buffer.cursor);
                            let width = (bounds.size.width - px(2.)).max(px(1.));
                            if cursor < input.scroll_x {
                                input.scroll_x = cursor;
                            }
                            if cursor > input.scroll_x + width {
                                input.scroll_x = cursor - width;
                            }
                            input.scroll_x = input.scroll_x.min((line.width - width).max(px(0.)));
                            input.bounds = Some(bounds);
                            input.layout = Some(line);
                        });
                    },
                    move |bounds, _, window, cx| {
                        let input = painted_input.read(cx);
                        let focus = input.focus.clone();
                        let layout = input.layout.clone();
                        let scroll_x = input.scroll_x;
                        let selected = input.buffer.selection();
                        let cursor = input.buffer.cursor;
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, painted_input.clone()),
                            cx,
                        );
                        if let Some(line) = &layout {
                            let origin = point(bounds.left() - scroll_x, bounds.top());
                            window.with_content_mask(
                                Some(gpui::ContentMask { bounds }),
                                |window| {
                                    if focus.is_focused(window) {
                                        if !selected.is_empty() {
                                            window.paint_quad(fill(
                                                Bounds::from_corners(
                                                    point(
                                                        origin.x + line.x_for_index(selected.start),
                                                        bounds.top(),
                                                    ),
                                                    point(
                                                        origin.x + line.x_for_index(selected.end),
                                                        bounds.bottom(),
                                                    ),
                                                ),
                                                rgba((theme.accent << 8) | 0x40),
                                            ));
                                        } else {
                                            window.paint_quad(fill(
                                                Bounds::new(
                                                    point(
                                                        origin.x + line.x_for_index(cursor),
                                                        bounds.top(),
                                                    ),
                                                    size(px(1.), bounds.size.height),
                                                ),
                                                rgb(theme.accent),
                                            ));
                                        }
                                    }
                                    let _ = line.paint(origin, bounds.size.height, window, cx);
                                },
                            );
                        }
                    },
                )
                .w_full()
                .h(px(20.)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_ranges_replace_unicode_without_splitting_characters() {
        let mut buffer = EditBuffer::default();
        buffer.replace(None, "A🦀éZ", None, false);
        assert_eq!(buffer.range_to_utf16(1..7), 1..4);
        buffer.replace(Some(1..3), "猫", None, false);
        assert_eq!(buffer.text, "A猫éZ");
        assert_eq!(buffer.cursor, 4);
        assert_eq!(buffer.anchor, 4);
        buffer.replace(Some(999..1000), "\r\nnext\t", None, false);
        assert_eq!(buffer.text, "A猫éZ  next ");
    }

    #[test]
    fn composing_selection_is_relative_to_the_inserted_text() {
        let mut buffer = EditBuffer::default();
        buffer.replace(None, "prefix suffix", None, false);
        buffer.replace(Some(7..13), "猫🦀", Some(1..3), true);
        assert_eq!(buffer.text, "prefix 猫🦀");
        assert_eq!(buffer.marked, Some(7..14));
        assert_eq!(buffer.selection(), 10..14);
        buffer.replace(None, "review", None, false);
        assert_eq!(buffer.text, "prefix review");
        assert!(buffer.marked.is_none());
        assert_eq!(buffer.selection(), 13..13);
    }

    #[test]
    fn cursor_boundaries_keep_combining_marks_and_emoji_together() {
        let mut buffer = EditBuffer::default();
        buffer.replace(None, "e\u{301}👩‍💻", None, false);
        assert_eq!(buffer.previous(), "e\u{301}".len());
        buffer.cursor = buffer.previous();
        assert_eq!(buffer.previous(), 0);
        assert_eq!(buffer.next(), buffer.text.len());
        buffer.cursor = 0;
        assert_eq!(buffer.next(), "e\u{301}".len());
    }
}
