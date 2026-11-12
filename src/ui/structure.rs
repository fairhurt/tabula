//! A Structure tab: columns, indexes, constraints and the CREATE statement.

use std::sync::Arc;

use gpui_kit::component::{
    StyledExt as _,
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div,
};

use super::icon;
use super::query::QueryEvent;
use crate::db::{DbObject, ObjectKind, Session, TableInfo};
use crate::rt;

pub struct StructureTab {
    pub title: SharedString,
    pub object: DbObject,
    info: Option<TableInfo>,
    error: Option<String>,
    ddl: Entity<EditorState>,
}

impl EventEmitter<QueryEvent> for StructureTab {}

impl StructureTab {
    pub fn new(
        session: Arc<Session>,
        object: DbObject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ddl = cx.new(|cx| EditorState::new(window, cx).language("sql").line_number(false));
        let o = object.clone();
        let fut = rt::spawn(async move {
            if o.kind == ObjectKind::Function {
                let ddl = session
                    .function_ddl(&o.schema, &o.name, o.signature.as_deref().unwrap_or(""))
                    .await?;
                Ok((None, ddl))
            } else {
                let info = session.table_info(&o.schema, &o.name).await?;
                let ddl = info.ddl.clone();
                Ok((Some(info), ddl))
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let out = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match out {
                    Ok((info, ddl)) => {
                        this.info = info;
                        this.ddl.update(cx, |e, cx| e.set_value(ddl, window, cx));
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        StructureTab {
            title: format!("{} (structure)", object.name).into(),
            object,
            info: None,
            error: None,
            ddl,
        }
    }

    fn section(&self, title: &str, cx: &Context<Self>) -> impl IntoElement {
        div()
            .pt_4()
            .pb_1()
            .text_sm()
            .font_semibold()
            .text_color(cx.theme().foreground)
            .child(title.to_string())
    }

    fn grid(
        &self,
        id: &'static str,
        headers: &[(&str, f32)],
        rows: Vec<Vec<(String, Option<Hsla>)>>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        // Every column has a fixed width but the last, which takes the room
        // that is left and wraps, so long definitions stay readable in a
        // narrow window.
        let last = headers.len().saturating_sub(1);
        let cell = |ix: usize, text: String, w: f32, color: Option<Hsla>| {
            div()
                .px_2()
                .py_1()
                .overflow_hidden()
                .map(|d| {
                    if ix == last {
                        d.flex_1().min_w(super::rem(120.)).whitespace_normal()
                    } else {
                        d.w(super::rem(w)).flex_shrink_0().text_ellipsis().whitespace_nowrap()
                    }
                })
                .when_some(color, |d, c| d.text_color(c))
                .child(text)
        };
        v_flex()
            .id(id)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius)
            .overflow_hidden()
            // The page scrolls; a grid must keep its full height, not be
            // squeezed to fit.
            .flex_shrink_0()
            .child(
                h_flex()
                    .bg(theme.table_head)
                    .text_color(theme.table_head_foreground)
                    .text_xs()
                    .font_semibold()
                    .children(headers.iter().enumerate().map(|(i, (h, w))| cell(i, h.to_string(), *w, None))),
            )
            .children(rows.into_iter().enumerate().map(|(i, r)| {
                h_flex()
                    .items_start()
                    .border_t_1()
                    .border_color(theme.table_row_border)
                    .when(i % 2 == 1, |d| d.bg(theme.table_even))
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .children(
                        r.into_iter()
                            .zip(headers.iter())
                            .enumerate()
                            .map(|(i, ((t, c), (_, w)))| cell(i, t, *w, c)),
                    )
            }))
            .into_any_element()
    }
}

impl Render for StructureTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = Some(theme.muted_foreground);
        let mut body = v_flex().id("structure").size_full().p_4().overflow_y_scroll();
        if let Some(e) = &self.error {
            return body.child(div().text_color(theme.danger).child(e.clone()));
        }
        let o = &self.object;
        body = body.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(icon(kind_icon(o.kind)).text_color(theme.primary))
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .child(format!("{}.{}", o.schema, o.name)),
                )
                .child(
                    div()
                        .text_xs()
                        .px_2()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_color(theme.muted_foreground)
                        .child(format!("{:?}", o.kind).to_lowercase()),
                ),
        );
        if let Some(info) = &self.info {
            let facts = [
                info.rows_estimate.map(|n| format!("~{n} rows")),
                info.total_size.clone(),
                Some(format!("{} columns", info.columns.len())),
            ];
            body = body.child(
                div()
                    .pt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(facts.into_iter().flatten().collect::<Vec<_>>().join(" · ")),
            );
            if let Some(c) = &info.comment {
                body = body.child(div().pt_2().text_sm().child(c.clone()));
            }
            body = body.child(self.section("Columns", cx)).child(
                self.grid(
                    "columns",
                    &[("", 28.), ("Name", 180.), ("Type", 180.), ("Null", 50.), ("Default", 200.), ("Comment", 260.)],
                    info.columns
                        .iter()
                        .map(|c| {
                            vec![
                                (if c.primary_key { "PK".into() } else { String::new() }, Some(theme.warning)),
                                (c.name.clone(), None),
                                (c.data_type.clone(), Some(theme.primary)),
                                (if c.nullable { "yes".into() } else { "no".into() }, muted),
                                (c.default.clone().unwrap_or_default(), muted),
                                (c.comment.clone().unwrap_or_default(), muted),
                            ]
                        })
                        .collect(),
                    cx,
                ),
            );
            if !info.indexes.is_empty() {
                body = body.child(self.section("Indexes", cx)).child(self.grid(
                    "indexes",
                    &[("Name", 200.), ("Definition", 720.)],
                    info.indexes
                        .iter()
                        .map(|i| vec![(i.name.clone(), None), (i.definition.clone(), muted)])
                        .collect(),
                    cx,
                ));
            }
            let cons: Vec<_> = info.constraints.iter().filter(|k| k.kind != "n").collect();
            if !cons.is_empty() {
                body = body.child(self.section("Constraints", cx)).child(self.grid(
                    "constraints",
                    &[("Name", 200.), ("Kind", 100.), ("Definition", 620.)],
                    cons.iter()
                        .map(|k| {
                            let kind = match k.kind.as_str() {
                                "p" => "primary key",
                                "f" => "foreign key",
                                "u" => "unique",
                                "c" => "check",
                                "x" => "exclusion",
                                _ => "other",
                            };
                            vec![(k.name.clone(), None), (kind.into(), muted), (k.definition.clone(), muted)]
                        })
                        .collect(),
                    cx,
                ));
            }
        } else if self.object.kind != ObjectKind::Function {
            return body.child(h_flex().pt_4().gap_2().child(Spinner::new().small()).child("Loading…"));
        }
        body.child(
            h_flex()
                .pt_4()
                .pb_1()
                .child(div().flex_1().text_sm().font_semibold().child("Definition"))
                .child(
                    Button::new("copy-ddl")
                        .xsmall()
                        .ghost()
                        .icon(icon("copy"))
                        .label("Copy")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let t = this.ddl.read(cx).value().to_string();
                            cx.write_to_clipboard(ClipboardItem::new_string(t));
                            cx.emit(QueryEvent::Status("Copied the definition".into()));
                        })),
                ),
        )
        .child(
            div()
                .h(super::rem(320.))
                .flex_shrink_0()
                .border_1()
                .border_color(theme.border)
                .rounded(theme.radius)
                .child(Editor::new(&self.ddl).h_full().bordered(false).readonly(true)),
        )
    }
}

pub fn kind_icon(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "eye",
        ObjectKind::MaterializedView => "layers",
        ObjectKind::ForeignTable => "table-properties",
        ObjectKind::Function => "square-function",
        ObjectKind::Sequence => "hash",
    }
}
