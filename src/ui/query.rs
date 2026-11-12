//! A query tab: a SQL editor over a results grid.

use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState},
    resizable::{resizable_panel, v_resizable},
    spinner::Spinner,
    table::{DataTable, TableState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use tokio_postgres::CancelToken;

use super::grid::{self, ResultGrid};
use super::{AskAiInEditor, CopySelection, RunQuery, ToggleHistory, icon};
use crate::db::{ResultSet, Session};
use crate::rt;

/// Rows kept per statement.
const MAX_ROWS: usize = 20_000;

pub enum QueryEvent {
    /// Send this prompt to the Ask AI panel.
    AskAi(String),
    /// Focus the Ask AI panel's input.
    FocusAi,
    /// The status bar should show this.
    Status(String),
    /// A statement finished; remember it in the history.
    Ran(crate::history::Entry),
}

pub struct QueryTab {
    pub title: SharedString,
    pub session: Arc<Session>,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultGrid>>,
    results: Vec<ResultSet>,
    shown: usize,
    running: bool,
    cancel: Option<CancelToken>,
    error: Option<String>,
    summary: Option<String>,
    last_sql: String,
    focus: FocusHandle,
}

impl EventEmitter<QueryEvent> for QueryTab {}

impl Focusable for QueryTab {
    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl QueryTab {
    pub fn new(
        title: impl Into<SharedString>,
        session: Arc<Session>,
        sql: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(sql.to_string())
        });
        let table = cx.new(|cx| {
            let mut t = TableState::new(ResultGrid::new(false), window, cx);
            t.cell_selectable = true;
            t
        });
        QueryTab {
            title: title.into(),
            session,
            editor,
            table,
            results: vec![],
            shown: 0,
            running: false,
            cancel: None,
            error: None,
            summary: None,
            last_sql: String::new(),
            focus: cx.focus_handle(),
        }
    }

    pub fn sql(&self, cx: &gpui_kit::App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// The selected text if any, else the whole editor.
    fn sql_to_run(&self, cx: &gpui_kit::App) -> String {
        let e = self.editor.read(cx);
        let sel = e.selected_text().to_string();
        if sel.trim().is_empty() {
            e.value().to_string()
        } else {
            sel
        }
    }

    pub fn editor_focus(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Put `sql` in the editor (below what's there) for the user to run.
    pub fn insert_sql(&mut self, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        let cur = self.sql(cx);
        let next = if cur.trim().is_empty() {
            sql.to_string()
        } else {
            format!("{}\n\n{sql}", cur.trim_end())
        };
        self.editor.update(cx, |e, cx| e.set_value(next, window, cx));
        cx.notify();
    }

    pub fn set_sql(&mut self, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |e, cx| e.set_value(sql.to_string(), window, cx));
    }

    pub fn run(&mut self, cx: &mut Context<Self>) {
        let sql = self.sql_to_run(cx);
        self.run_sql(sql, true, cx);
    }

    /// `record`: keep it in the query history.
    fn run_sql(&mut self, sql: String, record: bool, cx: &mut Context<Self>) {
        if sql.trim().is_empty() || self.running {
            return;
        }
        self.running = true;
        self.error = None;
        self.summary = None;
        self.last_sql = sql.clone();
        self.cancel = Some(self.session.cancel_token());
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_loading(true);
            t.refresh(cx);
        });
        let session = self.session.clone();
        let started = std::time::Instant::now();
        let mut entry = record.then(|| crate::history::Entry {
            at: chrono::Local::now().to_rfc3339(),
            connection: session.profile.name.clone(),
            database: session.profile.database.clone(),
            sql: sql.trim().to_string(),
            elapsed_ms: 0,
            rows: None,
            error: None,
        });
        let fut = rt::spawn(async move { session.run(&sql, MAX_ROWS).await });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                this.cancel = None;
                if let Some(mut e) = entry.take() {
                    e.elapsed_ms = started.elapsed().as_millis() as u64;
                    match &out {
                        Ok(o) => e.rows = Some(o.results.iter().map(|r| r.row_count).sum()),
                        Err(err) => e.error = Some(err.to_string()),
                    }
                    cx.emit(QueryEvent::Ran(e));
                }
                match out {
                    Ok(out) => {
                        let ms = out.elapsed.as_secs_f64() * 1000.;
                        this.shown = out
                            .results
                            .iter()
                            .rposition(ResultSet::has_rows)
                            .unwrap_or(out.results.len().saturating_sub(1));
                        let summary = match out.results.get(this.shown) {
                            Some(r) if r.has_rows() => format!(
                                "{} row{}{} · {ms:.0} ms",
                                r.row_count,
                                if r.row_count == 1 { "" } else { "s" },
                                if r.truncated {
                                    format!(" (showing first {})", r.rows.len())
                                } else {
                                    String::new()
                                }
                            ),
                            Some(r) => format!("OK, {} row(s) affected · {ms:.0} ms", r.row_count),
                            None => format!("OK · {ms:.0} ms"),
                        };
                        this.results = out.results;
                        cx.emit(QueryEvent::Status(summary.clone()));
                        this.summary = Some(summary);
                    }
                    Err(e) => {
                        this.results.clear();
                        this.error = Some(e.to_string());
                        cx.emit(QueryEvent::Status("Query failed".into()));
                    }
                }
                this.show_result(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn show_result(&mut self, cx: &mut Context<Self>) {
        let r = self.results.get(self.shown).cloned().unwrap_or_default();
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set(r.columns, r.rows);
            t.refresh(cx);
        });
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(token) = self.cancel.take() {
            let _ = rt::spawn(async move {
                token
                    .cancel_query(tokio_postgres::NoTls)
                    .await
                    .map_err(anyhow::Error::from)
            });
            cx.emit(QueryEvent::Status("Cancelling…".into()));
        }
    }

    fn explain(&mut self, cx: &mut Context<Self>) {
        let sql = self.sql_to_run(cx);
        let sql = sql.trim().trim_end_matches(';');
        if sql.is_empty() {
            return;
        }
        self.run_sql(format!("EXPLAIN {sql}"), false, cx);
    }

    fn ask_ai(&mut self, what: AiAsk, cx: &mut Context<Self>) {
        let sql = self.sql_to_run(cx);
        let prompt = match what {
            AiAsk::Explain => format!("Explain what this query does, in plain words:\n```sql\n{}\n```", sql.trim()),
            AiAsk::Optimize => format!(
                "How can this query be made faster? Look at the plan with explain_query (analyze is fine, it runs read-only) and the indexes on the tables involved, then suggest concrete changes.\n```sql\n{}\n```",
                sql.trim()
            ),
            AiAsk::Fix => format!(
                "This query failed. Look at the schema and give me a corrected version.\n```sql\n{}\n```\nError:\n```\n{}\n```",
                self.last_sql.trim(),
                self.error.clone().unwrap_or_default()
            ),
        };
        cx.emit(QueryEvent::AskAi(prompt));
    }


    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_error = self.error.is_some();
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().title_bar)
            .child(
                Button::new("run")
                    .small()
                    .primary()
                    .icon(icon("play"))
                    .label("Run")
                    .tooltip("Run the selection, or everything (Ctrl+Enter)")
                    .disabled(self.running)
                    .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
            )
            .child(
                Button::new("stop")
                    .small()
                    .ghost()
                    .icon(icon("square"))
                    .label("Stop")
                    .disabled(!self.running)
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
            )
            .child(
                Button::new("explain")
                    .small()
                    .ghost()
                    .icon(icon("list-tree"))
                    .label("Plan")
                    .tooltip("Show PostgreSQL's plan (EXPLAIN)")
                    .disabled(self.running)
                    .on_click(cx.listener(|this, _, _, cx| this.explain(cx))),
            )
            .child(div().w(px(1.)).h(px(18.)).mx_1().bg(cx.theme().border))
            .child(
                Button::new("ai-explain")
                    .small()
                    .ghost()
                    .icon(icon("sparkles"))
                    .label("Explain")
                    .tooltip("Ask AI what this query does")
                    .on_click(cx.listener(|this, _, _, cx| this.ask_ai(AiAsk::Explain, cx))),
            )
            .child(
                Button::new("ai-optimize")
                    .small()
                    .ghost()
                    .icon(icon("zap"))
                    .label("Optimize")
                    .tooltip("Ask AI how to make this query faster")
                    .on_click(cx.listener(|this, _, _, cx| this.ask_ai(AiAsk::Optimize, cx))),
            )
            .when(has_error, |d| {
                d.child(
                    Button::new("ai-fix")
                        .small()
                        .warning()
                        .icon(icon("wand-sparkles"))
                        .label("Fix with AI")
                        .on_click(cx.listener(|this, _, _, cx| this.ask_ai(AiAsk::Fix, cx))),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("history")
                    .small()
                    .ghost()
                    .icon(icon("history"))
                    .label("History")
                    .tooltip("Queries you ran before (Ctrl+H)")
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleHistory), cx)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} · {}",
                        self.session.profile.name, self.session.profile.database
                    )),
            )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tabs = (self.results.len() > 1).then(|| {
            h_flex().gap_1().px_2().py_1().children(
                self.results.iter().enumerate().map(|(i, r)| {
                    let label = if r.has_rows() {
                        format!("Result {} ({})", i + 1, r.row_count)
                    } else {
                        format!("Statement {} ({} affected)", i + 1, r.row_count)
                    };
                    Button::new(("result", i))
                        .xsmall()
                        .when(i == self.shown, |b| b.primary())
                        .when(i != self.shown, |b| b.ghost())
                        .label(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.shown = i;
                            this.show_result(cx);
                            cx.notify();
                        }))
                }),
            )
        });
        let body = if let Some(e) = &self.error {
            div()
                .id("error")
                .size_full()
                .p_3()
                .overflow_y_scroll()
                .child(
                    div()
                        .p_3()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.danger.opacity(0.5))
                        .bg(theme.danger.opacity(0.08))
                        .text_color(theme.danger)
                        .font_family(theme.mono_font_family.clone())
                        .text_size(super::rem(12.))
                        .whitespace_normal()
                        .child(e.clone()),
                )
                .into_any_element()
        } else if self.running {
            h_flex()
                .size_full()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child("Running…")
                .into_any_element()
        } else if self.results.get(self.shown).is_some_and(|r| !r.has_rows()) {
            h_flex()
                .size_full()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(self.summary.clone().unwrap_or_default())
                .into_any_element()
        } else if self.results.is_empty() {
            v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_1()
                .text_color(theme.muted_foreground)
                .child("Write SQL above and press Ctrl+Enter")
                .child(div().text_xs().child("Ctrl+K asks AI to write it for you"))
                .into_any_element()
        } else {
            div()
                .size_full()
                .key_context("ResultGrid")
                .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                    grid::copy_selection(&this.table, cx);
                }))
                .child(DataTable::new(&self.table)
                        .with_size(gpui_kit::component::Size::Size(super::scale(cx, 30.)))
                        .stripe(true))
                .into_any_element()
        };
        let has_rows = self.results.get(self.shown).is_some_and(|r| r.has_rows());
        v_flex()
            .size_full()
            .children(tabs)
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
                    .child(self.summary.clone().unwrap_or_default())
                    .child(div().flex_1())
                    .when(has_rows, |d| {
                        d.child(
                            Button::new("copy-tsv")
                                .xsmall()
                                .ghost()
                                .icon(icon("copy"))
                                .label("Copy")
                                .tooltip("Copy all rows as tab-separated text")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let t = this.table.read(cx).delegate().all_tsv();
                                    cx.write_to_clipboard(ClipboardItem::new_string(t));
                                    cx.emit(QueryEvent::Status("Copied".into()));
                                })),
                        )
                        .child(
                            Button::new("export-csv")
                                .xsmall()
                                .ghost()
                                .icon(icon("download"))
                                .label("CSV")
                                .tooltip("Save the rows as a CSV file")
                                .on_click(cx.listener(|this, _, _, cx| this.export_csv(cx))),
                        )
                    }),
            )
    }

    fn export_csv(&mut self, cx: &mut Context<Self>) {
        let csv = self.table.read(cx).delegate().all_csv();
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        let name = format!(
            "{}-{}.csv",
            self.title.replace(|c: char| !c.is_alphanumeric(), "_"),
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let path = dir.join(name);
        let msg = match std::fs::write(&path, csv) {
            Ok(()) => format!("Saved {}", path.display()),
            Err(e) => format!("Couldn't save CSV: {e}"),
        };
        cx.emit(QueryEvent::Status(msg));
    }
}

#[derive(Clone, Copy)]
enum AiAsk {
    Explain,
    Optimize,
    Fix,
}

impl Render for QueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context("QueryTab")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run(cx)))
            .on_action(cx.listener(|_, _: &AskAiInEditor, _, cx| cx.emit(QueryEvent::FocusAi)))
            .child(self.render_toolbar(cx))
            .child(
                v_resizable(SharedString::from(format!("query-split-{}", cx.entity_id())))
                    .child(
                        resizable_panel()
                            .size(px(260.))
                            .size_range(px(80.)..px(2000.))
                            .child(Editor::new(&self.editor).h_full().bordered(false)),
                    )
                    .child(resizable_panel().child(self.render_results(cx))),
            )
    }
}

