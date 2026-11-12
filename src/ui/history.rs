//! The query history dialog: search what you ran, put it back in the editor.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollStrategy,
    StatefulInteractiveElement as _, Styled as _, Subscription, UniformListScrollHandle, Window,
    div, uniform_list,
};

use super::{icon, rem};
use crate::history::Entry;

pub enum HistoryEvent {
    /// Put this SQL in the editor.
    Insert(String),
    /// Put this SQL in the editor and run it.
    Run(String),
    /// Forget everything.
    Clear,
    Close,
}

pub struct HistoryView {
    /// Newest first.
    entries: Vec<Entry>,
    /// Indexes into `entries` that match the search.
    shown: Vec<usize>,
    selected: usize,
    search: Entity<InputState>,
    scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<HistoryEvent> for HistoryView {}

impl HistoryView {
    /// `entries` oldest first, as the history keeps them.
    pub fn new(entries: &[Entry], window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search SQL, connection or database…")
        });
        let sub = cx.subscribe(&search, |this, _, e: &InputEvent, cx| match e {
            InputEvent::Change => this.filter(cx),
            InputEvent::PressEnter { .. } => this.choose(false, cx),
            _ => {}
        });
        let entries: Vec<Entry> = entries.iter().rev().cloned().collect();
        HistoryView {
            shown: (0..entries.len()).collect(),
            entries,
            selected: 0,
            search,
            scroll: UniformListScrollHandle::new(),
            _subs: vec![sub],
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |i, cx| i.focus(window, cx));
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let q = self.search.read(cx).value().to_string();
        self.shown = (0..self.entries.len()).filter(|&i| self.entries[i].matches(&q)).collect();
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn current(&self) -> Option<&Entry> {
        self.shown.get(self.selected).map(|&i| &self.entries[i])
    }

    fn choose(&mut self, run: bool, cx: &mut Context<Self>) {
        if let Some(e) = self.current() {
            let sql = e.sql.clone();
            cx.emit(if run { HistoryEvent::Run(sql) } else { HistoryEvent::Insert(sql) });
        }
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.notify();
    }

    fn on_key(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        match k.key.as_str() {
            "up" => self.step(-1, cx),
            "down" => self.step(1, cx),
            "pageup" => self.step(-10, cx),
            "pagedown" => self.step(10, cx),
            "enter" if k.modifiers.control || k.modifiers.platform => self.choose(true, cx),
            "escape" => cx.emit(HistoryEvent::Close),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for HistoryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let count = self.shown.len();
        let list = uniform_list(
            "history-list",
            count,
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                let theme = cx.theme().clone();
                range
                    .filter_map(|row| this.shown.get(row).map(|&i| (row, &this.entries[i])))
                    .map(|(row, e)| {
                        let first = e.sql.trim().lines().next().unwrap_or("").to_string();
                        let more = e.sql.trim().lines().count().saturating_sub(1);
                        let outcome = match (&e.error, e.rows) {
                            (Some(_), _) => "failed".to_string(),
                            (None, Some(n)) => format!("{n} row{}", if n == 1 { "" } else { "s" }),
                            (None, None) => "ok".to_string(),
                        };
                        v_flex()
                            .id(("history-row", row))
                            .w_full()
                            .h(rem(44.))
                            .px_3()
                            .justify_center()
                            .cursor_pointer()
                            .when(row == this.selected, |d| d.bg(theme.sidebar_accent))
                            .hover(|d| d.bg(theme.sidebar_accent.opacity(0.6)))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .font_family(theme.mono_font_family.clone())
                                            .text_size(rem(12.))
                                            .child(first),
                                    )
                                    .when(more > 0, |d| {
                                        d.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!("+{more} lines")),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(e.when())
                                    .child(format!("{} · {}", e.connection, e.database))
                                    .child(format!("{} ms", e.elapsed_ms))
                                    .child(
                                        div()
                                            .when(e.error.is_some(), |d| d.text_color(theme.danger))
                                            .child(outcome),
                                    ),
                            )
                            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                                this.selected = row;
                                if e.click_count() >= 2 {
                                    this.choose(false, cx);
                                }
                                cx.notify();
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();

        let current = self.current().cloned();
        let none = current.is_none();
        v_flex()
            .w(rem(760.))
            .h(rem(600.))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .shadow_lg()
            .overflow_hidden()
            .on_key_down(cx.listener(Self::on_key))
            .child(
                h_flex()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .gap_2()
                    .child(icon("history").text_color(theme.primary))
                    .child(div().text_lg().font_semibold().child("Query history"))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if count == self.entries.len() {
                                format!("{count} queries")
                            } else {
                                format!("{count} of {}", self.entries.len())
                            }),
                    )
                    .child(
                        Button::new("history-clear")
                            .xsmall()
                            .ghost()
                            .icon(icon("trash-2"))
                            .label("Clear")
                            .disabled(self.entries.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.entries.clear();
                                this.shown.clear();
                                this.selected = 0;
                                cx.emit(HistoryEvent::Clear);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("history-close")
                            .xsmall()
                            .ghost()
                            .icon(icon("x"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(HistoryEvent::Close))),
                    ),
            )
            .child(div().px_4().pb_2().child(Input::new(&self.search).small()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(if count == 0 {
                        div()
                            .p_6()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(if self.entries.is_empty() {
                                "Nothing yet. Queries you run show up here."
                            } else {
                                "No query matches."
                            })
                            .into_any_element()
                    } else {
                        list.into_any_element()
                    }),
            )
            .children(current.map(|e| {
                v_flex()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .id("history-preview")
                            .h(rem(150.))
                            .overflow_y_scroll()
                            .px_4()
                            .py_2()
                            .bg(theme.muted.opacity(0.4))
                            .font_family(theme.mono_font_family.clone())
                            .text_size(rem(12.))
                            .child(e.sql.clone()),
                    )
                    .children(e.error.map(|err| {
                        div()
                            .px_4()
                            .py_1()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(err.lines().next().unwrap_or("").to_string())
                    }))
            }))
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("↑↓ to move · Enter inserts · Ctrl+Enter runs"),
                    )
                    .child(
                        Button::new("history-run")
                            .small()
                            .outline()
                            .icon(icon("play"))
                            .label("Run")
                            .disabled(none)
                            .on_click(cx.listener(|this, _, _, cx| this.choose(true, cx))),
                    )
                    .child(
                        Button::new("history-insert")
                            .small()
                            .primary()
                            .icon(icon("square-pen"))
                            .label("Insert")
                            .disabled(none)
                            .on_click(cx.listener(|this, _, _, cx| this.choose(false, cx))),
                    ),
            )
    }
}
