//! Search filters over artifacts, mirroring Sieve's `:matches` with `i;ascii-casemap`.

use anyhow::{Result, bail};
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

#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// (lowercased header name, Sieve pattern)
    pub headers: Vec<(String, String)>,
    pub has: Vec<String>,
    pub folder: Option<String>,
    pub origin: Option<String>,
    pub since: Option<String>,
    pub before: Option<String>,
}

impl Filter {
    /// One `SELECT artifact_id ...` per condition, with its bind values.
    fn sets(&self) -> Result<Vec<(String, Vec<Value>)>> {
        let mut sets = Vec::new();
        for (name, pattern) in &self.headers {
            sets.push((
                "SELECT artifact_id FROM header WHERE name = ? AND value LIKE ? ESCAPE '\\'"
                    .to_string(),
                vec![
                    name.to_ascii_lowercase().into(),
                    sieve_to_like(pattern).into(),
                ],
            ));
        }
        for name in &self.has {
            sets.push((
                "SELECT artifact_id FROM header WHERE name = ?".to_string(),
                vec![name.to_ascii_lowercase().into()],
            ));
        }
        if self.folder.is_some() || self.origin.is_some() {
            let mut sql = String::from(
                "SELECT o.artifact_id FROM observation o
                 JOIN file_sighting s ON s.content_id = o.content_id
                 JOIN origin g ON g.origin_id = s.origin_id WHERE 1",
            );
            let mut params = Vec::new();
            if let Some(f) = &self.folder {
                sql.push_str(" AND s.path LIKE ? ESCAPE '\\'");
                params.push(sieve_to_like(f).into());
            }
            if let Some(o) = &self.origin {
                sql.push_str(" AND g.label = ?");
                params.push(o.clone().into());
            }
            sets.push((sql, params));
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
            sets.push((sql, params));
        }
        Ok(sets)
    }

    /// Fill TEMP TABLE `hit` with matching artifact ids and return the table to
    /// select from: `hit`, or `artifact` when there is no filter. Each condition
    /// is materialised and intersected by primary-key joins, because turso
    /// scans `IN (subquery-with-join)` lists linearly.
    pub async fn materialize(&self, conn: &Connection) -> Result<&'static str> {
        let sets = self.sets()?;
        if sets.is_empty() {
            return Ok("artifact");
        }
        let mut sized = Vec::new();
        for (i, (sql, params)) in sets.into_iter().enumerate() {
            let t = format!("c{i}");
            conn.execute(
                &format!("CREATE TEMP TABLE IF NOT EXISTS {t} (artifact_id INTEGER PRIMARY KEY)"),
                (),
            )
            .await?;
            conn.execute(&format!("DELETE FROM {t}"), ()).await?;
            let n = conn
                .execute(&format!("INSERT OR IGNORE INTO {t} {sql}"), params)
                .await?;
            sized.push((n, t));
        }
        sized.sort();
        let (_, first) = &sized[0];
        let mut sql = format!("INSERT INTO hit SELECT {first}.artifact_id FROM {first}");
        for (_, t) in &sized[1..] {
            sql.push_str(&format!(
                " JOIN {t} ON {t}.artifact_id = {first}.artifact_id"
            ));
        }
        conn.execute(
            "CREATE TEMP TABLE IF NOT EXISTS hit (artifact_id INTEGER PRIMARY KEY)",
            (),
        )
        .await?;
        conn.execute("DELETE FROM hit", ()).await?;
        conn.execute(&sql, ()).await?;
        Ok("hit")
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
    }
}
