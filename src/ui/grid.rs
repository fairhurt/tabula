//! The results grid shared by the Query and Data tabs: virtualized rows,
//! resizable columns, sorting, NULLs drawn as NULL, copy as TSV.

use gpui_kit::component::{
    ActiveTheme as _,
    table::{Column, ColumnSort, TableDelegate, TableSelection, TableState},
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClipboardItem, Context, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div,
};

use crate::db::{self, ResultColumn};

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
        }
    }

    pub fn set(&mut self, columns: Vec<ResultColumn>, rows: Vec<Vec<Option<String>>>) {
        self.order = (0..rows.len()).collect();
        self.columns = columns;
        self.rows = rows;
        self.loading = false;
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    pub fn columns(&self) -> &[ResultColumn] {
        &self.columns
    }


    fn value(&self, row_ix: usize, col_ix: usize) -> Option<&str> {
        let r = *self.order.get(row_ix)?;
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

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let numeric = self.columns.get(col_ix).is_some_and(|c| c.is_numeric());
        let cell = div()
            .w_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(super::rem(12.))
            .when(numeric, |d| d.text_right());
        match self.value(row_ix, col_ix) {
            None => cell
                .italic()
                .text_color(cx.theme().muted_foreground.opacity(0.7))
                .child("NULL"),
            Some(v) => {
                let mut s: String = v.chars().take(CELL_CHARS).collect();
                if s.contains(['\n', '\r', '\t']) {
                    s = s.replace(['\n', '\r', '\t'], " ");
                }
                cell.child(s)
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
