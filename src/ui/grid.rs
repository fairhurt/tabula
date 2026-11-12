//! The results grid shared by the Query and Data tabs: virtualized rows,
//! resizable columns, sorting, NULLs drawn as NULL, copy as TSV. In the
//! Data tab it also holds edits that haven't been saved yet.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    input::{Escape, Input, InputState},
    menu::{PopupMenu, PopupMenuItem},
    table::{Column, ColumnSort, TableDelegate, TableSelection, TableState},
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClipboardItem, Context, Div, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Stateful, Styled as _, Window, div,
};

use super::icon;
use crate::db::{self, Change, ResultColumn};

/// Something the user asked for from a cell's right-click menu; the Data tab
/// picks it up. Rows and columns are as displayed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridRequest {
    /// Show the whole value of a cell.
    View(usize, usize),
    Edit(usize, usize),
    SetNull(usize, usize),
    ToggleDelete(usize),
    Duplicate(usize),
}

/// Unsaved edits to the rows on screen.
struct Edits {
    /// Primary key columns, by name; resolved to indexes when rows load.
    key_names: Vec<String>,
    key: Vec<usize>,
    /// New values by (row, column), rows as stored (not as displayed).
    edited: HashMap<(usize, usize), Option<String>>,
    deleted: HashSet<usize>,
    /// Rows that came from the server; any after them are new.
    loaded: usize,
    /// The input a cell turns into while it's edited.
    editor: Entity<InputState>,
}

#[derive(Clone, Copy, PartialEq)]
enum RowState {
    Clean,
    New,
    Deleted,
}

/// Longest text drawn in a cell; the whole value is still copied.
const CELL_CHARS: usize = 300;

pub struct ResultGrid {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Option<String>>>,
    /// Display order (indexes into `rows`), for client-side sorting.
    order: Vec<usize>,
    /// Sort on the server instead: the Data tab picks this up.
    server_sort: bool,
    pub sort_request: Option<(usize, bool)>,
    loading: bool,
    edits: Option<Edits>,
    /// The cell being edited, as displayed.
    pub editing: Option<(usize, usize)>,
    pub request: Option<GridRequest>,
}

impl ResultGrid {
    pub fn new(server_sort: bool) -> Self {
        ResultGrid {
            columns: vec![],
            rows: vec![],
            order: vec![],
            server_sort,
            sort_request: None,
            loading: false,
            edits: None,
            editing: None,
            request: None,
        }
    }

    pub fn set(&mut self, columns: Vec<ResultColumn>, rows: Vec<Vec<Option<String>>>) {
        self.order = (0..rows.len()).collect();
        self.columns = columns;
        self.rows = rows;
        self.loading = false;
        self.editing = None;
        if let Some(e) = &mut self.edits {
            e.edited.clear();
            e.deleted.clear();
            e.loaded = self.rows.len();
            e.key = e
                .key_names
                .iter()
                .filter_map(|k| self.columns.iter().position(|c| &c.name == k))
                .collect();
        }
    }

    // ---------------------------------------------------------------- edits

    /// Let rows be edited; `key` is the table's primary key.
    pub fn enable_editing(&mut self, key: Vec<String>, editor: Entity<InputState>) {
        self.edits = Some(Edits {
            key: key.iter().filter_map(|k| self.columns.iter().position(|c| &c.name == k)).collect(),
            key_names: key,
            edited: HashMap::new(),
            deleted: HashSet::new(),
            loaded: self.rows.len(),
            editor,
        });
    }

    /// Rows can be edited: there is a primary key and every part of it is
    /// among the columns.
    pub fn editable(&self) -> bool {
        self.edits.as_ref().is_some_and(|e| !e.key_names.is_empty() && e.key.len() == e.key_names.len())
    }

    fn row_state(&self, row_ix: usize) -> RowState {
        match (&self.edits, self.order.get(row_ix)) {
            (Some(e), Some(r)) if e.deleted.contains(r) => RowState::Deleted,
            (Some(e), Some(r)) if *r >= e.loaded => RowState::New,
            _ => RowState::Clean,
        }
    }

