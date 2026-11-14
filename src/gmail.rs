//! Gmail sync through the API (read-only) into the account. Each message's
//! raw bytes become an artifact; each sync is an import run whose sightings
//! are the labels (as paths), so `--folder`, `--current`, `changes` and
//! removals cover Gmail as they cover mbox folders.
//!
//! The checkpoint (Gmail's history id) advances only in the transaction that
//! stores the run, after every change it covers is committed. A full listing
//! marks messages gone only once it has completed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use data_encoding::BASE64URL_NOPAD;
use serde_json::Value as Json;
use tracing::{info, warn};

use crate::google::Session;
use crate::import::{Artifacts, prepare};
use crate::mbox;
use crate::store::{Store, now};
use crate::takeout::{self, Linker};

/// The origin label every Gmail account's sync uses.
pub const ORIGIN: &str = "gmail";
/// Labels that are state rather than place, left out of sightings.
const NOT_PLACES: [&str; 1] = ["UNREAD"];
/// The path for mail in neither Spam nor Trash, as Gmail's All Mail.
const ALL_MAIL: &str = "All Mail";
const BATCH: usize = 100;

#[derive(Debug, Default)]
pub struct Stats {
    pub full: bool,
    pub fetched: u64,
    pub new_artifacts: u64,
    pub relabelled: u64,
    pub gone: u64,
    /// Takeout copies joined to the messages of newly fetched API copies.
    pub takeout: takeout::Stats,
}

enum Fetched {
    Raw {
        id: String,
        thread: String,
        labels: String,
        internal_date: Option<i64>,
        raw: Vec<u8>,
    },
    Labels {
        id: String,
        labels: String,
    },
    Gone {
        id: String,
    },
}

fn labels_of(v: &Json) -> String {
    let mut l: Vec<&str> = v["labelIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .collect();
    l.sort_unstable();
    l.join(" ")
}

fn fetch(s: &Session, id: &str, raw: bool) -> Result<Fetched> {
    let format = if raw { "raw" } else { "minimal" };
    let Some(v) = s.gmail_opt(&format!("messages/{id}?format={format}"))? else {
        return Ok(Fetched::Gone { id: id.into() });
    };
    let labels = labels_of(&v);
    if !raw {
        return Ok(Fetched::Labels {
            id: id.into(),
            labels,
        });
    }
    let b64 = v["raw"].as_str().context("message has no raw bytes")?;
    let raw = BASE64URL_NOPAD
        .decode(b64.trim_end_matches('=').as_bytes())
        .with_context(|| format!("message {id}: raw is not base64url"))?;
    Ok(Fetched::Raw {
        id: id.into(),
        thread: v["threadId"].as_str().unwrap_or("").into(),
        labels,
        internal_date: v["internalDate"]
            .as_str()
            .and_then(|ms| ms.parse::<i64>().ok())
            .map(|ms| ms / 1000),
        raw,
    })
}

/// Fetch `work` (id, whether raw bytes are needed) on `jobs` threads; results
/// arrive in any order on the returned channel.
fn fetch_all(
    s: Arc<Session>,
    work: Vec<(String, bool)>,
    jobs: usize,
) -> mpsc::Receiver<Result<Fetched>> {
    let queue = Arc::new(Mutex::new(work));
    let (tx, rx) = mpsc::sync_channel(jobs * 4);
    for _ in 0..jobs {
        let (s, queue, tx) = (s.clone(), queue.clone(), tx.clone());
        std::thread::spawn(move || {
            loop {
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).pop();
                let Some((id, raw)) = next else { return };
                if tx.send(fetch(&s, &id, raw)).is_err() {
                    return;
                }
            }
        });
    }
    rx
}

/// Every message id, Spam and Trash included.
fn list_all(s: &Session) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut page = String::new();
    loop {
        let v = s.gmail(&format!(
            "messages?includeSpamTrash=true&maxResults=500{page}"
        ))?;
        for m in v["messages"].as_array().into_iter().flatten() {
            if let Some(id) = m["id"].as_str() {
                ids.push(id.to_string());
            }
        }
        match v["nextPageToken"].as_str() {
            Some(t) => page = format!("&pageToken={t}"),
            None => return Ok(ids),
        }
        if ids.len() % 5000 < 500 {
            info!(listed = ids.len(), "listing Gmail");
        }
    }
}

