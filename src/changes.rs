//! Message-level changes between import runs, read from the index, and recorded removals.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use anyhow::{Context, Result};
use tracing::info;
use turso::Value;

use crate::diff::{self, Msg};
use crate::headers::Field;
use crate::store::{Store, now};

/// One copy of a message at a path in a snapshot.
pub struct Copy {
    pub run_id: i64,
    /// Path as recorded in its run, relative to that origin's root.
    pub recorded_path: String,
    /// Path after renames; locations pair up by this.
    pub path: String,
    pub artifact_id: i64,
}

/// The messages of a set of runs in files that differ from the other side.
pub struct Snapshot {
    pub runs: Vec<i64>,
    /// Sorted by path; `msgs[i]` describes `copies[i]`.
    pub copies: Vec<Copy>,
    pub msgs: Vec<Msg>,
}

impl Snapshot {
    fn ranges(&self) -> BTreeMap<&str, Range<usize>> {
        let mut out: BTreeMap<&str, Range<usize>> = BTreeMap::new();
        for (i, c) in self.copies.iter().enumerate() {
            out.entry(c.path.as_str())
                .and_modify(|r| r.end = i + 1)
                .or_insert(i..i + 1);
        }
        out
    }
}

pub struct Changes {
    pub before: Snapshot,
    pub after: Snapshot,
    /// Files at the same path with identical content on both sides; not loaded.
    pub unchanged_files: usize,
    pub unchanged_file_messages: u64,
    /// Identical copies at the same path within changed files.
    pub unchanged: usize,
    /// (before, after) at the same path with the same body and Message-ID but new header bytes.
    pub changed: Vec<(usize, usize)>,
    /// (before, after) gone from their path and found at another path new to it.
    pub moved: Vec<(usize, usize)>,
    /// Before copies found nowhere new in the after snapshot.
    pub removed: Vec<usize>,
    /// After copies that are new at their path and came from no removed copy.
    pub added: Vec<usize>,
}

struct File {
    run_id: i64,
    recorded_path: String,
    /// An mbox file's content; 0 for a live listing, whose entries are `presence` rows.
    content_id: i64,
    presence: Vec<i64>,
    messages: i64,
}

/// A live listing's entries at run `?1`: presence rows `p` open then.
const AT_RUN: &str = "p.origin_id = (SELECT origin_id FROM import_run WHERE run_id = ?1)
     AND p.since_run <= ?1 AND (p.until_run IS NULL OR p.until_run > ?1)";

async fn files(
    store: &Store,
    runs: &[i64],
    rename: &[(String, String)],
) -> Result<BTreeMap<String, Vec<File>>> {
    let mut stmt = store
        .conn
        .prepare(
            "SELECT s.path, s.content_id, c.message_count FROM file_sighting s
             JOIN file_content c ON c.content_id = s.content_id WHERE s.run_id = ?1",
        )
        .await?;
    let mut live = store
        .conn
        .prepare(&format!(
            "SELECT p.path, p.presence_id FROM presence p WHERE {AT_RUN}
             ORDER BY p.path, p.presence_id"
        ))
        .await?;
    let mut exists = store
        .conn
        .prepare("SELECT 1 FROM import_run WHERE run_id = ?1")
        .await?;
    let renamed = |recorded: &String| {
        rename
            .iter()
            .find(|(old, _)| old == recorded)
            .map_or_else(|| recorded.clone(), |(_, new)| new.clone())
    };
    let mut out: BTreeMap<String, Vec<File>> = BTreeMap::new();
    for &run_id in runs {
        let found = exists.query((run_id,)).await?.next().await?.is_some();
        anyhow::ensure!(
            found,
            "no run {run_id}; `mmadm origin list` shows origins and their runs"
        );
        let mut rows = stmt.query((run_id,)).await?;
        while let Some(r) = rows.next().await? {
            let recorded_path: String = r.get(0)?;
            out.entry(renamed(&recorded_path)).or_default().push(File {
                run_id,
                recorded_path,
                content_id: r.get(1)?,
                presence: Vec::new(),
                messages: r.get(2)?,
            });
        }
        drop(rows);
        let mut listing: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        let mut rows = live.query((run_id,)).await?;
        while let Some(r) = rows.next().await? {
            listing.entry(r.get(0)?).or_default().push(r.get(1)?);
        }
        for (recorded_path, presence) in listing {
            out.entry(renamed(&recorded_path)).or_default().push(File {
                run_id,
                messages: presence.len() as i64,
                recorded_path,
                content_id: 0,
                presence,
            });
        }
    }
    Ok(out)
}

