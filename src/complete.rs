//! What to suggest while SQL is typed: the tables, columns and schemas of the
//! connected database, and SQL's own words. Plain text in, suggestions out;
//! the editor glue is in `ui::sql_complete`.

/// The names a database offers, loaded once per tab.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub schemas: Vec<String>,
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub view: bool,
    /// Name and type.
    pub columns: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Schema,
    Table,
    View,
    Column,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// What the menu shows.
    pub label: String,
    /// What is put in the text, replacing the word being typed.
    pub insert: String,
    /// A note beside the label: a column's type, a table's schema.
    pub detail: String,
    pub kind: Kind,
}

/// Most suggestions worth showing at once.
const LIMIT: usize = 40;

const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "LEFT JOIN", "INNER JOIN", "RIGHT JOIN", "FULL JOIN", "CROSS JOIN",
    "ON", "USING", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "OFFSET", "AS", "DISTINCT", "AND", "OR",
    "NOT", "NULL", "IS NULL", "IS NOT NULL", "IN", "LIKE", "ILIKE", "BETWEEN", "EXISTS", "CASE", "WHEN",
    "THEN", "ELSE", "END", "ASC", "DESC", "NULLS FIRST", "NULLS LAST", "UNION", "UNION ALL", "EXCEPT",
    "INTERSECT", "WITH", "INSERT INTO", "VALUES", "UPDATE", "SET", "DELETE FROM", "RETURNING", "CREATE TABLE",
    "CREATE INDEX", "ALTER TABLE", "DROP TABLE", "EXPLAIN", "EXPLAIN ANALYZE", "BEGIN", "COMMIT", "ROLLBACK",
    "TRUE", "FALSE", "INTERVAL", "CAST", "COALESCE", "NULLIF", "COUNT", "SUM", "AVG", "MIN", "MAX", "NOW()",
    "CURRENT_DATE", "CURRENT_TIMESTAMP", "DATE_TRUNC", "EXTRACT", "LOWER", "UPPER", "LENGTH", "ROUND",
    "STRING_AGG", "ARRAY_AGG", "JSONB_BUILD_OBJECT", "GENERATE_SERIES", "OVER", "PARTITION BY", "ROW_NUMBER()",
    "FILTER", "LATERAL", "ANY", "ALL",
];

