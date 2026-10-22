//! Search filters over artifacts, mirroring Sieve's `:matches` with `i;ascii-casemap`.

use anyhow::{Context, Result, bail};
use turso::{Connection, Value};

use crate::util::parse_ymd;

/// Sieve wildcard (`*`, `?`, `\` escapes) to a LIKE pattern with `\` as escape.
/// A pattern without wildcards is treated as `:contains`.
pub fn sieve_to_like(pattern: &str) -> String {
    let has_wild = {
        let mut esc = false;
        pattern.chars().any(|c| {
            let wild = !esc && (c == '*' || c == '?');
            esc = !esc && c == '\\';
            wild
        })
    };
    let mut out = String::with_capacity(pattern.len() + 2);
    if !has_wild {
        out.push('%');
    }
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' if has_wild => out.push('%'),
            '?' if has_wild => out.push('_'),
            '\\' => match chars.next() {
                Some(n) => push_literal(&mut out, n),
                None => push_literal(&mut out, '\\'),
            },
            c => push_literal(&mut out, c),
        }
    }
    if !has_wild {
        out.push('%');
    }
    out
}

fn push_literal(out: &mut String, c: char) {
    if matches!(c, '%' | '_' | '\\') {
        out.push('\\');
    }
    out.push(c);
}

/// Sieve `:matches` (or `:contains` without wildcards), ASCII case-insensitive.
pub fn sieve_match(pattern: &str, text: &str) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Tok {
        Any,
        One,
        Lit(char),
    }
    let mut toks = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        toks.push(match c {
            '*' => Tok::Any,
            '?' => Tok::One,
            '\\' => Tok::Lit(chars.next().unwrap_or('\\')),
            c => Tok::Lit(c),
        });
    }
    if !toks.iter().any(|t| matches!(t, Tok::Any | Tok::One)) {
        toks.insert(0, Tok::Any);
        toks.push(Tok::Any);
    }
    let text: Vec<char> = text.chars().collect();
    // Classic glob matching with single-star backtracking.
    let (mut p, mut t, mut star, mut mark) = (0, 0, None, 0);
    while t < text.len() {
        match toks.get(p) {
            Some(Tok::Any) => {
                star = Some(p);
                mark = t;
                p += 1;
            }
            Some(Tok::One) => {
                p += 1;
                t += 1;
            }
            Some(Tok::Lit(c)) if c.eq_ignore_ascii_case(&text[t]) => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some(s) => {
                    p = s + 1;
                    mark += 1;
                    t = mark;
                }
                None => return false,
            },
        }
    }
    toks[p..].iter().all(|t| *t == Tok::Any)
}

/// The longest wildcard-free run of a pattern as a `%run%` LIKE, to narrow candidates.
fn like_hint(pattern: &str) -> String {
    let mut best = String::new();
    let mut cur = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' | '?' => {
                if cur.len() > best.len() {
                    best = std::mem::take(&mut cur);
                }
                cur.clear();
            }
            '\\' => push_literal(&mut cur, chars.next().unwrap_or('\\')),
            c => push_literal(&mut cur, c),
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    format!("%{best}%")
}

enum Set {
    Sql(String, Vec<Value>),
    /// Header values narrowed in SQL by name and a LIKE hint, then decided in Rust;
    /// a message matches if any value of any of the names does.
    Predicate {
        names: Vec<String>,
        like: String,
        test: Box<dyn Fn(&str) -> bool>,
    },
}

async fn predicate_set(
    conn: &Connection,
    table: &str,
    names: &[String],
    like: &str,
    test: &dyn Fn(&str) -> bool,
) -> Result<u64> {
    let mut stmt = conn
        .prepare(
            "SELECT artifact_id, value FROM header WHERE name = ?1 AND value LIKE ?2 ESCAPE '\\'",
        )
        .await?;
    let mut ids = Vec::new();
    for name in names {
        let mut rows = stmt.query((name.as_str(), like)).await?;
        while let Some(r) = rows.next().await? {
            let value: String = r.get(1)?;
            if test(&value) {
                ids.push(r.get::<i64>(0)?);
            }
        }
    }
    let mut ins = conn
        .prepare(&format!(
            "INSERT OR IGNORE INTO {table} (artifact_id) VALUES (?1)"
        ))
        .await?;
    let mut n = 0;
    for id in ids {
        n += ins.execute((id,)).await?;
    }
    Ok(n)
}

