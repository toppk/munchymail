//! Where copies are in a live API or IMAP origin, as intervals.
//!
//! An mbox file is recorded whole at each import (`file_sighting`,
//! `observation`). A Gmail label or IMAP folder is a listing that a sync
//! checks every few minutes, so recording it whole at each run would grow with
//! the folder, not with what changed. Instead each entry (a Gmail message id,
//! an IMAP `uidvalidity:uid`) at a path has a `presence` row from the run it
//! appeared in until the run it was gone from; a run writes only what changed.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use tracing::info;

use crate::store::{Store, now};

/// Each path's (entry, artifact) pairs, as a run found them.
pub type Listing = BTreeMap<String, Vec<(String, i64)>>;

/// Make `listing` the origin's presence as of `run_id`: open rows for new
/// entries, close those no longer there. Returns (opened, closed).
pub async fn record(
    store: &Store,
    origin_id: i64,
    run_id: i64,
    listing: &Listing,
) -> Result<(u64, u64)> {
    let c = &store.conn;
    let mut open: HashMap<(String, String), (i64, i64)> = HashMap::new();
    let mut rows = c
        .query(
            "SELECT presence_id, path, entry, artifact_id FROM presence
             WHERE origin_id = ?1 AND until_run IS NULL",
            (origin_id,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        open.insert((r.get(1)?, r.get(2)?), (r.get(0)?, r.get(3)?));
    }
    drop(rows);
    let mut ins = c
        .prepare(
            "INSERT INTO presence (origin_id, path, entry, artifact_id, since_run)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .await?;
    let mut close = c
        .prepare("UPDATE presence SET until_run = ?2 WHERE presence_id = ?1")
        .await?;
    let (mut opened, mut closed) = (0, 0);
    for (path, entries) in listing {
        for (entry, artifact) in entries {
            match open.remove(&(path.clone(), entry.clone())) {
                Some((_, a)) if a == *artifact => continue,
                Some((id, _)) => {
                    close.execute((id, run_id)).await?;
                    closed += 1;
                }
                None => {}
            }
            ins.execute((origin_id, path.as_str(), entry.as_str(), *artifact, run_id))
                .await?;
            opened += 1;
        }
    }
    for (id, _) in open.into_values() {
        close.execute((id, run_id)).await?;
        closed += 1;
    }
    Ok((opened, closed))
}

/// Record in `removal` what run `run_id` found gone: each closed copy, and
/// where it turned up in the same run, if anywhere. Caller owns the transaction.
pub async fn record_removals(store: &Store, origin_id: i64, run_id: i64) -> Result<u64> {
    let c = &store.conn;
    let mut rows = c
        .query(
            "SELECT max(run_id) FROM import_run
             WHERE origin_id = ?1 AND run_id < ?2 AND finished_at IS NOT NULL",
            (origin_id, run_id),
        )
        .await?;
    let prev: Option<i64> = rows.next().await?.map(|r| r.get(0)).transpose()?.flatten();
    drop(rows);
    let Some(prev) = prev else {
        return Ok(0);
    };
    let n = c
        .execute(
            "INSERT OR IGNORE INTO removal
             (artifact_id, path, last_seen_run, gone_run, moved_to, recorded_at)
             SELECT p.artifact_id, p.path, ?3, ?2,
                    (SELECT min(q.path) FROM presence q
                     WHERE q.origin_id = p.origin_id AND q.artifact_id = p.artifact_id
                       AND q.since_run = ?2 AND q.path <> p.path),
                    ?4
             FROM presence p WHERE p.origin_id = ?1 AND p.until_run = ?2",
            (origin_id, run_id, prev, now()),
        )
        .await?;
    Ok(n)
}

/// Start a run, record `listing` as its presence and its removals, and finish
/// it, in one transaction. Returns the run id.
pub async fn run(store: &Store, origin_id: i64, listing: &Listing) -> Result<i64> {
    store.begin().await?;
    store
        .conn
        .execute(
            "INSERT INTO import_run (origin_id, started_at) VALUES (?1, ?2)",
            (origin_id, now()),
        )
        .await?;
    let run_id = store.conn.last_insert_rowid();
    let (opened, closed) = record(store, origin_id, run_id, listing).await?;
    let removed = record_removals(store, origin_id, run_id).await?;
    store
        .conn
        .execute(
            "UPDATE import_run SET finished_at = ?1 WHERE run_id = ?2",
            (now(), run_id),
        )
        .await?;
    store.commit().await?;
    info!(run_id, opened, closed, removed, "recorded presence");
    Ok(run_id)
}

/// Turn listings recorded whole (formats `gmail-label`, `imap-folder`) into
/// presence, replaying each origin's runs in order, then drop them. Once.
pub async fn convert_listings(store: &Store) -> Result<Option<u64>> {
    let c = &store.conn;
    let done = c
        .query("SELECT 1 FROM meta WHERE key = 'presence'", ())
        .await?
        .next()
        .await?
        .is_some();
    if done {
        return Ok(None);
    }
    store.begin().await?;
    let mut runs: BTreeMap<(i64, i64), Listing> = BTreeMap::new();
    let mut rows = c
        .query(
            "SELECT s.origin_id, s.run_id, s.path, o.from_line, o.artifact_id
             FROM file_sighting s JOIN file_content f ON f.content_id = s.content_id
             JOIN import_run r ON r.run_id = s.run_id
             JOIN observation o ON o.content_id = s.content_id
             WHERE f.format IN ('gmail-label', 'imap-folder') AND r.finished_at IS NOT NULL
             ORDER BY s.origin_id, s.run_id, s.path, o.ordinal",
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        let entry: Vec<u8> = r.get(3)?;
        runs.entry((r.get(0)?, r.get(1)?))
            .or_default()
            .entry(r.get(2)?)
            .or_default()
            .push((String::from_utf8_lossy(&entry).into_owned(), r.get(4)?));
    }
    drop(rows);
    // A run that saw nothing has no sightings but still counts.
    let mut all: Vec<(i64, i64)> = Vec::new();
    let mut rows = c
        .query(
            "SELECT DISTINCT r.origin_id, r.run_id FROM import_run r
             WHERE r.finished_at IS NOT NULL AND r.origin_id IN
               (SELECT s.origin_id FROM file_sighting s JOIN file_content f
                ON f.content_id = s.content_id WHERE f.format IN ('gmail-label', 'imap-folder'))
             ORDER BY 1, 2",
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        all.push((r.get(0)?, r.get(1)?));
    }
    drop(rows);
    let empty = Listing::new();
    let mut rows_written = 0;
    for key in &all {
        let (opened, closed) = record(store, key.0, key.1, runs.get(key).unwrap_or(&empty)).await?;
        rows_written += opened + closed;
    }
    for sql in [
        "DELETE FROM observation WHERE content_id IN
           (SELECT content_id FROM file_content WHERE format IN ('gmail-label', 'imap-folder'))",
        "DELETE FROM file_sighting WHERE content_id IN
           (SELECT content_id FROM file_content WHERE format IN ('gmail-label', 'imap-folder'))",
        "DELETE FROM file_content WHERE format IN ('gmail-label', 'imap-folder')",
        "INSERT INTO meta (key, value) VALUES ('presence', '1')",
    ] {
        c.execute(sql, ()).await?;
    }
    store.commit().await?;
    info!(
        runs = all.len(),
        rows = rows_written,
        "listings converted to presence"
    );
    Ok(Some(rows_written))
}

/// SQL for every place a copy was seen, file sightings and presence alike:
/// (artifact_id, origin_id, path, run_id), where an open presence row counts
/// as seen in its origin's latest finished run, and a closed one in its first.
pub const SEEN: &str = "SELECT o.artifact_id, s.origin_id, s.path, s.run_id
     FROM observation o JOIN file_sighting s ON s.content_id = o.content_id
     UNION ALL
     SELECT p.artifact_id, p.origin_id, p.path,
            CASE WHEN p.until_run IS NULL
                 THEN (SELECT max(r.run_id) FROM import_run r
                       WHERE r.origin_id = p.origin_id AND r.finished_at IS NOT NULL)
                 ELSE p.since_run END
     FROM presence p";