    fn is_edited(&self, row_ix: usize, col_ix: usize) -> bool {
        match (&self.edits, self.order.get(row_ix)) {
            (Some(e), Some(r)) => e.edited.contains_key(&(*r, col_ix)),
            _ => false,
        }
    }

    /// The value shown in a cell, with unsaved edits applied.
    pub fn shown(&self, row_ix: usize, col_ix: usize) -> Option<&str> {
        self.value(row_ix, col_ix)
    }

    /// Stage a new value for a cell. Putting the saved value back unstages it.
    pub fn set_cell(&mut self, row_ix: usize, col_ix: usize, value: Option<String>) {
        let Some(&r) = self.order.get(row_ix) else { return };
        let saved = self.rows.get(r).and_then(|row| row.get(col_ix)).cloned();
        let Some(e) = &mut self.edits else { return };
        if r < e.loaded && saved == Some(value.clone()) {
            e.edited.remove(&(r, col_ix));
        } else {
            e.edited.insert((r, col_ix), value);
        }
    }

    /// Add an empty row at the end; returns where it is displayed.
    pub fn add_row(&mut self) -> usize {
        self.rows.push(vec![None; self.columns.len()]);
        self.order.push(self.rows.len() - 1);
        self.order.len() - 1
    }

    /// Add a row with this row's values, leaving the key to its default.
    pub fn duplicate_row(&mut self, row_ix: usize) -> usize {
        let values: Vec<Option<String>> =
            (0..self.columns.len()).map(|c| self.value(row_ix, c).map(str::to_string)).collect();
        let new = self.add_row();
        let r = self.order[new];
        if let Some(e) = &mut self.edits {
            for (c, v) in values.into_iter().enumerate() {
                if !e.key.contains(&c) && v.is_some() {
                    e.edited.insert((r, c), v);
                }
            }
        }
        new
    }

    /// Mark a row for deletion, or take the mark off.
    pub fn toggle_delete(&mut self, row_ix: usize) {
        let Some(&r) = self.order.get(row_ix) else { return };
        if let Some(e) = &mut self.edits {
            if !e.deleted.remove(&r) {
                e.deleted.insert(r);
            }
        }
    }

    pub fn is_deleted(&self, row_ix: usize) -> bool {
        self.row_state(row_ix) == RowState::Deleted
    }

    /// Drop every unsaved edit.
    pub fn discard(&mut self) {
        self.editing = None;
        if let Some(e) = &mut self.edits {
            e.edited.clear();
            e.deleted.clear();
            self.rows.truncate(e.loaded);
            self.order.retain(|r| *r < e.loaded);
        }
    }