/// Headers naming someone who sent or received a message.
pub const PARTICIPANT_HEADERS: [&str; 6] = ["from", "sender", "reply-to", "to", "cc", "bcc"];

#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// (header name, Sieve pattern) on decoded values, like Sieve `header`.
    pub headers: Vec<(String, String)>,
    /// (header name, Sieve pattern) on any address in it, like Sieve `address :all`.
    pub addresses: Vec<(String, String)>,
    /// Sieve pattern on any address in any of [`PARTICIPANT_HEADERS`].
    pub participants: Vec<String>,
    /// (header name, regex) on decoded values, case-insensitive, like Sieve `header :regex`.
    pub regexes: Vec<(String, String)>,
    pub has: Vec<String>,
    pub folder: Option<String>,
    pub origin: Option<String>,
    /// Restrict origin/folder to each origin's latest import instead of any sighting ever.
    pub current: bool,
    pub since: Option<String>,
    pub before: Option<String>,
}

impl Filter {
    /// One `SELECT artifact_id ...` per condition, with its bind values.
    fn sets(&self) -> Result<Vec<Set>> {
        let mut sets = Vec::new();
        let participants = self.participants.iter().map(|p| {
            let names = PARTICIPANT_HEADERS.map(String::from).to_vec();
            (names, p)
        });
        let addresses = self
            .addresses
            .iter()
            .map(|(name, p)| (vec![name.to_ascii_lowercase()], p));
        for (names, pattern) in addresses.chain(participants) {
            let pattern = pattern.clone();
            sets.push(Set::Predicate {
                names,
                like: like_hint(&pattern),
                test: Box::new(move |v| {
                    crate::headers::addresses(v)
                        .iter()
                        .any(|a| sieve_match(&pattern, a))
                }),
            });
        }
        for (name, re) in &self.regexes {
            let re = regex::RegexBuilder::new(re)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("bad regex {re:?}"))?;
            sets.push(Set::Predicate {
                names: vec![name.to_ascii_lowercase()],
                like: "%".into(),
                test: Box::new(move |v| re.is_match(v)),
            });
        }
        for (name, pattern) in &self.headers {
            sets.push(Set::Sql(
                "SELECT artifact_id FROM header WHERE name = ? AND value LIKE ? ESCAPE '\\'"
                    .to_string(),
                vec![
                    name.to_ascii_lowercase().into(),
                    sieve_to_like(pattern).into(),
                ],
            ));
        }
        for name in &self.has {
            sets.push(Set::Sql(
                "SELECT artifact_id FROM header WHERE name = ?".to_string(),
                vec![name.to_ascii_lowercase().into()],
            ));
        }
        if self.folder.is_some() || self.origin.is_some() || self.current {
            let mut sql = String::from(
                "SELECT o.artifact_id FROM observation o
                 JOIN file_sighting s ON s.content_id = o.content_id
                 JOIN origin g ON g.origin_id = s.origin_id",
            );
            if self.current {
                // Only what each origin's latest finished import saw.
                sql.push_str(
                    " JOIN (SELECT max(run_id) AS run_id FROM import_run
                      WHERE finished_at IS NOT NULL GROUP BY origin_id) cur ON cur.run_id = s.run_id",
                );
            }
            sql.push_str(" WHERE 1");
            if self.current && self.origin.is_none() {
                // A retired origin's last run is history, not where mail is now.
                sql.push_str(" AND g.retired_at IS NULL");
            }
            let mut params = Vec::new();
            if let Some(f) = &self.folder {
                sql.push_str(" AND s.path LIKE ? ESCAPE '\\'");
                params.push(sieve_to_like(f).into());
            }
            if let Some(o) = &self.origin {
                sql.push_str(" AND g.label = ?");
                params.push(o.clone().into());
            }
            sets.push(Set::Sql(sql, params));
        }
        if self.since.is_some() || self.before.is_some() {
            let mut sql = String::from("SELECT artifact_id FROM artifact WHERE 1");
            let mut params = Vec::new();
            for (date, op) in [(&self.since, ">="), (&self.before, "<")] {
                if let Some(d) = date {
                    let Some(t) = parse_ymd(d) else {
                        bail!("bad date {d:?}, want YYYY-MM-DD")
                    };
                    sql.push_str(&format!(" AND date {op} ?"));
                    params.push(t.into());
                }
            }
            sets.push(Set::Sql(sql, params));
        }
        Ok(sets)
    }

    /// Only the conditions that define the population (where and when), not
    /// which messages within it; percentages are taken against this.
    pub fn scope(&self) -> Filter {
        Filter {
            folder: self.folder.clone(),
            origin: self.origin.clone(),
            current: self.current,
            since: self.since.clone(),
            before: self.before.clone(),
            ..Filter::default()
        }
    }

    /// Fill TEMP TABLE `hit` with matching artifact ids and return the table to
    /// select from: `hit`, or `artifact` when there is no filter.
    pub async fn materialize(&self, conn: &Connection) -> Result<&'static str> {
        Ok(if self.materialize_into(conn, "hit").await? {
            "hit"
        } else {
            "artifact"
        })
    }

    /// Fill TEMP TABLE `table` with matching artifact ids; false when there is
    /// no filter (the table is left untouched). Each condition is materialised
    /// and intersected by primary-key joins, because turso scans
    /// `IN (subquery-with-join)` lists linearly.
    pub async fn materialize_into(&self, conn: &Connection, table: &str) -> Result<bool> {
        let sets = self.sets()?;
        if sets.is_empty() {
            return Ok(false);
        }
        let mut sized = Vec::new();
        for (i, set) in sets.into_iter().enumerate() {
            let t = format!("c{i}");
            conn.execute(
                &format!("CREATE TEMP TABLE IF NOT EXISTS {t} (artifact_id INTEGER PRIMARY KEY)"),
                (),
            )
            .await?;
            conn.execute(&format!("DELETE FROM {t}"), ()).await?;
            let n = match set {
                Set::Sql(sql, params) => {
                    conn.execute(&format!("INSERT OR IGNORE INTO {t} {sql}"), params)
                        .await?
                }
                Set::Predicate { names, like, test } => {
                    predicate_set(conn, &t, &names, &like, &*test).await?
                }
            };
            sized.push((n, t));
        }
        sized.sort();
        let (_, first) = &sized[0];
        let mut sql = format!("INSERT INTO {table} SELECT {first}.artifact_id FROM {first}");
        for (_, t) in &sized[1..] {
            sql.push_str(&format!(
                " JOIN {t} ON {t}.artifact_id = {first}.artifact_id"
            ));
        }
        conn.execute(
            &format!("CREATE TEMP TABLE IF NOT EXISTS {table} (artifact_id INTEGER PRIMARY KEY)"),
            (),
        )
        .await?;
        conn.execute(&format!("DELETE FROM {table}"), ()).await?;
        conn.execute(&sql, ()).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sieve_patterns() {
        assert_eq!(sieve_to_like("dovecot"), "%dovecot%");
        assert_eq!(sieve_to_like("*@github.com"), "%@github.com");
        assert_eq!(sieve_to_like("a?c"), "a_c");
        assert_eq!(sieve_to_like("100%_off*"), "100\\%\\_off%");
        assert_eq!(sieve_to_like("lit\\*eral"), "%lit*eral%");
        assert_eq!(sieve_to_like("x\\*y*"), "x*y%");
        assert!(sieve_match("*@substack.com", "News@SubStack.com"));
        assert!(!sieve_match("*@substack.com", "a@substack.com.evil"));
        assert!(sieve_match("github", "notifications@github.com"));
        assert!(sieve_match("a?c", "abc"));
        assert!(!sieve_match("a?c", "abbc"));
        assert!(sieve_match("*", ""));
        assert_eq!(like_hint("*@sub*stack.com"), "%stack.com%");
    }
}
