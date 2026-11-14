//! Where copies of a message were found, per origin and path, and which of
//! those places hold it now. Only finished imports count.

use std::collections::HashMap;

use anyhow::Result;
use turso::Statement;

use crate::store::Store;
use crate::util::ymd;

#[derive(Clone, Debug)]
pub struct Location {
    pub origin: String,
    pub path: String,
    /// Seen in the origin's latest finished import, and the origin is not retired.
    pub current: bool,
    pub first_seen: i64,
    pub last_seen: i64,
    /// When the origin was last imported successfully, whether or not this copy was there.
    pub origin_last_import: Option<i64>,
    pub origin_retired: bool,
}

struct Origin {
    label: String,
    retired: bool,
    last_run: Option<i64>,
    last_time: Option<i64>,
}

pub struct Sightings {
    origins: HashMap<i64, Origin>,
    stmt: Statement,
    presence: Statement,
}

impl Sightings {
    pub async fn new(store: &Store) -> Result<Sightings> {
        let mut origins = HashMap::new();
        let mut rows = store
            .conn
            .query(
                "SELECT g.origin_id, g.label, g.retired_at IS NOT NULL, max(r.run_id), max(r.started_at)
                 FROM origin g LEFT JOIN import_run r
                   ON r.origin_id = g.origin_id AND r.finished_at IS NOT NULL
                 GROUP BY g.origin_id",
                (),
            )
            .await?;
        while let Some(r) = rows.next().await? {
            origins.insert(
                r.get::<i64>(0)?,
                Origin {
                    label: r.get(1)?,
                    retired: r.get::<i64>(2)? != 0,
                    last_run: r.get(3)?,
                    last_time: r.get(4)?,
                },
            );
        }
        let stmt = store
            .conn
            .prepare(
                "SELECT s.origin_id, s.run_id, s.path, r.started_at FROM observation o
                 JOIN file_sighting s ON s.content_id = o.content_id
                 JOIN import_run r ON r.run_id = s.run_id
                 WHERE o.artifact_id = ?1 AND r.finished_at IS NOT NULL",
            )
            .await?;
        // A closed interval was last seen in the origin's last run before it closed.
        let presence = store
            .conn
            .prepare(
                "SELECT p.origin_id, p.path, p.until_run IS NULL,
                        (SELECT started_at FROM import_run WHERE run_id = p.since_run),
                        (SELECT max(r.started_at) FROM import_run r
                         WHERE r.origin_id = p.origin_id AND r.finished_at IS NOT NULL
                           AND (p.until_run IS NULL OR r.run_id < p.until_run))
                 FROM presence p WHERE p.artifact_id = ?1",
            )
            .await?;
        Ok(Sightings {
            origins,
            stmt,
            presence,
        })
    }

    /// Current places first, then most recently seen first.
    pub async fn of(&mut self, artifact_id: i64) -> Result<Vec<Location>> {
        let mut by: HashMap<(i64, String), Location> = HashMap::new();
        let mut rows = self.stmt.query((artifact_id,)).await?;
        while let Some(r) = rows.next().await? {
            let (origin_id, run, path, time): (i64, i64, String, i64) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            let Some(g) = self.origins.get(&origin_id) else {
                continue;
            };
            let current = !g.retired && g.last_run == Some(run);
            let loc = by.entry((origin_id, path.clone())).or_insert(Location {
                origin: g.label.clone(),
                path,
                current,
                first_seen: time,
                last_seen: time,
                origin_last_import: g.last_time,
                origin_retired: g.retired,
            });
            loc.current |= current;
            loc.first_seen = loc.first_seen.min(time);
            loc.last_seen = loc.last_seen.max(time);
        }
        drop(rows);
        let mut rows = self.presence.query((artifact_id,)).await?;
        while let Some(r) = rows.next().await? {
            let (origin_id, path, open): (i64, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            let (Some(first), Some(last)) = (r.get::<Option<i64>>(3)?, r.get::<Option<i64>>(4)?)
            else {
                continue;
            };
            let Some(g) = self.origins.get(&origin_id) else {
                continue;
            };
            let current = !g.retired && open != 0;
            let loc = by.entry((origin_id, path.clone())).or_insert(Location {
                origin: g.label.clone(),
                path,
                current,
                first_seen: first,
                last_seen: last,
                origin_last_import: g.last_time,
                origin_retired: g.retired,
            });
            loc.current |= current;
            loc.first_seen = loc.first_seen.min(first);
            loc.last_seen = loc.last_seen.max(last);
        }
        let mut out: Vec<Location> = by.into_values().collect();
        sort(&mut out);
        Ok(out)
    }
}

fn sort(locs: &mut [Location]) {
    locs.sort_by(|a, b| {
        (b.current, b.last_seen, &a.origin, &a.path).cmp(&(
            a.current,
            a.last_seen,
            &b.origin,
            &b.path,
        ))
    });
}

/// Combine the places of several copies of one message.
pub fn merge(into: &mut Vec<Location>, more: Vec<Location>) {
    for m in more {
        match into
            .iter_mut()
            .find(|l| l.origin == m.origin && l.path == m.path)
        {
            Some(l) => {
                l.current |= m.current;
                l.first_seen = l.first_seen.min(m.first_seen);
                l.last_seen = l.last_seen.max(m.last_seen);
            }
            None => into.push(m),
        }
    }
    sort(into);
}

/// Folders holding it now, or where it was last seen if none does.
pub fn folder(locs: &[Location]) -> String {
    let mut now: Vec<&str> = locs
        .iter()
        .filter(|l| l.current)
        .map(|l| l.path.as_str())
        .collect();
    now.sort_unstable();
    now.dedup();
    if !now.is_empty() {
        return now.join(", ");
    }
    match locs.iter().max_by_key(|l| l.last_seen) {
        Some(l) => format!("gone; last seen in {} {}", l.path, ymd(l.last_seen)),
        None => "never seen in a finished import".into(),
    }
}