    /// The unsaved edits as changes to apply: deletes, then updates, then
    /// new rows.
    pub fn changes(&self) -> Vec<Change> {
        let Some(e) = &self.edits else { return vec![] };
        let name = |c: usize| self.columns[c].name.clone();
        let key = |r: usize| -> Vec<db::Cell> { e.key.iter().map(|&c| (name(c), self.rows[r][c].clone())).collect() };
        let set = |r: usize| -> Vec<db::Cell> {
            let mut cells: Vec<(usize, Option<String>)> =
                e.edited.iter().filter(|((row, _), _)| *row == r).map(|((_, c), v)| (*c, v.clone())).collect();
            cells.sort_by_key(|(c, _)| *c);
            cells.into_iter().map(|(c, v)| (name(c), v)).collect()
        };
        let mut out = vec![];
        let mut deleted: Vec<usize> = e.deleted.iter().copied().filter(|r| *r < e.loaded).collect();
        deleted.sort();
        out.extend(deleted.into_iter().map(|r| Change::Delete { key: key(r) }));
        for r in 0..e.loaded {
            if !e.deleted.contains(&r) {
                let set = set(r);
                if !set.is_empty() {
                    out.push(Change::Update { key: key(r), set });
                }
            }
        }
        for r in e.loaded..self.rows.len() {
            if !e.deleted.contains(&r) {
                out.push(Change::Insert { values: set(r) });
            }
        }
        out
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    pub fn columns(&self) -> &[ResultColumn] {
        &self.columns
    }


    fn value(&self, row_ix: usize, col_ix: usize) -> Option<&str> {
        let r = *self.order.get(row_ix)?;
        if let Some(v) = self.edits.as_ref().and_then(|e| e.edited.get(&(r, col_ix))) {
            return v.as_deref();
        }
        self.rows.get(r)?.get(col_ix)?.as_deref()
    }

    /// The selection (or everything) as TSV.
    pub fn copy_text(&self, selection: &TableSelection) -> String {
        match *selection {
            TableSelection::Cell(r, c) => self.value(r, c).unwrap_or("").to_string(),
            TableSelection::Row(r) => {
                let row: Vec<_> = self.order.get(r).map(|&i| self.rows[i].clone()).into_iter().collect();
                db::to_tsv(&self.columns, &row)
            }
            TableSelection::Column(c) => (0..self.order.len())
                .map(|r| self.value(r, c).unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            TableSelection::None => self.all_tsv(),
        }
    }

    pub fn all_tsv(&self) -> String {
        let rows: Vec<_> = self.order.iter().map(|&i| self.rows[i].clone()).collect();
        db::to_tsv(&self.columns, &rows)
    }

    pub fn all_csv(&self) -> String {
        let rows: Vec<_> = self.order.iter().map(|&i| self.rows[i].clone()).collect();
        db::to_csv(&self.columns, &rows)
    }

    /// Width in pixels at 13px text; the caller scales it.
    fn width_of(&self, col_ix: usize) -> f32 {
        let header = self.columns[col_ix].name.chars().count() + 2;
        let widest = self
            .rows
            .iter()
            .take(200)
            .map(|r| r[col_ix].as_deref().map_or(4, |v| v.chars().count().min(60)))
            .max()
            .unwrap_or(4);
        (header.max(widest) as f32 * 7.5 + 20.).clamp(64., 420.)
    }
}

/// A cell's whole value, opened from the grid to read or copy.
pub struct CellValue {
    pub column: String,
    pub type_name: Option<String>,
    /// `None` is NULL.
    pub text: Option<String>,
}

/// Most characters drawn in the viewer; the rest is still copied.
const VIEW_CHARS: usize = 200_000;

impl ResultGrid {
    /// The whole value of a cell, for the viewer.
    pub fn cell_value(&self, row_ix: usize, col_ix: usize) -> Option<CellValue> {
        let column = self.columns.get(col_ix)?;
        Some(CellValue {
            column: column.name.clone(),
            type_name: column.type_name.clone(),
            text: self.value(row_ix, col_ix).map(str::to_string),
        })
    }
}

impl CellValue {
    /// The value laid out for reading: JSON is indented.
    pub fn pretty(&self) -> Option<String> {
        let text = self.text.as_deref()?;
        let looks_like_json = matches!(self.type_name.as_deref(), Some("json" | "jsonb"))
            || matches!(text.trim_start().chars().next(), Some('{' | '['));
        if looks_like_json {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                if let Ok(p) = serde_json::to_string_pretty(&v) {
                    return Some(p);
                }
            }
        }
        Some(text.to_string())
    }
}

/// The panel that shows a cell's whole value over a tab. `close` is called
/// by its close button.
pub fn value_viewer(
    value: &CellValue,
    close: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    use gpui_kit::StatefulInteractiveElement as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
    let theme = cx.theme();
    let pretty = value.pretty();
    let chars = pretty.as_ref().map_or(0, |p| p.chars().count());
    let shown: Option<String> = pretty.as_ref().map(|p| p.chars().take(VIEW_CHARS).collect());
    let copy = pretty.clone().unwrap_or_default();
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::black().opacity(0.35))
        .occlude()
        .child(
            v_flex()
                .w(super::rem(720.))
                .max_w_full()
                .max_h(gpui_kit::relative(0.85))
                .m_4()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .rounded(theme.radius_lg)
                .shadow_lg()
                .overflow_hidden()
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().font_semibold().text_sm().child(value.column.clone()))
                        .children(value.type_name.clone().map(|t| {
                            div().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.primary).child(t)
                        }))
                        .child(div().flex_1().text_xs().text_color(theme.muted_foreground).child(match &pretty {
                            None => "NULL".to_string(),
                            Some(_) if chars > VIEW_CHARS => format!("{chars} characters, first {VIEW_CHARS} shown"),
                            Some(_) => format!("{chars} character{}", if chars == 1 { "" } else { "s" }),
                        }))
                        .child(
                            Button::new("value-copy")
                                .xsmall()
                                .ghost()
                                .icon(icon("copy"))
                                .label("Copy")
                                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                        )
                        .child(Button::new("value-close").xsmall().ghost().icon(icon("x")).on_click(close)),
                )
                .child(
                    div()
                        .id("value-text")
                        .flex_1()
                        .min_h(super::rem(80.))
                        .overflow_y_scroll()
                        .p_3()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(super::rem(12.5))
                        .map(|d| match shown {
                            Some(text) => d.whitespace_normal().child(text),
                            None => d.italic().text_color(theme.muted_foreground).child("NULL"),
                        }),
                ),
        )
}

