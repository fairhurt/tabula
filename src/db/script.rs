//! Just enough lexing of SQL text for the server engines that don't split
//! scripts for us: where statements end, SQL Server's `GO` batches, and the
//! bare keywords of a statement (outside strings, comments and quoted names).
//!
//! It never rewrites SQL; it only finds boundaries, so a statement it doesn't
//! understand still goes to the server exactly as written.

use crate::profiles::Engine;

/// A piece of the text: code, or something to look past.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Piece {
    Code,
    /// A string, a quoted name, or a comment.
    Opaque,
}

/// Walk `sql`, calling `f(piece, start, end)` for each run of code and each
/// string, quoted identifier or comment.
fn lex(sql: &str, engine: Engine, mut f: impl FnMut(Piece, usize, usize)) {
    let b = sql.as_bytes();
    let (mut i, mut code_start) = (0, 0);
    let mysql = engine == Engine::MySql;
    while i < b.len() {
        let start = i;
        let end = match b[i] {
            b'\'' => Some(quoted(b, i, b'\'', mysql)),
            // A string in MySQL, a name everywhere else.
            b'"' => Some(quoted(b, i, b'"', mysql)),
            b'`' if mysql => Some(quoted(b, i, b'`', false)),
            b'[' if engine == Engine::SqlServer => Some(quoted(b, i, b']', false)),
            b'-' if b.get(i + 1) == Some(&b'-') => Some(line_end(b, i)),
            b'#' if mysql => Some(line_end(b, i)),
            // MySQL runs what's inside `/*! … */`, so that is code.
            b'/' if b.get(i + 1) == Some(&b'*') && !(mysql && b.get(i + 2) == Some(&b'!')) => {
                Some(block_comment_end(b, i, engine == Engine::SqlServer))
            }
            _ => None,
        };
        match end {
            Some(end) => {
                if code_start < start {
                    f(Piece::Code, code_start, start);
                }
                f(Piece::Opaque, start, end);
                i = end;
                code_start = end;
            }
            None => i += 1,
        }
    }
    if code_start < b.len() {
        f(Piece::Code, code_start, b.len());
    }
}

/// The end of the quoted run opening at `i`: a doubled closing quote is part
/// of it, and so (in MySQL strings) is anything after a backslash.
fn quoted(b: &[u8], i: usize, close: u8, backslash: bool) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == close {
            if b.get(j + 1) == Some(&close) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |n| i + n)
}

