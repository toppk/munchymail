//! The `part` index: every MIME leaf part of every message, with its declared
//! and detected type, and the file names inside zips. Derived from stored
//! bytes, so it can be rebuilt; `VERSION` says which messages are stale.

use std::time::Instant;

use anyhow::{Result, bail};
use rayon::prelude::*;
use tracing::info;
use turso::{Statement, Value};

use crate::message::{self, Part};
use crate::store::Store;

/// Bump when classification changes, so `index` redoes every message.
pub const VERSION: i64 = 1;

const BATCH: usize = 500;

#[derive(Debug, Default)]
pub struct Stats {
    pub messages: u64,
    pub parts: u64,
    pub problems: u64,
}

/// Messages whose parts are missing or from an older `VERSION`.
pub async fn pending(store: &Store) -> Result<i64> {
    let mut rows = store
        .conn
        .query(
            "SELECT count(*) FROM message WHERE parts_version IS NOT ?1",
            (VERSION,),
        )
        .await?;
    Ok(rows.next().await?.map_or(Ok(0), |r| r.get(0))?)
}

fn blob(v: Value) -> Result<Vec<u8>> {
    match v {
        Value::Blob(b) => Ok(b),
        v => bail!("expected a hash, got {v:?}"),
    }
}

/// One stored copy's whole bytes, body bytes and Subject, by `sel`:
/// `SELECT header_hash, body_hash, subject FROM artifact WHERE artifact_id = ?1`.
pub(crate) async fn stored(
    store: &Store,
    sel: &mut Statement,
    id: i64,
) -> Result<(Vec<u8>, Vec<u8>, Option<String>)> {
    let mut rows = sel.query((id,)).await?;
    let r = rows.next().await?.expect("artifact listed above");
    let (h, b) = (blob(r.get_value(0)?)?, blob(r.get_value(1)?)?);
    let subject = r.get(2)?;
    drop(rows);
    let mut bytes = store.object(&h).await?;
    let body = store.object(&b).await?;
    bytes.extend_from_slice(&body);
    Ok((bytes, body, subject))
}

/// Fill `part` and `archive_member` for every pending message, one
/// transaction per batch, decoding one stored copy of each.
pub async fn index(store: &Store) -> Result<Stats> {
    let t = Instant::now();
    let mut todo = Vec::new();
    let mut rows = store
        .conn
        .query(
            "SELECT a.msg_id, min(a.artifact_id) FROM artifact a
             JOIN message m ON m.msg_id = a.msg_id
             WHERE m.parts_version IS NOT ?1 GROUP BY a.msg_id",
            (VERSION,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        todo.push((r.get::<i64>(0)?, r.get::<i64>(1)?));
    }
    drop(rows);
    let c = &store.conn;
    let mut sel = c
        .prepare("SELECT header_hash, body_hash, subject FROM artifact WHERE artifact_id = ?1")
        .await?;
    let mut del = c.prepare("DELETE FROM part WHERE msg_id = ?1").await?;
    let mut ins = c
        .prepare(
            "INSERT INTO part (msg_id, ordinal, kind, filename, ext, declared_type, detected_type, size, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .await?;
    let mut ins_member = c
        .prepare("INSERT OR IGNORE INTO archive_member (hash, ordinal, name, size) VALUES (?1, ?2, ?3, ?4)")
        .await?;
    let mut done = c
        .prepare("UPDATE message SET parts_version = ?1, parts_problem = ?2 WHERE msg_id = ?3")
        .await?;
    let mut stats = Stats::default();
    for batch in todo.chunks(BATCH) {
        let mut raw = Vec::with_capacity(batch.len());
        for &(msg, id) in batch {
            let (bytes, body, _) = stored(store, &mut sel, id).await?;
            raw.push((msg, bytes, body));
        }
        let decoded: Vec<(i64, Vec<Part>, Option<String>)> = raw
            .par_iter()
            .map(|(msg, bytes, body)| {
                let d = message::decode(bytes, body);
                (*msg, d.parts, d.problem)
            })
            .collect();
        store.begin().await?;
        for (msg, parts, problem) in &decoded {
            del.execute((*msg,)).await?;
            for (i, p) in parts.iter().enumerate() {
                ins.execute((
                    *msg,
                    i as i64,
                    p.kind.as_str(),
                    p.filename.clone(),
                    p.ext(),
                    p.content_type.clone(),
                    p.detected,
                    p.size as i64,
                    p.sha256.to_vec(),
                ))
                .await?;
                for (j, (name, size)) in p.members.iter().enumerate() {
                    ins_member
                        .execute((p.sha256.to_vec(), j as i64, name.as_str(), *size as i64))
                        .await?;
                }
            }
            done.execute((VERSION, problem.clone(), *msg)).await?;
            stats.parts += parts.len() as u64;
            stats.problems += problem.is_some() as u64;
        }
        store.commit().await?;
        stats.messages += batch.len() as u64;
        if stats.messages % 20_000 < BATCH as u64 {
            info!(done = stats.messages, of = todo.len(), elapsed = ?t.elapsed(), "indexing parts");
        }
    }
    Ok(stats)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Key {
    /// Detected type, else declared.
    Type,
    Declared,
    Detected,
    Ext,
    /// "declared -> detected", only parts where they differ.
    Mismatch,
}

pub struct Group {
    pub key: String,
    pub parts: u64,
    pub messages: u64,
    pub bytes: u64,
}

/// Parts of the messages in `set` (a table of artifact ids, or `artifact`
/// for all), grouped by `key`, most parts first. Body parts only with `all`.
pub async fn report(store: &Store, set: &str, key: Key, all: bool) -> Result<Vec<Group>> {
    use std::collections::{HashMap, HashSet};
    let mut rows = store
        .conn
        .query(
            &format!("SELECT DISTINCT a.msg_id FROM {set} t JOIN artifact a ON a.artifact_id = t.artifact_id"),
            (),
        )
        .await?;
    let mut msgs = HashSet::new();
    while let Some(r) = rows.next().await? {
        msgs.insert(r.get::<i64>(0)?);
    }
    drop(rows);
    let mut rows = store
        .conn
        .query(
            "SELECT msg_id, kind, ext, declared_type, detected_type, size FROM part",
            (),
        )
        .await?;
    let mut by: HashMap<String, (u64, HashSet<i64>, u64)> = HashMap::new();
    while let Some(r) = rows.next().await? {
        let msg: i64 = r.get(0)?;
        if !msgs.contains(&msg) || (!all && r.get::<String>(1)? == "body") {
            continue;
        }
        let (ext, declared, detected) = (
            r.get::<Option<String>>(2)?,
            r.get::<String>(3)?,
            r.get::<Option<String>>(4)?,
        );
        let k = match key {
            Key::Type => detected.unwrap_or(declared),
            Key::Declared => declared,
            Key::Detected => detected.unwrap_or_else(|| "(not recognised)".into()),
            Key::Ext => ext.unwrap_or_else(|| "(none)".into()),
            Key::Mismatch => match detected {
                Some(d) if d != declared => format!("{declared} -> {d}"),
                _ => continue,
            },
        };
        let g = by.entry(k).or_default();
        g.0 += 1;
        g.1.insert(msg);
        g.2 += r.get::<i64>(5)? as u64;
    }
    let mut out: Vec<Group> = by
        .into_iter()
        .map(|(key, (parts, m, bytes))| Group {
            key,
            parts,
            messages: m.len() as u64,
            bytes,
        })
        .collect();
    out.sort_by(|a, b| b.parts.cmp(&a.parts).then_with(|| a.key.cmp(&b.key)));
    Ok(out)
}
