//! Grouped counts over matching messages, with percentages of the scope.

use std::collections::HashMap;

use anyhow::{Result, bail};
use turso::Connection;

use crate::headers;
use crate::query::Filter;

pub const NONE: &str = "(none)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Value,
    /// Lowercased first address, like Sieve `address :all`.
    Addr,
    /// Lowercased domain of the first address, like Sieve `address :domain`.
    Domain,
}

#[derive(Debug, Clone)]
pub struct Key {
    pub header: String,
    pub part: Part,
}

impl Key {
    /// `NAME`, `NAME:addr` or `NAME:domain`.
    pub fn parse(s: &str) -> Result<Key> {
        let (header, part) = match s.rsplit_once(':') {
            Some((h, "addr")) => (h, Part::Addr),
            Some((h, "domain")) => (h, Part::Domain),
            Some((_, p)) => bail!("unknown key part {p:?} in {s:?}; use :addr or :domain"),
            None => (s, Part::Value),
        };
        Ok(Key {
            header: header.trim().to_ascii_lowercase(),
            part,
        })
    }

    pub fn label(&self) -> String {
        match self.part {
            Part::Value => self.header.clone(),
            Part::Addr => format!("{}:addr", self.header),
            Part::Domain => format!("{}:domain", self.header),
        }
    }

    fn apply(&self, value: &str) -> String {
        match self.part {
            Part::Value => value.to_string(),
            Part::Addr => first_addr(value).unwrap_or_else(|| value.to_lowercase()),
            Part::Domain => first_addr(value)
                .and_then(|a| a.rsplit_once('@').map(|(_, d)| d.to_string()))
                .unwrap_or_else(|| NONE.to_string()),
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

/// First occurrence of `name` for each message in `table`.
async fn first_values(conn: &Connection, table: &str, name: &str) -> Result<HashMap<i64, String>> {
    let sql = if table == "artifact" {
        "SELECT artifact_id, value FROM header WHERE name = ?1 AND nth = 0".to_string()
    } else {
        format!(
            "SELECT h.artifact_id, h.value FROM {table} t
             CROSS JOIN header h ON h.artifact_id = t.artifact_id
             WHERE h.name = ?1 AND h.nth = 0"
        )
    };
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query((name,)).await?;
    let mut out = HashMap::new();
    while let Some(r) = rows.next().await? {
        out.insert(r.get(0)?, r.get(1)?);
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
        columns.push(first_values(conn, table, &k.header).await?);
    }
    let mut groups: HashMap<Vec<String>, u64> = HashMap::new();
    for id in ids(conn, table).await? {
        let group = keys
            .iter()
            .zip(&columns)
            .map(|(k, col)| {
                col.get(&id)
                    .map_or_else(|| NONE.to_string(), |v| k.apply(v))
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_addresses() {
        let k = Key::parse("From:addr").unwrap();
        assert_eq!(k.header, "from");
        assert_eq!(
            k.apply("Alice Example <Alice@Example.COM>"),
            "alice@example.com"
        );
        assert_eq!(
            Key::parse("from:domain")
                .unwrap()
                .apply("\"B, C\" <b@x.org>"),
            "x.org"
        );
        assert_eq!(Key::parse("x-mailer").unwrap().apply("Foo 1.0"), "Foo 1.0");
        assert!(Key::parse("from:bogus").is_err());
    }
}