/// SQL Server's block comments nest; MySQL's don't.
fn block_comment_end(b: &[u8], i: usize, nests: bool) -> usize {
    let (mut j, mut depth) = (i + 2, 1);
    while j + 1 < b.len() {
        if b[j] == b'*' && b[j + 1] == b'/' {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return j;
            }
        } else if nests && b[j] == b'/' && b[j + 1] == b'*' {
            depth += 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    b.len()
}

/// Whether `sql` holds anything but whitespace and comments.
pub fn has_code(sql: &str, engine: Engine) -> bool {
    let mut found = false;
    lex(sql, engine, |piece, s, e| {
        let is_comment = |t: &str| t.starts_with("--") || t.starts_with('#') || t.starts_with("/*");
        found |= match piece {
            Piece::Code => !sql[s..e].trim().is_empty(),
            Piece::Opaque => !is_comment(&sql[s..e]),
        };
    });
    found
}

/// The statements in `sql`, split at semicolons outside strings, names and
/// comments; ones that are only comments or space are left out.
pub fn statements(sql: &str, engine: Engine) -> Vec<&str> {
    let mut cuts = vec![];
    lex(sql, engine, |piece, s, e| {
        if piece == Piece::Code {
            cuts.extend(sql[s..e].match_indices(';').map(|(i, _)| s + i));
        }
    });
    let mut out = vec![];
    let mut from = 0;
    for cut in cuts.into_iter().chain([sql.len()]) {
        let stmt = &sql[from..cut];
        if has_code(stmt, engine) {
            out.push(stmt.trim());
        }
        from = (cut + 1).min(sql.len());
    }
    out
}

/// SQL Server scripts split into batches at lines that say just `GO` (as
/// SSMS and sqlcmd do; the server itself has never heard of it). A count
/// after it (`GO 5`) is accepted and ignored.
pub fn batches(sql: &str) -> Vec<&str> {
    let mut opaque: Vec<(usize, usize)> = vec![];
    lex(sql, Engine::SqlServer, |piece, s, e| {
        if piece == Piece::Opaque {
            opaque.push((s, e));
        }
    });
    let inside = |at: usize| opaque.iter().any(|&(s, e)| s < at && at < e);
    let mut out = vec![];
    let (mut from, mut at) = (0, 0);
    for line in sql.split_inclusive('\n') {
        let mut words = line.split_whitespace();
        let go = words.next().is_some_and(|w| w.eq_ignore_ascii_case("go"))
            && words.next().is_none_or(|n| n.chars().all(|c| c.is_ascii_digit()))
            && words.next().is_none();
        if go && !inside(at + line.len() - line.trim_start().len()) {
            if has_code(&sql[from..at], Engine::SqlServer) {
                out.push(sql[from..at].trim());
            }
            from = at + line.len();
        }
        at += line.len();
    }
    if has_code(&sql[from..], Engine::SqlServer) {
        out.push(sql[from..].trim());
    }
    out
}

/// The bare words of `sql` in upper case: keywords and unquoted names, but
/// nothing inside a string, a quoted name or a comment.
pub fn words(sql: &str, engine: Engine) -> Vec<String> {
    let mut out = vec![];
    lex(sql, engine, |piece, s, e| {
        if piece == Piece::Code {
            out.extend(
                sql[s..e]
                    .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@' || c == '$' || c == '#'))
                    .filter(|w| !w.is_empty())
                    .map(str::to_ascii_uppercase),
            );
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_statements_outside_quotes_and_comments() {
        let sql = "select ';' as `a;b`, \"x;\\\";\" -- one; two\n; /* c; */ update t set a = 1;\n-- trailing;\n";
        assert_eq!(
            statements(sql, Engine::MySql),
            vec!["select ';' as `a;b`, \"x;\\\";\" -- one; two", "/* c; */ update t set a = 1"]
        );
        // SQL Server's names in brackets, and nested comments.
        let sql = "select [a;]]b] from t /* x /* y; */ z; */; select 2";
        assert_eq!(statements(sql, Engine::SqlServer), vec!["select [a;]]b] from t /* x /* y; */ z; */", "select 2"]);
        assert!(statements("  -- nothing\n/* here */ ;", Engine::MySql).is_empty());
        assert!(!has_code("# a MySQL comment", Engine::MySql));
    }

    #[test]
    fn finds_go_batches() {
        let sql = "create table t (x int)\nGO\ninsert t values (1)\n  go 2  \nselect 'not\nGO\na batch'\n-- GO\nselect 1\nGO";
        assert_eq!(
            batches(sql),
            vec!["create table t (x int)", "insert t values (1)", "select 'not\nGO\na batch'\n-- GO\nselect 1"]
        );
        assert_eq!(batches("select 1"), vec!["select 1"]);
        assert_eq!(batches("select 1 -- go\n"), vec!["select 1 -- go"]);
        assert!(batches("GO\n\nGO").is_empty());
    }

    #[test]
    fn bare_words_skip_strings_names_and_comments() {
        let w = words("SELECT 'delete', `update` FROM t /* drop */ # insert\nWHERE x INTO", Engine::MySql);
        assert_eq!(w, vec!["SELECT", "FROM", "T", "WHERE", "X", "INTO"]);
        // MySQL runs the inside of a `/*! */` comment, so it counts.
        assert!(words("select 1 /*!50000 , sleep(1) */", Engine::MySql).contains(&"SLEEP".to_string()));
        let w = words("select [delete], \"insert\", N'drop' from dbo.t", Engine::SqlServer);
        assert_eq!(w, vec!["SELECT", "N", "FROM", "DBO", "T"]);
    }
}