fn hash(v: &Value) -> Result<[u8; 32]> {
    match v {
        Value::Blob(b) => b.as_slice().try_into().context("hash is not 32 bytes"),
        _ => anyhow::bail!("hash is not a blob"),
    }
}

async fn load(
    store: &Store,
    runs: &[i64],
    files: &BTreeMap<String, Vec<File>>,
    keep: &BTreeSet<&str>,
) -> Result<Snapshot> {
    let mut stmt = store
        .conn
        .prepare(
            "SELECT o.artifact_id, a.header_hash, a.body_hash, a.message_id
             FROM observation o JOIN artifact a ON a.artifact_id = o.artifact_id
             WHERE o.content_id = ?1 ORDER BY o.ordinal",
        )
        .await?;
    let mut live = store
        .conn
        .prepare(&format!(
            "SELECT p.artifact_id, a.header_hash, a.body_hash, a.message_id
             FROM presence p JOIN artifact a ON a.artifact_id = p.artifact_id
             WHERE {AT_RUN} AND p.path = ?2 ORDER BY p.presence_id"
        ))
        .await?;
    let (mut copies, mut msgs) = (Vec::new(), Vec::new());
    for (path, fs) in files.iter().filter(|(p, _)| keep.contains(p.as_str())) {
        for f in fs {
            let mut rows = if f.content_id == 0 {
                live.query((f.run_id, f.recorded_path.as_str())).await?
            } else {
                stmt.query((f.content_id,)).await?
            };
            while let Some(r) = rows.next().await? {
                copies.push(Copy {
                    run_id: f.run_id,
                    recorded_path: f.recorded_path.clone(),
                    path: path.clone(),
                    artifact_id: r.get(0)?,
                });
                msgs.push(Msg {
                    header_hash: hash(&r.get_value(1)?)?,
                    body_hash: hash(&r.get_value(2)?)?,
                    message_id: r.get(3)?,
                    fields: Vec::new(),
                });
            }
        }
    }
    Ok(Snapshot {
        runs: runs.to_vec(),
        copies,
        msgs,
    })
}

/// What a path held: its file contents, or its live entries.
fn contents(fs: Option<&Vec<File>>) -> Vec<i64> {
    let mut v: Vec<i64> = fs.map_or(Vec::new(), |fs| {
        fs.iter()
            .flat_map(|f| {
                (f.content_id != 0)
                    .then_some(f.content_id)
                    .into_iter()
                    .chain(f.presence.iter().map(|p| -p))
            })
            .collect()
    });
    v.sort_unstable();
    v
}