/// Messages touched since `start` and the history id that covers them; None
/// when Gmail no longer has history that far back.
fn history(s: &Session, start: &str) -> Result<Option<(HashSet<String>, String)>> {
    let mut touched = HashSet::new();
    let mut page = String::new();
    loop {
        let Some(v) = s.gmail_opt(&format!(
            "history?startHistoryId={start}&maxResults=500{page}"
        ))?
        else {
            return Ok(None);
        };
        for h in v["history"].as_array().into_iter().flatten() {
            let records = [
                "messagesAdded",
                "messagesDeleted",
                "labelsAdded",
                "labelsRemoved",
            ]
            .iter()
            .flat_map(|k| h[*k].as_array().into_iter().flatten())
            .map(|e| &e["message"]);
            for m in records.chain(h["messages"].as_array().into_iter().flatten()) {
                if let Some(id) = m["id"].as_str() {
                    touched.insert(id.to_string());
                }
            }
        }
        match v["nextPageToken"].as_str() {
            Some(t) => page = format!("&pageToken={t}"),
            None => {
                let latest = v["historyId"]
                    .as_str()
                    .context("history has no historyId")?;
                return Ok(Some((touched, latest.to_string())));
            }
        }
    }
}

async fn origin(store: &Store, address: &str) -> Result<i64> {
    let root = format!("gmail:{address}");
    let mut rows = store
        .conn
        .query(
            "SELECT origin_id, root FROM origin WHERE label = ?1",
            (ORIGIN,),
        )
        .await?;
    if let Some(r) = rows.next().await? {
        let found: String = r.get(1)?;
        if found != root {
            bail!(
                "origin {ORIGIN:?} is {found}, not {root}; this account's Gmail origin must be {root}"
            );
        }
        return Ok(r.get(0)?);
    }
    drop(rows);
    store.begin().await?;
    store
        .conn
        .execute(
            "INSERT INTO origin (label, root, host, note, created_at, role)
             VALUES (?1, ?2, 'gmail.googleapis.com', 'Gmail API sync', ?3, 'live')",
            (ORIGIN, root.as_str(), now()),
        )
        .await?;
    let id = store.conn.last_insert_rowid();
    store.commit().await?;
    Ok(id)
}

