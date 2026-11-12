//! A Data tab: browse a table or view a page at a time, with a WHERE filter
//! and server-side sorting.

use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    table::{DataTable, TableState},
    v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
};

use super::grid::{self, ResultGrid};
use super::query::QueryEvent;
use super::{CopySelection, icon};
use crate::db::{self, DbObject, Session};
use crate::rt;

const PAGE: usize = 500;

pub struct DataTab {
    pub title: SharedString,
    pub session: Arc<Session>,
    pub object: DbObject,
    filter: Entity<InputState>,
    table: Entity<TableState<ResultGrid>>,
    page: usize,
    sort: Option<(String, bool)>,
    loading: bool,
    error: Option<String>,
    rows_on_page: usize,
    elapsed_ms: f64,
    _subs: Vec<Subscription>,
}

impl EventEmitter<QueryEvent> for DataTab {}

impl DataTab {
    pub fn new(
        session: Arc<Session>,
        object: DbObject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g. status = 'paid' AND placed_at > now() - interval '1 day'")
        });
        let table = cx.new(|cx| {
            let mut t = TableState::new(ResultGrid::new(true), window, cx);
            t.cell_selectable = true;
            t
        });
        let subs = vec![
            cx.subscribe(&filter, |this, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) {
                    this.page = 0;
                    this.load(cx);
                }
            }),
            // A header click asks for a server-side sort.
            cx.observe(&table, |this, table, cx| {
                let req = table.update(cx, |t, _| t.delegate_mut().sort_request.take());
                if let Some((col, asc)) = req {
                    let name = table.read(cx).delegate().columns().get(col).map(|c| c.name.clone());
                    if let Some(name) = name {
                        this.sort = Some((name, asc));
                        this.page = 0;
                        this.load(cx);
                    }
                }
            }),
        ];
        let mut tab = DataTab {
            title: format!("{}", object.name).into(),
            session,
            object,
            filter,
            table,
            page: 0,
            sort: None,
            loading: false,
            error: None,
            rows_on_page: 0,
            elapsed_ms: 0.,
            _subs: subs,
        };
        tab.load(cx);
        tab
    }

    fn sql(&self, cx: &gpui_kit::App) -> String {
        let filter = self.filter.read(cx).value().to_string();
        db::browse_sql(
            &self.object,
            &filter,
            self.sort.as_ref().map(|(c, a)| (c.as_str(), *a)),
            PAGE,
            self.page * PAGE,
        )
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        let sql = self.sql(cx);
        self.loading = true;
        self.error = None;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_loading(true);
            t.refresh(cx);
        });
        let session = self.session.clone();
        let fut = rt::spawn(async move { session.run(&sql, PAGE).await });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match out {
                    Ok(out) => {
                        this.elapsed_ms = out.elapsed.as_secs_f64() * 1000.;
                        let r = out.results.into_iter().next().unwrap_or_default();
                        this.rows_on_page = r.rows.len();
                        this.table.update(cx, |t, cx| {
                            t.delegate_mut().set(r.columns, r.rows);
                            t.refresh(cx);
                        });
                    }
                    Err(e) => {
                        this.error = Some(e.to_string());
                        this.table.update(cx, |t, cx| {
                            t.delegate_mut().set_loading(false);
                            t.refresh(cx);
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn go(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = self.page as isize + delta;
        if next < 0 {
            return;
        }
        self.page = next as usize;
        self.load(cx);
    }

}

impl Render for DataTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let first = self.page * PAGE + 1;
        let last = self.page * PAGE + self.rows_on_page;
        let estimate = self
            .object
            .rows_estimate
            .map(|n| format!(" of ~{n}"))
            .unwrap_or_default();
        let body = match &self.error {
            Some(e) => div()
                .p_3()
                .m_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.danger.opacity(0.5))
                .bg(theme.danger.opacity(0.08))
                .text_color(theme.danger)
                .font_family(theme.mono_font_family.clone())
                .text_size(super::rem(12.))
                .child(e.clone())
                .into_any_element(),
            None => div()
                .size_full()
                .key_context("ResultGrid")
                .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                    grid::copy_selection(&this.table, cx);
                }))
                .child(DataTable::new(&self.table)
                        .with_size(gpui_kit::component::Size::Size(super::scale(cx, 30.)))
                        .stripe(true))
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.title_bar)
                    .child(
                        div()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child("WHERE"),
                    )
                    .child(div().flex_1().child(Input::new(&self.filter).small()))
                    .child(
                        Button::new("apply")
                            .small()
                            .icon(icon("list-filter"))
                            .label("Filter")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = 0;
                                this.load(cx);
                            })),
                    )
                    .child(
                        Button::new("reload")
                            .small()
                            .ghost()
                            .icon(icon("refresh-cw"))
                            .tooltip("Reload")
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if self.rows_on_page == 0 {
                        "No rows".to_string()
                    } else {
                        format!("Rows {first}–{last}{estimate} · {:.0} ms", self.elapsed_ms)
                    })
                    .children(self.sort.as_ref().map(|(c, asc)| {
                        format!("· sorted by {c} {}", if *asc { "↑" } else { "↓" })
                    }))
                    .child(div().flex_1())
                    .child(
                        Button::new("prev")
                            .xsmall()
                            .ghost()
                            .icon(icon("chevron-left"))
                            .disabled(self.page == 0 || self.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.go(-1, cx))),
                    )
                    .child(format!("Page {}", self.page + 1))
                    .child(
                        Button::new("next")
                            .xsmall()
                            .ghost()
                            .icon(icon("chevron-right"))
                            .disabled(self.rows_on_page < PAGE || self.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.go(1, cx))),
                    ),
            )
    }
}