/// Words that end a table reference, so can't be its alias.
const NOT_AN_ALIAS: &[&str] = &[
    "where", "on", "join", "left", "right", "inner", "outer", "full", "cross", "group", "order", "limit",
    "set", "values", "using", "natural", "having", "union", "returning", "offset", "as", "select", "from",
    "except", "intersect", "window", "for", "lateral",
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `name`, quoted when PostgreSQL would otherwise fold or reject it.
fn ident(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain { name.to_string() } else { crate::db::quote_ident(name) }
}

/// A table named in a statement, and what it is called there.
#[derive(Debug, PartialEq)]
struct Ref {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

/// The tables a statement reads or writes: after FROM, JOIN, UPDATE and INTO.
fn table_refs(statement: &str) -> Vec<Ref> {
    // Words (dotted names stay whole, quotes dropped) and commas.
    let mut tokens: Vec<String> = vec![];
    let mut chars = statement.chars().peekable();
    while let Some(c) = chars.next() {
        if is_word(c) || c == '"' {
            let mut t = String::new();
            let mut quoted = c == '"';
            if !quoted {
                t.push(c);
            }
            while let Some(&n) = chars.peek() {
                if quoted {
                    chars.next();
                    if n == '"' {
                        quoted = false;
                    } else {
                        t.push(n);
                    }
                } else if is_word(n) || n == '.' {
                    t.push(n);
                    chars.next();
                } else if n == '"' {
                    quoted = true;
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(t);
        } else if c == ',' || c == '(' || c == ')' {
            tokens.push(c.to_string());
        }
    }
    let mut refs = vec![];
    let mut i = 0;
    // Inside a FROM list, a comma introduces another table.
    let mut in_list = false;
    while i < tokens.len() {
        let low = tokens[i].to_lowercase();
        let starts = matches!(low.as_str(), "from" | "join" | "update" | "into") || (in_list && low == ",");
        if !starts {
            if low != "," && NOT_AN_ALIAS.contains(&low.as_str()) {
                in_list = false;
            }
            i += 1;
            continue;
        }
        in_list = low == "from" || low == ",";
        i += 1;
        let Some(name) = tokens.get(i).filter(|t| t.chars().next().is_some_and(|c| is_word(c))) else {
            continue;
        };
        if NOT_AN_ALIAS.contains(&name.to_lowercase().as_str()) {
            continue;
        }
        let (schema, table) = match name.rsplit_once('.') {
            Some((s, t)) => (Some(s.rsplit('.').next().unwrap_or(s).to_string()), t.to_string()),
            None => (None, name.clone()),
        };
        i += 1;
        if tokens.get(i).is_some_and(|t| t.eq_ignore_ascii_case("as")) {
            i += 1;
        }
        let alias = tokens
            .get(i)
            .filter(|t| t.chars().next().is_some_and(is_word) && !NOT_AN_ALIAS.contains(&t.to_lowercase().as_str()))
            .cloned();
        if alias.is_some() {
            i += 1;
        }
        refs.push(Ref { schema, table, alias });
    }
    refs
}

impl Catalog {
    fn table(&self, schema: Option<&str>, name: &str) -> Option<&Table> {
        let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        self.tables
            .iter()
            .find(|t| same(&t.name, name) && schema.is_none_or(|s| same(&t.schema, s)))
    }

    fn columns_of(&self, t: &Table, out: &mut Vec<Suggestion>) {
        out.extend(t.columns.iter().map(|(name, ty)| Suggestion {
            label: name.clone(),
            insert: ident(name),
            detail: format!("{ty} · {}", t.name),
            kind: Kind::Column,
        }));
    }

    fn tables_in(&self, schema: Option<&str>, qualify: bool, out: &mut Vec<Suggestion>) {
        for t in &self.tables {
            if schema.is_some_and(|s| !t.schema.eq_ignore_ascii_case(s)) {
                continue;
            }
            out.push(Suggestion {
                label: t.name.clone(),
                insert: if qualify && t.schema != "public" {
                    format!("{}.{}", ident(&t.schema), ident(&t.name))
                } else {
                    ident(&t.name)
                },
                detail: format!("{} · {}", t.schema, if t.view { "view" } else { "table" }),
                kind: if t.view { Kind::View } else { Kind::Table },
            });
        }
    }

    fn schemas(&self, out: &mut Vec<Suggestion>) {
        out.extend(self.schemas.iter().map(|s| Suggestion {
            label: s.clone(),
            insert: ident(s),
            detail: "schema".into(),
            kind: Kind::Schema,
        }));
    }
}

/// Suggestions for the word that ends at byte `offset` of `text`. Returns
/// where that word starts (what a chosen suggestion replaces) and the
/// suggestions, best first. `focus` is the table a bare column name belongs
/// to when the text names none: the table a Data tab's filter is for.
pub fn suggest(catalog: &Catalog, text: &str, offset: usize, focus: Option<(&str, &str)>) -> (usize, Vec<Suggestion>) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let start = before.char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(offset, |(i, _)| i);
    let word = &before[start..];
    // `a.b.` before the word: the names it is qualified by, nearest first.
    let mut qualifiers: Vec<&str> = vec![];
    let mut rest = &before[..start];
    while let Some(r) = rest.strip_suffix('.') {
        let r = r.strip_suffix('"').unwrap_or(r);
        let s = r.char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(r.len(), |(i, _)| i);
        if s == r.len() {
            break;
        }
        qualifiers.push(&r[s..]);
        rest = r[..s].strip_suffix('"').unwrap_or(&r[..s]);
    }
    if word.is_empty() && qualifiers.is_empty() {
        return (start, vec![]);
    }

    // The statement the cursor is in.
    let from = before.rfind(';').map_or(0, |i| i + 1);
    let to = text[offset..].find(';').map_or(text.len(), |i| offset + i);
    let refs = table_refs(&text[from..to]);

    let mut found: Vec<Suggestion> = vec![];
    if let Some(&q) = qualifiers.first() {
        if let Some(&schema) = qualifiers.get(1) {
            if let Some(t) = catalog.table(Some(schema), q) {
                catalog.columns_of(t, &mut found);
            }
        } else if let Some(r) = refs.iter().find(|r| r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q))) {
            if let Some(t) = catalog.table(r.schema.as_deref(), &r.table) {
                catalog.columns_of(t, &mut found);
            }
        } else if catalog.schemas.iter().any(|s| s.eq_ignore_ascii_case(q)) {
            catalog.tables_in(Some(q), false, &mut found);
        } else {
            let schema = refs.iter().find(|r| r.table.eq_ignore_ascii_case(q)).and_then(|r| r.schema.as_deref());
            if let Some(t) = catalog.table(schema, q) {
                catalog.columns_of(t, &mut found);
            }
        }
    } else {
        let previous = before[..start].split(|c: char| !is_word(c)).rev().find(|w| !w.is_empty()).unwrap_or("");
        let wants_table = matches!(previous.to_lowercase().as_str(), "from" | "join" | "update" | "into" | "table");
        if wants_table {
            catalog.tables_in(None, true, &mut found);
            catalog.schemas(&mut found);
        } else {
            let mut seen: Vec<&Table> = vec![];
            let named = refs.iter().filter_map(|r| catalog.table(r.schema.as_deref(), &r.table));
            let focused = focus.and_then(|(s, t)| catalog.table(Some(s), t));
            for t in focused.into_iter().chain(named) {
                if !seen.iter().any(|s| std::ptr::eq(*s, t)) {
                    seen.push(t);
                    catalog.columns_of(t, &mut found);
                }
            }
            // Keywords follow the case being typed.
            let lower = word.chars().any(|c| c.is_lowercase());
            found.extend(KEYWORDS.iter().map(|k| Suggestion {
                label: if lower { k.to_lowercase() } else { k.to_string() },
                insert: if lower { k.to_lowercase() } else { k.to_string() },
                detail: String::new(),
                kind: Kind::Keyword,
            }));
            if focus.is_none() {
                catalog.tables_in(None, true, &mut found);
                catalog.schemas(&mut found);
            }
        }
    }

    // Names that start with the word, then names that contain it.
    let w = word.to_lowercase();
    let (mut first, mut later): (Vec<Suggestion>, Vec<Suggestion>) = (vec![], vec![]);
    for s in found {
        let label = s.label.to_lowercase();
        if first.iter().chain(&later).any(|k: &Suggestion| k.label == s.label && k.kind == s.kind && k.detail == s.detail) {
            continue;
        }
        if label.starts_with(&w) {
            // The word itself, fully typed, isn't worth offering.
            if label != w || s.kind != Kind::Keyword {
                first.push(s);
            }
        } else if w.len() >= 2 && label.contains(&w) {
            later.push(s);
        }
    }
    first.extend(later);
    first.truncate(LIMIT);
    (start, first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shop() -> Catalog {
        let table = |name: &str, view: bool, cols: &[(&str, &str)]| Table {
            schema: "shop".into(),
            name: name.into(),
            view,
            columns: cols.iter().map(|(n, t)| (n.to_string(), t.to_string())).collect(),
        };
        Catalog {
            schemas: vec!["public".into(), "shop".into()],
            tables: vec![
                table("customers", false, &[("id", "integer"), ("email", "text"), ("country", "character(2)")]),
                table("orders", false, &[("id", "bigint"), ("customer_id", "integer"), ("status", "shop.order_status")]),
                table("order_totals", true, &[("order_id", "bigint"), ("total", "numeric")]),
            ],
        }
    }

    /// Suggestions for `text` with the cursor at `|`.
    fn at(text: &str) -> (usize, Vec<String>) {
        let offset = text.find('|').expect("a cursor");
        let text = text.replace('|', "");
        let (start, found) = suggest(&shop(), &text, offset, None);
        (start, found.into_iter().map(|s| s.insert).collect())
    }

    #[test]
    fn tables_after_from_and_join() {
        let (start, found) = at("select * from ord|");
        assert_eq!(start, 14);
        assert_eq!(found, ["shop.orders", "shop.order_totals"]);
        assert_eq!(at("select * from shop.orders o join cu|").1, ["shop.customers"]);
        assert_eq!(at("select * from sh|").1, ["shop"]);
    }

    #[test]
    fn a_schema_lists_its_tables_and_a_table_its_columns() {
        assert_eq!(at("select * from shop.|").1, ["customers", "orders", "order_totals"]);
        assert_eq!(at("select * from shop.cu|").1, ["customers"]);
        // Names that start with the word come before names that contain it.
        assert_eq!(at("select shop.orders.st| from shop.orders").1, ["status", "customer_id"]);
        assert_eq!(at("select orders.| from shop.orders").1, ["id", "customer_id", "status"]);
    }

    #[test]
    fn an_alias_stands_for_its_table() {
        assert_eq!(at("select o.cu| from shop.orders o join shop.customers as c on c.id = o.customer_id").1, ["customer_id"]);
        assert_eq!(at("select c.| from shop.orders o join shop.customers as c on true").1, ["id", "email", "country"]);
        // Only the statement the cursor is in counts.
        assert_eq!(at("select 1 from shop.customers o; select o.| from shop.orders o").1, ["id", "customer_id", "status"]);
    }

    #[test]
    fn bare_words_offer_columns_then_keywords() {
        let (_, found) = at("select * from shop.orders where st|");
        assert_eq!(found[0], "status");
        // Keywords follow the case being typed, and one already typed out isn't offered.
        assert_eq!(at("sel|").1, ["select"]);
        assert_eq!(at("SEL|").1, ["SELECT"]);
        assert!(at("select|").1.is_empty());
        assert!(at("select * from shop.orders where status = 'paid' ord|").1.contains(&"order by".to_string()));
        // Nothing to go on yet.
        assert!(at("select |").1.is_empty());
    }

    #[test]
    fn a_filter_completes_its_tables_columns() {
        let (start, found) = suggest(&shop(), "status = 'paid' and cus", 23, Some(("shop", "orders")));
        assert_eq!(start, 20);
        assert_eq!(found[0].insert, "customer_id");
        assert_eq!(found[0].detail, "integer · orders");
        // A filter doesn't offer other tables.
        let (_, found) = suggest(&shop(), "cu", 2, Some(("shop", "orders")));
        assert!(found.iter().all(|s| s.kind != Kind::Table), "{found:?}");
    }

    #[test]
    fn finds_the_tables_a_statement_names() {
        let refs = table_refs("select * from shop.orders o, \"shop\".\"customers\" c left join products as p on true where x in (1)");
        let names: Vec<_> = refs.iter().map(|r| (r.table.as_str(), r.alias.as_deref())).collect();
        assert_eq!(names, [("orders", Some("o")), ("customers", Some("c")), ("products", Some("p"))]);
        assert_eq!(refs[1].schema.as_deref(), Some("shop"));
        assert_eq!(table_refs("update shop.orders set status = 'paid'")[0].alias, None);
    }

    #[test]
    fn names_that_need_quotes_get_them() {
        assert_eq!(ident("orders"), "orders");
        assert_eq!(ident("Order Items"), "\"Order Items\"");
        assert_eq!(ident("2fa"), "\"2fa\"");
    }
}
