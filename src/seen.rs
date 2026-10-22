//! Where copies of a message were seen, and which folders hold them now.

use std::collections::{BTreeSet, HashSet};

use anyhow::Result;
use turso::Statement;

use crate::store::Store;
use crate::util::ymd;

#[derive(Default)]
pub struct Seen {
    pub last_run: i64,
    pub last_path: String,
    pub last_time: i64,
    /// Paths in each active origin's latest import.
    pub current: BTreeSet<String>,
}

impl Seen {
    /// Current folders, or where it was last seen if it is in none.
    pub fn folder(&self) -> String {
        if self.current.is_empty() {
            format!(
                "gone; last seen in {} {}",
                self.last_path,
                ymd(self.last_time)
            )
        } else {
            self.current.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    }
}

pub struct Sightings {
    current_runs: HashSet<i64>,
    stmt: Statement,
}

impl Sightings {
    pub async fn new(store: &Store) -> Result<Sightings> {
        let mut current_runs = HashSet::new();
        let mut rows = store
            .conn
            .query(
                "SELECT max(r.run_id) FROM import_run r JOIN origin g ON g.origin_id = r.origin_id
                 WHERE r.finished_at IS NOT NULL AND g.retired_at IS NULL GROUP BY r.origin_id",
                (),
            )
            .await?;
        while let Some(r) = rows.next().await? {
            current_runs.insert(r.get::<i64>(0)?);
        }
        let stmt = store
            .conn
            .prepare(
                "SELECT s.run_id, s.path, r.started_at FROM observation o
                 JOIN file_sighting s ON s.content_id = o.content_id
                 JOIN import_run r ON r.run_id = s.run_id
                 WHERE o.artifact_id = ?1",
            )
            .await?;
        Ok(Sightings { current_runs, stmt })
    }

    /// Fold an artifact's sightings into `seen`; returns the artifact's last run.
    pub async fn add(&mut self, artifact_id: i64, seen: &mut Seen) -> Result<i64> {
        let mut last = 0;
        let mut rows = self.stmt.query((artifact_id,)).await?;
        while let Some(r) = rows.next().await? {
            let (run, path, time): (i64, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            last = last.max(run);
            if self.current_runs.contains(&run) {
                seen.current.insert(path.clone());
            }
            if run > seen.last_run {
                (seen.last_run, seen.last_path, seen.last_time) = (run, path, time);
            }
        }
        Ok(last)
    }
}