/// Bring the account's Gmail origin up to date: changes since the last
/// checkpoint, or everything if there is none or Gmail's history expired.
/// `takeouts`: origins that are Takeouts of this Gmail, whose copies join new API copies.
pub async fn sync(store: &Store, address: &str, jobs: usize, takeouts: &[i64]) -> Result<Stats> {
    let t = Instant::now();
    let origin_id = origin(store, address).await?;
    let s = Arc::new(Session::open(address)?);
    let labels = s.gmail("labels")?;
    store.begin().await?;
    for l in labels["labels"].as_array().into_iter().flatten() {
        store
            .conn
            .execute(
                "INSERT OR REPLACE INTO gmail_label (origin_id, label_id, name, type) VALUES (?1, ?2, ?3, ?4)",
                (
                    origin_id,
                    l["id"].as_str().unwrap_or(""),
                    l["name"].as_str().unwrap_or(""),
                    l["type"].as_str().unwrap_or(""),
                ),
            )
            .await?;
    }
    store.commit().await?;

    // id -> (labels, gone, checked_at)
    let mut known: HashMap<String, (String, bool, i64)> = HashMap::new();
    let mut rows = store
        .conn
        .query(
            "SELECT gmail_id, labels, gone_at IS NOT NULL, coalesce(checked_at, 0)
             FROM gmail_message WHERE origin_id = ?1",
            (origin_id,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        known.insert(r.get(0)?, (r.get(1)?, r.get::<i64>(2)? != 0, r.get(3)?));
    }
    drop(rows);
    // An interrupted sync's run is continued rather than left unfinished.
    let mut rows = store
        .conn
        .query(
            "SELECT run_id, started_at FROM import_run
             WHERE origin_id = ?1 AND finished_at IS NULL AND run_id >
                   coalesce((SELECT max(run_id) FROM import_run
                             WHERE origin_id = ?1 AND finished_at IS NOT NULL), 0)
             ORDER BY run_id DESC LIMIT 1",
            (origin_id,),
        )
        .await?;
    let resumed: Option<(i64, i64)> = match rows.next().await? {
        Some(r) => Some((r.get(0)?, r.get(1)?)),
        None => None,
    };
    drop(rows);
    let mut rows = store
        .conn
        .query(
            "SELECT history_id FROM gmail_state WHERE origin_id = ?1",
            (origin_id,),
        )
        .await?;
    let checkpoint: Option<String> = rows.next().await?.map(|r| r.get(0)).transpose()?;
    drop(rows);

    let mut stats = Stats::default();
    let incremental = match &checkpoint {
        Some(h) => {
            let found = history(&s, h)?;
            if found.is_none() {
                warn!(
                    history_id = h.as_str(),
                    "Gmail no longer has history that far back; listing everything"
                );
            }
            found
        }
        None => None,
    };
    let (ids, covered, listed) = match incremental {
        Some((touched, latest)) => (touched.into_iter().collect::<Vec<_>>(), latest, None),
        None => {
            stats.full = true;
            let start = s.gmail("profile")?["historyId"]
                .as_str()
                .context("profile has no historyId")?
                .to_string();
            let ids = list_all(&s)?;
            info!(messages = ids.len(), "listed Gmail");
            (ids.clone(), start, Some(ids))
        }
    };
    // A full listing resumed skips what this run already asked Gmail about.
    let checked_since = match resumed {
        Some((_, started)) if stats.full => started,
        _ => i64::MAX,
    };
    let work: Vec<(String, bool)> = ids
        .into_iter()
        .filter(|id| known.get(id).is_none_or(|k| k.2 < checked_since))
        .map(|id| {
            let raw = !known.contains_key(&id);
            (id, raw)
        })
        .collect();
    let total = work.len();
    // Nothing touched since the checkpoint: advance it, and record no run.
    if !stats.full && resumed.is_none() && work.is_empty() {
        store
            .conn
            .execute(
                "UPDATE gmail_state SET history_id = ?2, synced_at = ?3 WHERE origin_id = ?1",
                (origin_id, covered.as_str(), now()),
            )
            .await?;
        return Ok(stats);
    }

    let run_id = match resumed {
        Some((run, _)) => {
            info!(run, "continuing an interrupted sync");
            run
        }
        None => {
            store.begin().await?;
            store
                .conn
                .execute(
                    "INSERT INTO import_run (origin_id, started_at) VALUES (?1, ?2)",
                    (origin_id, now()),
                )
                .await?;
            let run = store.conn.last_insert_rowid();
            store.commit().await?;
            run
        }
    };

    let mut art = Artifacts::new(store).await?;
    let ins = store
        .conn
        .prepare(
            "INSERT OR REPLACE INTO gmail_message
             (gmail_id, origin_id, artifact_id, thread_id, internal_date, labels, gone_at, checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
        )
        .await?;
    let relabel = store
        .conn
        .prepare(
            "UPDATE gmail_message SET labels = ?2, gone_at = NULL, checked_at = ?3
             WHERE gmail_id = ?1",
        )
        .await?;
    let gone = store
        .conn
        .prepare(
            "UPDATE gmail_message SET gone_at = coalesce(gone_at, ?2), checked_at = ?2
             WHERE gmail_id = ?1",
        )
        .await?;
    let history = store
        .conn
        .prepare(
            "INSERT INTO gmail_labels (origin_id, gmail_id, labels, seen_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .await?;
    let (mut ins, mut relabel, mut gone, mut history) = (ins, relabel, gone, history);
    let mut linker = Linker::new(store).await?;
    let mut fresh = Vec::new();
    let rx = fetch_all(s.clone(), work, jobs);
    store.begin().await?;
    for (n, got) in rx.iter().enumerate() {
        match got? {
            Fetched::Raw {
                id,
                thread,
                labels,
                internal_date,
                raw,
            } => {
                let (h, b) = mbox::split_message(&raw);
                let p = prepare(&raw[h.clone()], &raw[b.clone()]);
                let (artifact, new) = art.put(store, &raw[h], &raw[b], &p).await?;
                ins.execute((
                    id.as_str(),
                    origin_id,
                    artifact,
                    thread.as_str(),
                    internal_date,
                    labels.as_str(),
                    now(),
                ))
                .await?;
                if known.get(&id).is_none_or(|k| k.0 != labels) {
                    history
                        .execute((origin_id, id.as_str(), labels.as_str(), now()))
                        .await?;
                }
                stats.fetched += 1;
                stats.new_artifacts += new as u64;
                if new {
                    fresh.push(artifact);
                }
            }
            Fetched::Labels { id, labels } => {
                relabel
                    .execute((id.as_str(), labels.as_str(), now()))
                    .await?;
                if known.get(&id).is_some_and(|k| k.0 != labels) {
                    history
                        .execute((origin_id, id.as_str(), labels.as_str(), now()))
                        .await?;
                }
                stats.relabelled += known.get(&id).is_some_and(|k| k.0 != labels || k.1) as u64;
            }
            Fetched::Gone { id } => {
                gone.execute((id.as_str(), now())).await?;
                stats.gone += known.get(&id).is_some_and(|k| !k.1) as u64;
            }
        }
        if (n + 1) % BATCH == 0 {
            stats.takeout.add(linker.from_api(takeouts, &fresh).await?);
            fresh.clear();
            store.commit().await?;
            info!(done = n + 1, of = total, fetched = stats.fetched, elapsed = ?t.elapsed(), "gmail sync");
            store.begin().await?;
        }
    }
    stats.takeout.add(linker.from_api(takeouts, &fresh).await?);
    if let Some(ids) = listed {
        store
            .conn
            .execute(
                "CREATE TEMP TABLE IF NOT EXISTS listed (gmail_id TEXT PRIMARY KEY)",
                (),
            )
            .await?;
        store.conn.execute("DELETE FROM listed", ()).await?;
        let mut put = store
            .conn
            .prepare("INSERT OR IGNORE INTO listed VALUES (?1)")
            .await?;
        for id in &ids {
            put.execute((id.as_str(),)).await?;
        }
        stats.gone += store
            .conn
            .execute(
                "UPDATE gmail_message SET gone_at = ?2
                 WHERE origin_id = ?1 AND gone_at IS NULL
                   AND gmail_id NOT IN (SELECT gmail_id FROM listed)",
                (origin_id, now()),
            )
            .await?;
    }
    snapshot(store, origin_id, run_id).await?;
    crate::presence::record_removals(store, origin_id, run_id).await?;
    store
        .conn
        .execute(
            "INSERT OR REPLACE INTO gmail_state (origin_id, history_id, run_id, synced_at)
             VALUES (?1, ?2, ?3, ?4)",
            (origin_id, covered.as_str(), run_id, now()),
        )
        .await?;
    store
        .conn
        .execute(
            "UPDATE import_run SET finished_at = ?1 WHERE run_id = ?2",
            (now(), run_id),
        )
        .await?;
    store.commit().await?;
    Ok(stats)
}

/// Record run `run_id`'s presence: each label's messages as of the stored
/// state; only entries that came or went are written.
async fn snapshot(store: &Store, origin_id: i64, run_id: i64) -> Result<()> {
    let mut names = HashMap::new();
    let mut rows = store
        .conn
        .query(
            "SELECT label_id, name FROM gmail_label WHERE origin_id = ?1",
            (origin_id,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        names.insert(r.get::<String>(0)?, r.get::<String>(1)?);
    }
    drop(rows);
    let mut paths = crate::presence::Listing::new();
    let mut rows = store
        .conn
        .query(
            "SELECT gmail_id, artifact_id, labels FROM gmail_message
             WHERE origin_id = ?1 AND gone_at IS NULL ORDER BY internal_date, gmail_id",
            (origin_id,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        let (id, artifact, labels): (String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let labels: Vec<&str> = labels.split(' ').filter(|l| !l.is_empty()).collect();
        let mut places: Vec<String> = labels
            .iter()
            .filter(|l| !NOT_PLACES.contains(l))
            .map(|l| names.get(*l).cloned().unwrap_or_else(|| l.to_string()))
            .collect();
        if !labels.contains(&"SPAM") && !labels.contains(&"TRASH") {
            places.push(ALL_MAIL.into());
        }
        for p in places {
            paths.entry(p).or_default().push((id.clone(), artifact));
        }
    }
    drop(rows);
    crate::presence::record(store, origin_id, run_id, &paths).await?;
    Ok(())
}

/// Start each Gmail message's label history from its labels now, for messages
/// stored before history was kept. Once.
pub async fn start_label_history(store: &Store) -> Result<Option<u64>> {
    let c = &store.conn;
    let done = c
        .query("SELECT 1 FROM meta WHERE key = 'gmail_label_history'", ())
        .await?
        .next()
        .await?
        .is_some();
    if done {
        return Ok(None);
    }
    store.begin().await?;
    let n = c
        .execute(
            "INSERT INTO gmail_labels (origin_id, gmail_id, labels, seen_at)
             SELECT m.origin_id, m.gmail_id, m.labels, coalesce(m.checked_at, m.internal_date, 0)
             FROM gmail_message m
             WHERE NOT EXISTS (SELECT 1 FROM gmail_labels l
                               WHERE l.origin_id = m.origin_id AND l.gmail_id = m.gmail_id)",
            (),
        )
        .await?;
    c.execute(
        "INSERT INTO meta (key, value) VALUES ('gmail_label_history', '1')",
        (),
    )
    .await?;
    store.commit().await?;
    Ok(Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each label's entries open at `run`, and how many presence rows exist in all.
    async fn sightings(store: &Store, run: i64) -> (Vec<(String, i64)>, i64) {
        let mut rows = store
            .conn
            .query(
                "SELECT path, count(*) FROM presence
                 WHERE since_run <= ?1 AND (until_run IS NULL OR until_run > ?1)
                 GROUP BY path ORDER BY path",
                (run,),
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(r) = rows.next().await.unwrap() {
            out.push((r.get(0).unwrap(), r.get(1).unwrap()));
        }
        drop(rows);
        let mut rows = store
            .conn
            .query("SELECT count(*) FROM presence", ())
            .await
            .unwrap();
        let all = rows.next().await.unwrap().unwrap().get(0).unwrap();
        (out, all)
    }

    #[test]
    fn labels_become_paths() {
        let dir = std::env::temp_dir().join(format!("munchymail-gmail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        pollster::block_on(async {
            let store = Store::open(&dir).await.unwrap();
            let c = &store.conn;
            for sql in [
                "INSERT INTO gmail_label VALUES (1, 'INBOX', 'INBOX', 'system')",
                "INSERT INTO gmail_label VALUES (1, 'Label_7', 'Receipts', 'user')",
                "INSERT INTO gmail_message VALUES ('a', 1, 10, 't', 1, 'INBOX UNREAD', NULL, 0)",
                "INSERT INTO gmail_message VALUES ('b', 1, 11, 't', 2, 'Label_7', NULL, 0)",
                "INSERT INTO gmail_message VALUES ('c', 1, 12, 't', 3, 'SPAM', NULL, 0)",
            ] {
                c.execute(sql, ()).await.unwrap();
            }
            assert_eq!(start_label_history(&store).await.unwrap(), Some(3));
            assert_eq!(start_label_history(&store).await.unwrap(), None);
            snapshot(&store, 1, 1).await.unwrap();
            let (one, rows) = sightings(&store, 1).await;
            let want = |v: &[(&str, i64)]| {
                v.iter()
                    .map(|(p, n)| (p.to_string(), *n))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                one,
                want(&[("All Mail", 2), ("INBOX", 1), ("Receipts", 1), ("SPAM", 1)])
            );
            assert_eq!(rows, 5);
            c.execute(
                "UPDATE gmail_message SET gone_at = 5 WHERE gmail_id = 'b'",
                (),
            )
            .await
            .unwrap();
            snapshot(&store, 1, 2).await.unwrap();
            let (two, rows) = sightings(&store, 2).await;
            assert_eq!(two, want(&[("All Mail", 1), ("INBOX", 1), ("SPAM", 1)]));
            // Only what changed is written: two rows closed, none added.
            assert_eq!(rows, 5);
            let (still, _) = sightings(&store, 1).await;
            assert_eq!(still, one, "run 1 is still answerable");
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
