//! Regroup stored copies into messages under the current key rule.
//!
//! Every copy that isn't folded is re-keyed; copies with one key are one
//! message. An old message's id goes with its preferred copy (not folded, then
//! the earliest stored). Where one group holds the preferred copies of several
//! old messages, the lowest id survives and the others record `merged_into`
//! it; a group holding none is new, with `split_from`. Messages are never
//! deleted, so no id is reused. All writes are one transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use tracing::info;

use crate::key::{self, Hash, Key};
use crate::store::Store;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Messages whose stored key changed.
    pub rekeyed: u64,
    /// Messages begun by a split.
    pub split: Vec<(i64, i64)>,
    /// (merged, into)
    pub merged: Vec<(i64, i64)>,
    /// Copies now in another message.
    pub moved: u64,
    /// Messages to index again: their copies or preferred copy changed.
    pub reindexed: u64,
}

impl Stats {
    pub fn changed(&self) -> bool {
        *self != Stats::default()
    }
}

struct Copy {
    id: i64,
    msg: i64,
    header: Vec<u8>,
    body: Vec<u8>,
    date: Option<i64>,
    copy_of: Option<i64>,
}

/// Each message's preferred copy (not folded, then the earliest stored), with
/// `msg` saying which message a copy is in; `copies` are in id order.
fn preferred_copies(copies: &[Copy], msg: impl Fn(&Copy) -> i64) -> BTreeMap<i64, i64> {
    let mut out: BTreeMap<i64, (i64, bool)> = BTreeMap::new();
    for cp in copies {
        let e = out.entry(msg(cp)).or_insert((cp.id, cp.copy_of.is_none()));
        if !e.1 && cp.copy_of.is_none() {
            *e = (cp.id, true);
        }
    }
    out.into_iter().map(|(m, (a, _))| (m, a)).collect()
}

/// Regroup once per rule (`meta` `identity`), before anything is stored or
/// indexed under it.
pub async fn rekey(store: &Store) -> Result<Option<Stats>> {
    let current = store
        .conn
        .query("SELECT value FROM meta WHERE key = 'identity'", ())
        .await?
        .next()
        .await?
        .map(|r| r.get::<String>(0))
        .transpose()?;
    if current.as_deref() == Some(key::RULE) {
        return Ok(None);
    }
    store.begin().await?;
    match regroup_in(store).await {
        Ok(stats) => {
            store
                .conn
                .execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('identity', ?1)",
                    (key::RULE,),
                )
                .await?;
            store.commit().await?;
            Ok(Some(stats))
        }
        Err(e) => {
            store.rollback().await?;
            Err(e)
        }
    }
}

/// Regroup now, whatever the recorded rule; a second run changes nothing.
pub async fn regroup(store: &Store) -> Result<Stats> {
    store.begin().await?;
    match regroup_in(store).await {
        Ok(stats) => {
            store.commit().await?;
            Ok(stats)
        }
        Err(e) => {
            store.rollback().await?;
            Err(e)
        }
    }
}