/// Pair every message of `before_runs` with `after_runs`. Each side may span
/// several origins; `rename` maps a recorded path to the path it is compared as.
pub async fn between(
    store: &Store,
    before_runs: &[i64],
    after_runs: &[i64],
    rename: &[(String, String)],
) -> Result<Changes> {
    let bf = files(store, before_runs, rename).await?;
    let af = files(store, after_runs, rename).await?;
    let (mut unchanged_files, mut unchanged_file_messages) = (0, 0);
    let mut keep = BTreeSet::new();
    let paths: BTreeSet<&String> = bf.keys().chain(af.keys()).collect();
    for path in paths {
        let (b, a) = (bf.get(path), af.get(path));
        if contents(b) == contents(a) {
            if let Some(b) = b {
                unchanged_files += b.len();
                unchanged_file_messages += b.iter().map(|f| f.messages as u64).sum::<u64>();
            }
        } else {
            keep.insert(path.as_str());
        }
    }
    let before = load(store, before_runs, &bf, &keep).await?;
    let after = load(store, after_runs, &af, &keep).await?;

    let (br, ar) = (before.ranges(), after.ranges());
    let mut unchanged = 0;
    let (mut changed, mut lost, mut new) = (Vec::new(), Vec::new(), Vec::new());
    for path in &keep {
        let rb = br.get(path).cloned().unwrap_or(0..0);
        let ra = ar.get(path).cloned().unwrap_or(0..0);
        let d = diff::compare(&before.msgs[rb.clone()], &after.msgs[ra.clone()], &[]);
        unchanged += d.unchanged;
        changed.extend(d.changed.iter().map(|&(i, j)| (rb.start + i, ra.start + j)));
        lost.extend(d.lost.iter().map(|&i| rb.start + i));
        new.extend(d.added.iter().map(|&j| ra.start + j));
    }
    // A copy gone from one path counts as moved only to a path where that message is new.
    let mut pool: HashMap<([u8; 32], Option<String>), Vec<usize>> = HashMap::new();
    for &j in new.iter().rev() {
        pool.entry(after.msgs[j].same_message_key())
            .or_default()
            .push(j);
    }
    let (mut moved, mut removed) = (Vec::new(), Vec::new());
    for i in lost {
        match pool
            .get_mut(&before.msgs[i].same_message_key())
            .and_then(Vec::pop)
        {
            Some(j) => moved.push((i, j)),
            None => removed.push(i),
        }
    }
    let mut added: Vec<usize> = pool.into_values().flatten().collect();
    added.sort_unstable();
    Ok(Changes {
        before,
        after,
        unchanged_files,
        unchanged_file_messages,
        unchanged,
        changed,
        moved,
        removed,
        added,
    })
}

/// Load decoded headers for the given copies, for report keys and listings.
pub async fn fill_fields(store: &Store, snap: &mut Snapshot, idxs: &[usize]) -> Result<()> {
    let mut stmt = store
        .conn
        .prepare("SELECT name, value FROM header WHERE artifact_id = ?1 ORDER BY ordinal")
        .await?;
    for &i in idxs {
        if !snap.msgs[i].fields.is_empty() {
            continue;
        }
        let mut rows = stmt.query((snap.copies[i].artifact_id,)).await?;
        while let Some(r) = rows.next().await? {
            snap.msgs[i].fields.push(Field {
                name: r.get(0)?,
                value: r.get(1)?,
            });
        }
    }
    Ok(())
}

/// Store removed and moved copies in `removal`; already-recorded ones are skipped.
/// Returns how many rows were new. The caller owns the transaction.
pub async fn record(store: &Store, c: &Changes) -> Result<u64> {
    let mut stmt = store
        .conn
        .prepare(
            "INSERT OR IGNORE INTO removal
             (artifact_id, path, last_seen_run, gone_run, moved_to, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .await?;
    let last_after = *c.after.runs.iter().max().context("no after runs")?;
    let t = now();
    let mut n = 0;
    let rows = c
        .removed
        .iter()
        .map(|&i| (i, None))
        .chain(c.moved.iter().map(|&(i, j)| (i, Some(&c.after.copies[j]))));
    for (i, dest) in rows {
        let from = &c.before.copies[i];
        n += stmt
            .execute((
                from.artifact_id,
                from.recorded_path.as_str(),
                from.run_id,
                dest.map_or(last_after, |d| d.run_id),
                dest.map(|d| d.recorded_path.clone()),
                t,
            ))
            .await?;
    }
    Ok(n)
}

/// After an import, record what left each path since the origin's previous finished run.
pub async fn record_since_previous(store: &Store, origin_id: i64, run_id: i64) -> Result<()> {
    let mut rows = store
        .conn
        .query(
            "SELECT max(run_id) FROM import_run
             WHERE origin_id = ?1 AND run_id < ?2 AND finished_at IS NOT NULL",
            (origin_id, run_id),
        )
        .await?;
    let prev: Option<i64> = rows.next().await?.map(|r| r.get(0)).transpose()?.flatten();
    drop(rows);
    let Some(prev) = prev else {
        return Ok(());
    };
    let c = between(store, &[prev], &[run_id], &[]).await?;
    store.begin().await?;
    let n = record(store, &c).await?;
    store.commit().await?;
    info!(
        since_run = prev,
        removed = c.removed.len(),
        moved = c.moved.len(),
        headers_changed = c.changed.len(),
        added = c.added.len(),
        recorded = n,
        "changes since previous run"
    );
    Ok(())
}