/// Copy the grid's selection (or all of it) to the clipboard.
pub fn copy_selection(table: &gpui_kit::Entity<TableState<ResultGrid>>, cx: &mut App) {
    let t = table.read(cx);
    let text = t.delegate().copy_text(&t.selection());
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, col_ix: usize, cx: &App) -> Column {
        let c = &self.columns[col_ix];
        let mut col = Column::new(SharedString::from(format!("c{col_ix}")), c.name.clone())
            .width(super::scale(cx, self.width_of(col_ix)))
            .sortable();
        if c.is_numeric() {
            col = col.text_right();
        }
        col
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        if self.server_sort {
            self.sort_request = Some((col_ix, sort != ColumnSort::Descending));
            return;
        }
        let numeric = self.columns[col_ix].is_numeric();
        let rows = &self.rows;
        let key = |i: &usize| rows[*i][col_ix].clone();
        match sort {
            ColumnSort::Default => self.order = (0..rows.len()).collect(),
            ColumnSort::Ascending | ColumnSort::Descending => {
                self.order.sort_by(|a, b| {
                    let (x, y) = (key(a), key(b));
                    let o = match (&x, &y) {
                        (None, None) => std::cmp::Ordering::Equal,
                        (None, _) => std::cmp::Ordering::Greater,
                        (_, None) => std::cmp::Ordering::Less,
                        (Some(x), Some(y)) if numeric => x
                            .parse::<f64>()
                            .unwrap_or(f64::NAN)
                            .total_cmp(&y.parse::<f64>().unwrap_or(f64::NAN)),
                        (Some(x), Some(y)) => x.cmp(y),
                    };
                    if sort == ColumnSort::Descending { o.reverse() } else { o }
                });
            }
        }
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let row = div().id(("row", row_ix));
        match self.row_state(row_ix) {
            RowState::Clean => row,
            RowState::New => row.bg(cx.theme().success.opacity(0.12)),
            RowState::Deleted => row.bg(cx.theme().danger.opacity(0.12)),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let table = cx.entity().downgrade();
        let view = {
            let table = table.clone();
            PopupMenuItem::new("View value").icon(icon("maximize-2")).on_click(move |_, _, cx| {
                let _ = table.update(cx, |t, cx| {
                    let cell = t.right_clicked_cell().or(t.selected_cell()).filter(|(r, _)| *r == row_ix);
                    t.delegate_mut().request = cell.map(|(r, c)| GridRequest::View(r, c));
                    cx.notify();
                });
            })
        };
        if !self.editable() {
            return menu.item(view);
        }
        let menu = menu.item(view).separator();
        let deleted = self.is_deleted(row_ix);
        let item = |label: &str, ic: &str, make: fn(usize, Option<usize>) -> Option<GridRequest>| {
            let table = table.clone();
            PopupMenuItem::new(label.to_string()).icon(icon(ic)).on_click(move |_, _, cx| {
                let _ = table.update(cx, |t, cx| {
                    // The cell that was right-clicked, when it is on this row.
                    let col = t.right_clicked_cell().filter(|(r, _)| *r == row_ix).map(|(_, c)| c);
                    t.delegate_mut().request = make(row_ix, col);
                    cx.notify();
                });
            })
        };
        let mut menu = menu;
        if !deleted {
            menu = menu
                .item(item("Edit cell", "square-pen", |r, c| c.map(|c| GridRequest::Edit(r, c))))
                .item(item("Set to NULL", "circle-slash", |r, c| c.map(|c| GridRequest::SetNull(r, c))))
                .separator()
                .item(item("Duplicate row", "copy-plus", |r, _| Some(GridRequest::Duplicate(r))));
        }
        menu.item(item(
            if deleted { "Keep row" } else { "Delete row" },
            if deleted { "undo-2" } else { "trash-2" },
            |r, _| Some(GridRequest::ToggleDelete(r)),
        ))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if self.editing == Some((row_ix, col_ix)) {
            if let Some(e) = &self.edits {
                return div()
                    .w_full()
                    .on_action(cx.listener(|t, _: &Escape, _, cx| {
                        t.delegate_mut().editing = None;
                        cx.notify();
                    }))
                    .child(Input::new(&e.editor).xsmall().appearance(false))
                    .into_any_element();
            }
        }
        let theme = cx.theme();
        let numeric = self.columns.get(col_ix).is_some_and(|c| c.is_numeric());
        let state = self.row_state(row_ix);
        let edited = self.is_edited(row_ix, col_ix);
        let cell = div()
            .w_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .font_family(theme.mono_font_family.clone())
            .text_size(super::rem(12.))
            .when(numeric, |d| d.text_right())
            .when(state == RowState::Deleted, |d| d.line_through().text_color(theme.muted_foreground))
            .when(edited && state != RowState::Deleted, |d| {
                d.px_1().rounded_sm().bg(theme.warning.opacity(0.28))
            });
        match self.value(row_ix, col_ix) {
            // A new row's untouched cells take the column default.
            None if state == RowState::New && !edited => cell
                .italic()
                .text_color(theme.muted_foreground.opacity(0.5))
                .child("default")
                .into_any_element(),
            None => cell
                .italic()
                .text_color(theme.muted_foreground.opacity(0.7))
                .child("NULL")
                .into_any_element(),
            Some(v) => {
                let mut s: String = v.chars().take(CELL_CHARS).collect();
                if s.contains(['\n', '\r', '\t']) {
                    s = s.replace(['\n', '\r', '\t'], " ");
                }
                cell.child(s).into_any_element()
            }
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        Some(self.value(row_ix, col_ix).unwrap_or("NULL").to_string().into())
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.value(row_ix, col_ix).unwrap_or("").to_string()
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(type_name: Option<&str>, text: Option<&str>) -> CellValue {
        CellValue { column: "prefs".into(), type_name: type_name.map(str::to_string), text: text.map(str::to_string) }
    }

    #[test]
    fn the_viewer_indents_json_and_leaves_other_text_alone() {
        assert_eq!(value(Some("jsonb"), Some(r#"{"a":1,"b":[true]}"#)).pretty().unwrap(), "{\n  \"a\": 1,\n  \"b\": [\n    true\n  ]\n}");
        // JSON kept in a text column is indented too.
        assert!(value(Some("text"), Some("[1,2]")).pretty().unwrap().contains('\n'));
        assert_eq!(value(Some("text"), Some("{not json")).pretty().unwrap(), "{not json");
        assert_eq!(value(Some("text"), Some("plain")).pretty().unwrap(), "plain");
        assert_eq!(value(Some("text"), None).pretty(), None);
    }
}