/// The regrouping, inside the caller's transaction.
pub async fn regroup_in(store: &Store) -> Result<Stats> {
    let c = &store.conn;
    let mut copies = Vec::new();
    let mut rows = c
        .query(
            "SELECT a.artifact_id, a.msg_id, a.header_hash, a.body_hash, a.date, f.copy_of
             FROM artifact a LEFT JOIN fold f ON f.artifact_id = a.artifact_id
             WHERE a.msg_id IS NOT NULL ORDER BY a.artifact_id",
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        copies.push(Copy {
            id: r.get(0)?,
            msg: r.get(1)?,
            header: r.get(2)?,
            body: r.get(3)?,
            date: r.get(4)?,
            copy_of: r.get(5)?,
        });
    }
    drop(rows);
    let mut old: HashMap<i64, Option<Vec<u8>>> = HashMap::new();
    let mut rows = c
        .query(
            "SELECT msg_id, key FROM message WHERE merged_into IS NULL",
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        old.insert(r.get(0)?, r.get(1)?);
    }
    drop(rows);

    // Keys first: nothing is compared with a key under the old rule.
    let t = std::time::Instant::now();
    let mut keys: HashMap<i64, Key> = HashMap::new();
    let mut identities: HashMap<Vec<u8>, Hash> = HashMap::new();
    for (n, cp) in copies.iter().enumerate() {
        if cp.copy_of.is_some() {
            continue;
        }
        let header = store.object(&cp.header).await?;
        let identity = match identities.get(&cp.body) {
            Some(i) => *i,
            None => {
                let i = key::identity(&store.object(&cp.body).await?);
                identities.insert(cp.body.clone(), i);
                i
            }
        };
        keys.insert(cp.id, key::key_with(&header, identity, cp.date));
        if (n + 1) % 50_000 == 0 {
            info!(done = n + 1, of = copies.len(), elapsed = ?t.elapsed(), "re-keying copies");
        }
    }

    let preferred = preferred_copies(&copies, |cp| cp.msg);
    let claims: HashMap<i64, i64> = preferred.iter().map(|(&m, &a)| (a, m)).collect();

    // Groups by key, in order of their earliest copy.
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut first: HashMap<Hash, i64> = HashMap::new();
    for cp in &copies {
        if let Some(k) = keys.get(&cp.id) {
            let g = *first.entry(k.hash).or_insert(cp.id);
            groups.entry(g).or_default().push(cp.id);
        }
    }
    let msg_of: HashMap<i64, i64> = copies.iter().map(|cp| (cp.id, cp.msg)).collect();

    let mut stats = Stats::default();
    let mut target: HashMap<i64, i64> = HashMap::new();
    let mut touched: BTreeSet<i64> = BTreeSet::new();
    let mut ins_new = c
        .prepare("INSERT INTO message (key, body_hash, message_id, date, split_from) VALUES (?1, ?2, ?3, ?4, ?5)")
        .await?;
    let mut set_key = c
        .prepare("UPDATE message SET key = ?2, body_hash = ?3, message_id = ?4, date = ?5 WHERE msg_id = ?1")
        .await?;
    for members in groups.values() {
        let k = &keys[&members[0]];
        let date = copies[copies.partition_point(|cp| cp.id < members[0])].date;
        let claimants: BTreeSet<i64> = members
            .iter()
            .filter_map(|a| claims.get(a))
            .copied()
            .collect();
        let msg = match claimants.first() {
            Some(&survivor) => {
                for &m in claimants.iter().skip(1) {
                    stats.merged.push((m, survivor));
                }
                if old.get(&survivor).and_then(|key| key.as_deref()) != Some(&k.hash[..]) {
                    set_key
                        .execute((
                            survivor,
                            k.hash.to_vec(),
                            k.identity.to_vec(),
                            k.message_id.clone(),
                            date,
                        ))
                        .await?;
                    stats.rekeyed += 1;
                }
                survivor
            }
            None => {
                let mut from: BTreeMap<i64, usize> = BTreeMap::new();
                for a in members {
                    *from.entry(msg_of[a]).or_default() += 1;
                }
                let most = from.values().max().copied().unwrap_or(0);
                let origin = from.iter().find(|(_, n)| **n == most).map(|(m, _)| *m);
                ins_new
                    .execute((
                        k.hash.to_vec(),
                        k.identity.to_vec(),
                        k.message_id.clone(),
                        date,
                        origin,
                    ))
                    .await?;
                let id = c.last_insert_rowid();
                stats.split.push((id, origin.unwrap_or(0)));
                touched.insert(id);
                id
            }
        };
        for a in members {
            target.insert(*a, msg);
        }
    }
    for cp in &copies {
        if let Some(of) = cp.copy_of {
            let msg = target.get(&of).copied().unwrap_or(cp.msg);
            target.insert(cp.id, msg);
        }
    }

    let mut move_copy = c
        .prepare("UPDATE artifact SET msg_id = ?2 WHERE artifact_id = ?1")
        .await?;
    for cp in &copies {
        let to = target[&cp.id];
        if to != cp.msg {
            move_copy.execute((cp.id, to)).await?;
            stats.moved += 1;
            touched.insert(to);
            touched.insert(cp.msg);
        }
    }
    for (m, into) in &stats.merged {
        c.execute(
            "UPDATE message SET merged_into = ?2, key = NULL WHERE msg_id = ?1",
            (*m, *into),
        )
        .await?;
        c.execute("DELETE FROM part WHERE msg_id = ?1", (*m,))
            .await?;
        c.execute(
            "INSERT OR IGNORE INTO text_forget (msg_id) VALUES (?1)",
            (*m,),
        )
        .await?;
        touched.remove(m);
    }
    // A message's preferred copy may change without its copies changing.
    for (m, a) in &preferred_copies(&copies, |cp| target[&cp.id]) {
        if preferred.get(m) != Some(a) {
            touched.insert(*m);
        }
    }
    let merged: BTreeSet<i64> = stats.merged.iter().map(|(m, _)| *m).collect();
    for m in touched.iter().filter(|m| !merged.contains(m)) {
        c.execute(
            "UPDATE message SET parts_version = NULL, text_version = NULL WHERE msg_id = ?1",
            (*m,),
        )
        .await?;
        stats.reindexed += 1;
    }
    info!(
        copies = copies.len(),
        rekeyed = stats.rekeyed,
        split = stats.split.len(),
        merged = stats.merged.len(),
        moved = stats.moved,
        reindexed = stats.reindexed,
        elapsed = ?t.elapsed(),
        "regrouped messages"
    );
    Ok(stats)
}
