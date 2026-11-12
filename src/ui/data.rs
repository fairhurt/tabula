//! A Data tab: browse a table or view a page at a time, with a WHERE filter
//! and server-side sorting. Tables with a primary key can be edited: changes
//! are held until you save, then applied in one transaction.

use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, InputEvent, InputState},
    table::{DataTable, TableEvent, TableState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div,
};

use super::grid::{self, GridRequest, ResultGrid};
use super::query::QueryEvent;
use super::{CopySelection, EditCell, SaveChanges, icon};
use crate::db::{self, DbObject, EditTarget, ObjectKind, Session};
use crate::rt;

const PAGE: usize = 500;

/// Whether the rows can be edited, and if not, why.
enum Editing {
    /// Still reading the table's columns.
    Unknown,
    On(EditTarget),
    Off(&'static str),
}

pub struct DataTab {
    pub title: SharedString,
    pub session: Arc<Session>,
    pub object: DbObject,
    /// A one-line SQL editor, so the filter gets highlighting and suggestions.
    filter: Entity<EditorState>,
    table: Entity<TableState<ResultGrid>>,
    /// The input a cell turns into while it's edited.
    cell_input: Entity<InputState>,
    editing: Editing,
    /// Primary key columns: the default order, so pages are stable.
    key: Vec<String>,
    page: usize,
    sort: Option<(String, bool)>,
    loading: bool,
    saving: bool,
    error: Option<String>,
    save_error: Option<String>,
    /// Show the SQL the pending changes will run.
    review: bool,
    /// A cell's whole value, shown over the grid.
    viewing: Option<grid::CellValue>,
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
            EditorState::new(window, cx)
                .language("sql")
                .soft_wrap(false)
                // Enter applies the filter instead of adding a line.
                .submit_on_enter(true)
                .placeholder("e.g. status = 'paid' AND placed_at > now() - interval '1 day'")
        });
        super::sql_complete::attach(&filter, &session, Some((object.schema.clone(), object.name.clone())), cx);
        let cell_input = cx.new(|cx| InputState::new(window, cx));
        let table = cx.new(|cx| {
            let mut t = TableState::new(ResultGrid::new(true), window, cx);
            t.cell_selectable = true;
            t
        });
        let subs = vec![
            cx.subscribe(&filter, |this, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) && this.can_leave_page(cx) {
                    this.page = 0;
                    this.load(cx);
                }
            }),
            // A header click asks for a server-side sort; a menu item asks
            // for an edit.
            cx.observe_in(&table, window, |this, table, window, cx| {
                let (sort, request) = table.update(cx, |t, _| {
                    let d = t.delegate_mut();
                    (d.sort_request.take(), d.request.take())
                });
                if let Some((col, asc)) = sort {
                    let name = table.read(cx).delegate().columns().get(col).map(|c| c.name.clone());
                    if let (Some(name), true) = (name, this.can_leave_page(cx)) {
                        this.sort = Some((name, asc));
                        this.page = 0;
                        this.load(cx);
                    }
                }
                match request {
                    Some(GridRequest::View(r, c)) => {
                        this.viewing = table.read(cx).delegate().cell_value(r, c);
                        cx.notify();
                    }
                    Some(GridRequest::Edit(r, c)) => this.start_edit(r, c, window, cx),
                    Some(GridRequest::SetNull(r, c)) => this.stage(cx, |g| g.set_cell(r, c, None)),
                    Some(GridRequest::ToggleDelete(r)) => this.stage(cx, |g| g.toggle_delete(r)),
                    Some(GridRequest::Duplicate(r)) => this.stage(cx, |g| {
                        g.duplicate_row(r);
                    }),
                    None => {}
                }
            }),
            cx.subscribe_in(&table, window, |this, _, e: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(r, c) = e {
                    this.start_edit(*r, *c, window, cx);
                }
            }),
            cx.subscribe(&cell_input, |this, _, e: &InputEvent, cx| match e {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.finish_edit(cx),
                _ => {}
            }),
        ];
        let mut tab = DataTab {
            title: format!("{}", object.name).into(),
            session,
            object,
            filter,
            table,
            cell_input,
            editing: Editing::Unknown,
            key: vec![],
            page: 0,
            sort: None,
            loading: false,
            saving: false,
            error: None,
            save_error: None,
            review: false,
            viewing: None,
            rows_on_page: 0,
            elapsed_ms: 0.,
            _subs: subs,
        };
        tab.open(cx);
        tab
    }

    /// Read the table's key and column types, then load the first page.
    fn open(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_loading(true);
            t.refresh(cx);
        });
        let session = self.session.clone();
        let (schema, name) = (self.object.schema.clone(), self.object.name.clone());
        let fut = rt::spawn(async move { session.table_info(&schema, &name).await });
        cx.spawn(async move |this, cx| {
            let info = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.editing = match info {
                    Err(_) => Editing::Off("Read-only: couldn't read this table's columns"),
                    Ok(_) if this.object.kind != ObjectKind::Table => Editing::Off("Read-only: not a table"),
                    Ok(info) => {
                        this.key = info.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
                        if this.key.is_empty() {
                            Editing::Off("Read-only: this table has no primary key")
                        } else {
                            let (key, input) = (this.key.clone(), this.cell_input.clone());
                            this.table.update(cx, |t, _| t.delegate_mut().enable_editing(key, input));
                            Editing::On(EditTarget::new(this.object.qualified(), &info.columns))
                        }
                    }
                };
                this.load(cx);
            });
        })
        .detach();
    }

    fn sql(&self, cx: &gpui_kit::App) -> String {
        // One line: a pasted line break would otherwise hide part of it.
        let filter = self.filter.read(cx).value().replace(['\n', '\r'], " ");
        db::browse_sql(
            &self.object,
            &filter,
            self.sort.as_ref().map(|(c, a)| (c.as_str(), *a)),
            &self.key,
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
        if next < 0 || !self.can_leave_page(cx) {
            return;
        }
        self.page = next as usize;
        self.load(cx);
    }

    /// Stage a few edits and show their SQL (headless screenshots).
    pub fn shot_edits(&mut self, cx: &mut Context<Self>) {
        let col = |this: &Self, name: &str, cx: &gpui_kit::App| {
            this.table.read(cx).delegate().columns().iter().position(|c| c.name == name)
        };
        let (Some(title), Some(price)) = (col(self, "title", cx), col(self, "price", cx)) else { return };
        self.review = true;
        self.stage(cx, |g| {
            g.set_cell(1, price, Some("24.50".into()));
            g.set_cell(3, title, Some("Walnut desk organiser".into()));
            g.toggle_delete(6);
            let new = g.add_row();
            g.set_cell(new, title, Some("Brass pen tray".into()));
            g.set_cell(new, price, Some("18.00".into()));
        });
    }

    // ---------------------------------------------------------------- edits

    fn pending(&self, cx: &gpui_kit::App) -> usize {
        self.table.read(cx).delegate().changes().len()
    }

    /// Loading other rows would lose unsaved edits: say so instead.
    fn can_leave_page(&mut self, cx: &mut Context<Self>) -> bool {
        let n = self.pending(cx);
        if n > 0 {
            cx.emit(QueryEvent::Status(format!(
                "Save or discard your {n} unsaved change{} first",
                if n == 1 { "" } else { "s" }
            )));
        }
        n == 0
    }

    /// Change the staged edits and redraw.
    fn stage(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut ResultGrid)) {
        if !matches!(self.editing, Editing::On(_)) {
            return;
        }
        self.save_error = None;
        self.table.update(cx, |t, cx| {
            f(t.delegate_mut());
            t.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn start_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.editing, Editing::On(_)) || self.saving {
            if let Editing::Off(why) = self.editing {
                cx.emit(QueryEvent::Status(why.into()));
            }
            return;
        }
        self.finish_edit(cx);
        let grid = self.table.read(cx).delegate();
        if grid.is_deleted(row) {
            return;
        }
        let value = grid.shown(row, col).unwrap_or("").to_string();
        self.cell_input.update(cx, |i, cx| {
            i.set_value(value, window, cx);
            i.focus(window, cx);
        });
        self.table.update(cx, |t, cx| {
            t.delegate_mut().editing = Some((row, col));
            cx.notify();
        });
    }

    /// Stage what was typed into the cell being edited.
    fn finish_edit(&mut self, cx: &mut Context<Self>) {
        let Some((row, col)) = self.table.read(cx).delegate().editing else { return };
        let text = self.cell_input.read(cx).value().to_string();
        let Editing::On(target) = &self.editing else { return };
        // Nothing typed means NULL, except where an empty string is a value.
        let grid = self.table.read(cx).delegate();
        let texty = grid
            .columns()
            .get(col)
            .and_then(|c| target.types.get(&c.name))
            .is_some_and(|t| t == "text" || t.starts_with("character") || t == "citext");
        let was_null = grid.shown(row, col).is_none();
        let value = if text.is_empty() && (!texty || was_null) { None } else { Some(text) };
        self.save_error = None;
        self.table.update(cx, |t, cx| {
            let g = t.delegate_mut();
            g.editing = None;
            g.set_cell(row, col, value);
            cx.notify();
        });
        cx.notify();
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.editing, Editing::On(_)) {
            return;
        }
        self.finish_edit(cx);
        let row = self.table.update(cx, |t, cx| {
            let row = t.delegate_mut().add_row();
            t.refresh(cx);
            t.scroll_to_row(row, cx);
            row
        });
        // Start typing in the first column that isn't the key.
        let col = {
            let grid = self.table.read(cx).delegate();
            (0..grid.columns().len()).find(|c| !self.key.contains(&grid.columns()[*c].name)).unwrap_or(0)
        };
        self.start_edit(row, col, window, cx);
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let row = {
            let t = self.table.read(cx);
            t.selected_cell().map(|(r, _)| r).or(t.selected_row())
        };
        match row {
            Some(r) => self.stage(cx, |g| g.toggle_delete(r)),
            None => cx.emit(QueryEvent::Status("Click a row first".into())),
        }
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        self.save_error = None;
        self.review = false;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().discard();
            t.refresh(cx);
        });
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        self.finish_edit(cx);
        let Editing::On(target) = &self.editing else { return };
        let changes = self.table.read(cx).delegate().changes();
        if changes.is_empty() || self.saving {
            return;
        }
        self.saving = true;
        self.save_error = None;
        let (session, target) = (self.session.clone(), target.clone());
        let fut = rt::spawn(async move { session.apply(&target, &changes).await });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                match out {
                    Ok(n) => {
                        this.review = false;
                        cx.emit(QueryEvent::Status(format!(
                            "Saved: {n} row{} changed in {}",
                            if n == 1 { "" } else { "s" },
                            this.object.name
                        )));
                        this.load(cx);
                    }
                    // The edits stay, so they can be fixed and saved again.
                    Err(e) => {
                        this.save_error = Some(e.to_string());
                        cx.emit(QueryEvent::Status("Couldn't save; nothing was changed".into()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The bar that appears once there are unsaved changes.
    fn render_pending(&self, pending: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let preview = match (&self.editing, self.review) {
            (Editing::On(target), true) => Some(
                self.table
                    .read(cx)
                    .delegate()
                    .changes()
                    .iter()
                    .map(|c| target.preview(c))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        };
        v_flex()
            .border_t_1()
            .border_color(theme.warning.opacity(0.5))
            .bg(theme.warning.opacity(0.08))
            .children(self.save_error.clone().map(|e| {
                div()
                    .px_3()
                    .py_2()
                    .text_color(theme.danger)
                    .font_family(theme.mono_font_family.clone())
                    .text_size(super::rem(12.))
                    .child(e)
            }))
            .children(preview.map(|sql| {
                div()
                    .id("pending-sql")
                    .max_h(super::rem(180.))
                    .overflow_y_scroll()
                    .px_3()
                    .py_2()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(super::rem(12.))
                    .child(sql)
            }))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .child(icon("pencil-line").small().text_color(theme.warning))
                    .child(div().flex_1().text_sm().child(format!(
                        "{pending} unsaved change{} · saved together, or not at all",
                        if pending == 1 { "" } else { "s" }
                    )))
                    .child(
                        Button::new("review")
                            .small()
                            .ghost()
                            .icon(icon("code"))
                            .label(if self.review { "Hide SQL" } else { "Review SQL" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.review = !this.review;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("discard")
                            .small()
                            .outline()
                            .label("Discard")
                            .disabled(self.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.discard(cx))),
                    )
                    .child(
                        Button::new("save")
                            .small()
                            .primary()
                            .icon(icon("save"))
                            .label("Save")
                            .tooltip("Apply the changes (Ctrl+S)")
                            .loading(self.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
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
        let pending = self.pending(cx);
        let editable = matches!(self.editing, Editing::On(_));
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
                .child(
                    DataTable::new(&self.table)
                        .with_size(gpui_kit::component::Size::Size(super::scale(cx, 30.)))
                        .stripe(true),
                )
                .into_any_element(),
        };
        let viewer = self.viewing.as_ref().map(|v| {
            grid::value_viewer(
                v,
                cx.listener(|this, _, _, cx| {
                    this.viewing = None;
                    cx.notify();
                }),
                cx,
            )
        });
        v_flex()
            .size_full()
            .relative()
            .children(viewer)
            .key_context("DataTab")
            .on_action(cx.listener(|this, _: &SaveChanges, _, cx| this.save(cx)))
            .on_action(cx.listener(|this, _: &EditCell, window, cx| {
                if let Some((r, c)) = this.table.read(cx).selected_cell() {
                    this.start_edit(r, c, window, cx);
                }
            }))
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
                    .child(
                        div()
                            .flex_1()
                            .h(super::rem(30.))
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.input)
                            .bg(theme.background)
                            .overflow_hidden()
                            .child(Editor::new(&self.filter).h_full().bordered(false)),
                    )
                    .child(
                        Button::new("apply")
                            .small()
                            .icon(icon("list-filter"))
                            .label("Filter")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.can_leave_page(cx) {
                                    this.page = 0;
                                    this.load(cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("reload")
                            .small()
                            .ghost()
                            .icon(icon("refresh-cw"))
                            .tooltip("Reload")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.can_leave_page(cx) {
                                    this.load(cx);
                                }
                            })),
                    )
                    .when(editable, |d| {
                        d.child(div().w(gpui_kit::px(1.)).h(super::rem(18.)).bg(theme.border))
                            .child(
                                Button::new("add-row")
                                    .small()
                                    .ghost()
                                    .icon(icon("plus"))
                                    .label("Row")
                                    .tooltip("Add a row")
                                    .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                            )
                            .child(
                                Button::new("delete-row")
                                    .small()
                                    .ghost()
                                    .icon(icon("trash-2"))
                                    .tooltip("Delete the selected row (again to keep it)")
                                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))),
                            )
                    }),
            )
            .child(div().flex_1().min_h_0().child(body))
            .when(pending > 0 || self.save_error.is_some(), |d| d.child(self.render_pending(pending, cx)))
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
                    .child(match &self.editing {
                        Editing::On(_) => "· double-click a cell to edit".to_string(),
                        Editing::Off(why) => format!("· {why}"),
                        Editing::Unknown => String::new(),
                    })
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
