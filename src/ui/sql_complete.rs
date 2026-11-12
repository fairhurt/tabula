//! Hooks the SQL suggestions in `crate::complete` up to an editor: the menu
//! that opens as you type in a query tab or a Data tab's filter.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::input::{CompletionProvider, EditorState};
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::{App, Context, Entity, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, Range, TextEdit,
};

use crate::complete::{self, Catalog, Kind};
use crate::db::Session;
use crate::rt;

/// The database's names, shared between a tab and its editor's menu. Empty
/// until they have loaded; keywords are suggested meanwhile.
pub type SharedCatalog = Rc<RefCell<Arc<Catalog>>>;

struct SqlCompletion {
    catalog: SharedCatalog,
    /// The table a bare column name belongs to (a Data tab's filter).
    focus: Option<(String, String)>,
}

impl CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let sql = text.to_string();
        let catalog = self.catalog.borrow().clone();
        let focus = self.focus.as_ref().map(|(s, t)| (s.as_str(), t.as_str()));
        let (start, found) = complete::suggest(&catalog, &sql, offset, focus);
        // Each suggestion replaces the word being typed, wherever the menu
        // thinks that word began.
        let range = Range {
            start: text.offset_to_position(start),
            end: text.offset_to_position(offset.min(sql.len())),
        };
        let typed = sql.get(start..offset.min(sql.len())).unwrap_or("").to_string();
        let items = found
            .into_iter()
            .enumerate()
            .map(|(i, s)| CompletionItem {
                kind: Some(match s.kind {
                    Kind::Keyword => CompletionItemKind::KEYWORD,
                    Kind::Schema => CompletionItemKind::MODULE,
                    Kind::Table => CompletionItemKind::CLASS,
                    Kind::View => CompletionItemKind::INTERFACE,
                    Kind::Column => CompletionItemKind::FIELD,
                }),
                detail: (!s.detail.is_empty()).then_some(s.detail),
                // Keep the order the suggestions came in, and highlight as
                // much of the label as was typed.
                sort_text: Some(format!("{i:04}")),
                filter_text: Some(typed.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: s.insert })),
                label: s.label,
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    /// Open (or refine) the menu on a typed letter, digit, underscore or dot.
    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        let mut chars = new_text.chars();
        matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphanumeric() || c == '_' || c == '.')
    }
}

/// Give `editor` SQL suggestions from `session`'s database. The names load
/// in the background; `reload` fetches them again (after a table changes).
pub fn attach<T: 'static>(
    editor: &Entity<EditorState>,
    session: &Arc<Session>,
    focus: Option<(String, String)>,
    cx: &mut Context<T>,
) -> SharedCatalog {
    let catalog: SharedCatalog = Rc::new(RefCell::new(Arc::new(Catalog::default())));
    let provider = Rc::new(SqlCompletion { catalog: catalog.clone(), focus });
    editor.update(cx, |e, _| e.lsp_mut().completion_provider = Some(provider));
    reload(&catalog, session, cx);
    catalog
}

pub fn reload<T: 'static>(catalog: &SharedCatalog, session: &Arc<Session>, cx: &mut Context<T>) {
    let session = session.clone();
    let fut = rt::spawn(async move { session.completion_catalog().await });
    let catalog = catalog.clone();
    cx.spawn(async move |_, _| {
        if let Ok(loaded) = fut.await {
            *catalog.borrow_mut() = Arc::new(loaded);
        }
    })
    .detach();
}

/// Whether running `sql` may have changed what there is to suggest.
pub fn changes_schema(sql: &str) -> bool {
    let sql = sql.to_lowercase();
    ["create ", "alter ", "drop ", "rename "].iter().any(|w| sql.contains(w))
}
