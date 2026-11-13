//! Grouped counts over matching messages, with percentages of the scope.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use turso::Connection;

use crate::headers;
use crate::query::{Filter, sieve_match};

pub const NONE: &str = "(none)";

pub const MATCH: &str = "match";
pub const NO_MATCH: &str = "no match";

#[derive(Debug, Clone)]
pub enum Part {
    Value,
    /// Lowercased first address, like Sieve `address :all`.
    Addr,
    /// Lowercased domain of the first address, like Sieve `address :domain`.
    Domain,
    /// Any occurrence matches, case-insensitive, like Sieve `header :regex`.
    Regex(regex::Regex),
    /// Any occurrence matches, like Sieve `header :matches`.
    Matches(String),
}

#[derive(Debug, Clone)]
pub struct Key {
    pub header: String,
    pub part: Part,
    label: String,
}

impl Key {
    /// `NAME`, `NAME:addr`, `NAME:domain`, `NAME~REGEX` or `NAME=SIEVE-PATTERN`.
    pub fn parse(s: &str) -> Result<Key> {
        let (header, part) = if let Some((h, re)) = s.split_once('~') {
            let re = regex::RegexBuilder::new(re)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("bad regex in {s:?}"))?;
            (h, Part::Regex(re))
        } else if let Some((h, pat)) = s.split_once('=') {
            (h, Part::Matches(pat.to_string()))
        } else {
            match s.rsplit_once(':') {
                Some((h, "addr")) => (h, Part::Addr),
                Some((h, "domain")) => (h, Part::Domain),
                Some((_, p)) => bail!("unknown key part {p:?} in {s:?}; use :addr or :domain"),
                None => (s, Part::Value),
            }
        };
        Ok(Key {
            header: header.trim().to_ascii_lowercase(),
            part,
            label: s.trim().to_string(),
        })
    }

    pub fn label(&self) -> String {
        self.label.clone()
    }

    fn is_test(&self) -> bool {
        matches!(self.part, Part::Regex(_) | Part::Matches(_))
    }

    /// Group label for a message given every occurrence of the header, first first.
    pub fn group(&self, values: Option<&Vec<String>>) -> String {
        let Some(values) = values else {
            return NONE.to_string();
        };
        let hit = |v: &String| match &self.part {
            Part::Regex(re) => re.is_match(v),
            Part::Matches(p) => sieve_match(p, v),
            _ => false,
        };
        if self.is_test() {
            return if values.iter().any(hit) {
                MATCH
            } else {
                NO_MATCH
            }
            .to_string();
        }
        let value = &values[0];
        match self.part {
            Part::Addr => first_addr(value).unwrap_or_else(|| value.to_lowercase()),
            Part::Domain => first_addr(value)
                .and_then(|a| a.rsplit_once('@').map(|(_, d)| d.to_string()))
                .unwrap_or_else(|| NONE.to_string()),
            _ => value.clone(),
        }
    }
}

pub fn first_addr(value: &str) -> Option<String> {
    headers::addresses(value).into_iter().next()
}

pub struct Report {
    pub scope: u64,
    pub matched: u64,
    /// (group key values, count), most common first.
    pub rows: Vec<(Vec<String>, u64)>,
}

async fn count(conn: &Connection, table: &str) -> Result<u64> {
    let mut rows = conn
        .query(format!("SELECT count(*) FROM {table}"), ())
        .await?;
    let n: i64 = rows.next().await?.map_or(Ok(0), |r| r.get(0))?;
    Ok(n as u64)
}

async fn ids(conn: &Connection, table: &str) -> Result<Vec<i64>> {
    let mut rows = conn
        .query(format!("SELECT artifact_id FROM {table}"), ())
        .await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push(r.get(0)?);
    }
    Ok(out)
}

/// Values of `name` for each message in `table`, in header order; only the first if `first_only`.
async fn values(
    conn: &Connection,
    table: &str,
    name: &str,
    first_only: bool,
) -> Result<HashMap<i64, Vec<String>>> {
    let nth = if first_only { " AND h.nth = 0" } else { "" };
    let sql = if table == "artifact" {
        format!(
            "SELECT h.artifact_id, h.value FROM header h WHERE h.name = ?1{nth} ORDER BY h.artifact_id, h.nth"
        )
    } else {
        format!(
            "SELECT h.artifact_id, h.value FROM {table} t
             CROSS JOIN header h ON h.artifact_id = t.artifact_id
             WHERE h.name = ?1{nth} ORDER BY h.artifact_id, h.nth"
        )
    };
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query((name,)).await?;
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    while let Some(r) = rows.next().await? {
        out.entry(r.get(0)?).or_default().push(r.get(1)?);
    }
    Ok(out)
}

pub async fn run(conn: &Connection, filter: &Filter, keys: &[Key]) -> Result<Report> {
    let scope_table = if filter.scope().materialize_into(conn, "scope").await? {
        "scope"
    } else {
        "artifact"
    };
    let scope = count(conn, scope_table).await?;
    let table = filter.materialize(conn).await?;
    let matched = count(conn, table).await?;

    let mut columns = Vec::new();
    for k in keys {
        columns.push(values(conn, table, &k.header, !k.is_test()).await?);
    }
    let mut groups: HashMap<Vec<String>, u64> = HashMap::new();
    for id in ids(conn, table).await? {
        let group = keys
            .iter()
            .zip(&columns)
            .map(|(k, col)| k.group(col.get(&id)))
            .collect();
        *groups.entry(group).or_default() += 1;
    }
    let mut rows: Vec<_> = groups.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(Report {
        scope,
        matched,
        rows,
    })
}

pub struct HeaderProfile {
    pub name: String,
    /// Messages carrying the header.
    pub carried: u64,
    /// (value, messages), most common first.
    pub values: Vec<(String, u64)>,
}

impl HeaderProfile {
    pub fn distinct(&self) -> usize {
        self.values.len()
    }

    pub fn all_unique(&self) -> bool {
        self.carried > 1 && self.distinct() as u64 == self.carried
    }

    /// Sort rank and section label.
    pub fn kind(&self) -> (u8, &'static str) {
        match self.distinct() {
            1 => (1, "constant"),
            d if d <= CATEGORY_MAX && !self.all_unique() => (0, "categorical"),
            _ => (2, "high-cardinality / identifiers"),
        }
    }
}

/// Above this many distinct values a header is treated as an identifier, not a category.
pub const CATEGORY_MAX: usize = 50;

/// Every header (first occurrence per message) among matching messages, with its value
/// distribution. Categorical headers first, then constants, then near-unique ones.
pub async fn profile(
    conn: &Connection,
    filter: &Filter,
    like: Option<&str>,
) -> Result<(u64, Vec<HeaderProfile>)> {
    let table = filter.materialize(conn).await?;
    let matched = count(conn, table).await?;
    let (from, mut params): (String, Vec<turso::Value>) = if table == "artifact" {
        ("header h".into(), vec![])
    } else {
        (
            format!("{table} t CROSS JOIN header h ON h.artifact_id = t.artifact_id"),
            vec![],
        )
    };
    let mut sql = format!("SELECT h.name, h.value FROM {from} WHERE h.nth = 0");
    if let Some(l) = like {
        sql.push_str(" AND h.name LIKE ? ESCAPE '\\'");
        params.push(crate::query::sieve_to_like(l).into());
    }
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut by_name: HashMap<String, HashMap<String, u64>> = HashMap::new();
    while let Some(r) = rows.next().await? {
        let name: String = r.get(0)?;
        let value: String = r.get(1)?;
        *by_name.entry(name).or_default().entry(value).or_default() += 1;
    }
    let mut out: Vec<HeaderProfile> = by_name
        .into_iter()
        .map(|(name, vals)| {
            let carried = vals.values().sum();
            let mut values: Vec<_> = vals.into_iter().collect();
            values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            HeaderProfile {
                name,
                carried,
                values,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.kind()
            .0
            .cmp(&b.kind().0)
            .then_with(|| a.distinct().cmp(&b.distinct()))
            .then_with(|| b.carried.cmp(&a.carried))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok((matched, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_addresses() {
        let g = |k: &str, vs: &[&str]| {
            Key::parse(k)
                .unwrap()
                .group(Some(&vs.iter().map(|v| v.to_string()).collect()))
        };
        assert_eq!(Key::parse("From:addr").unwrap().header, "from");
        assert_eq!(
            g("from:addr", &["Alice Example <Alice@Example.COM>"]),
            "alice@example.com"
        );
        assert_eq!(g("from:domain", &["\"B, C\" <b@x.org>"]), "x.org");
        assert_eq!(g("x-mailer", &["Foo 1.0", "Bar"]), "Foo 1.0");
        let re = "references~^<Acme/(rocket|anvil)/";
        assert_eq!(g(re, &["<acme/Rocket/pull/1@github.com>"]), MATCH);
        assert_eq!(g(re, &["<widgets/gear/pull/1@github.com>"]), NO_MATCH);
        assert_eq!(g("received=*by b*", &["from a by c", "from x by b"]), MATCH);
        assert_eq!(Key::parse(re).unwrap().group(None), NONE);
        assert!(Key::parse("from:bogus").is_err());
        assert!(Key::parse("x~(").is_err());
    }
}
