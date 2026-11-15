//! What a person says about stored copies: that a copy is damaged
//! (`artifact.kind`, each change kept in `copy_kind_change`), and which copy
//! repaired which (`copy_link`). munchymail itself only classifies copies
//! that aren't mail (`folder-data`); a person's kinds never override that.

use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::api::{self, Ref};
use crate::headers;
use crate::store::{Store, now};

/// A copy whose bytes are known to be wrong; a person's judgement.
pub const DAMAGED: &str = "damaged";
/// Copy `artifact_id` was built to replace copy `other_id`.
pub const REPAIRED_FROM: &str = "repaired-from";
/// Who `mmadm copy` records as having said so.
pub const OWNER: &str = "owner";

/// The copies `r` names: the copy, or every copy of the message.
pub async fn copies_of(store: &Store, r: Ref) -> Result<Vec<i64>> {
    let (sql, id) = match r {
        Ref::Copy(id) => (
            "SELECT artifact_id FROM artifact WHERE artifact_id = ?1",
            id,
        ),
        Ref::Message(m) => (
            "SELECT artifact_id FROM artifact WHERE msg_id = ?1 ORDER BY artifact_id",
            api::resolve(store, m).await?,
        ),
    };
    let mut rows = store.conn.query(sql, (id,)).await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push(r.get(0)?);
    }
    if out.is_empty() {
        bail!("no stored copy or message {r}");
    }
    Ok(out)
}

/// Set `copies`' kind to `kind`, or clear it (`None`), saying why. The
/// indexes follow the one rule for copies that aren't mail: no header rows
/// for such a copy, no parts or text for a message without a mail copy, and
/// a message whose copies changed is indexed again from its preferred copy
/// (by the next `index`). Returns the copies that changed.
pub async fn set(
    store: &Store,
    copies: &[i64],
    kind: Option<&str>,
    reason: &str,
    source: &str,
) -> Result<Vec<i64>> {
    let c = &store.conn;
    store.begin().await?;
    let r = async {
        let mut changed = Vec::new();
        let mut messages = BTreeSet::new();
        for &id in copies {
            let mut rows = c
                .query(
                    "SELECT kind, msg_id, header_hash FROM artifact WHERE artifact_id = ?1",
                    (id,),
                )
                .await?;
            let Some(r) = rows.next().await? else {
                bail!("no stored copy {id}");
            };
            let (now_kind, msg, header): (Option<String>, i64, Vec<u8>) =
                (r.get(0)?, r.get(1)?, r.get(2)?);
            drop(rows);
            match now_kind.as_deref() {
                Some(k) if k != DAMAGED => {
                    bail!(
                        "copy {id} is {k}, as munchymail classified it; that isn't changed by hand"
                    )
                }
                k if k == kind => continue,
                _ => {}
            }
            c.execute(
                "UPDATE artifact SET kind = ?2 WHERE artifact_id = ?1",
                (id, kind),
            )
            .await?;
            c.execute(
                "INSERT INTO copy_kind_change (artifact_id, kind, reason, source, set_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (id, kind, reason, source, now()),
            )
            .await?;
            c.execute("DELETE FROM header WHERE artifact_id = ?1", (id,))
                .await?;
            if kind.is_none() {
                let fields = headers::fields(&store.object(&header).await?);
                let mut nth = std::collections::HashMap::<&str, i64>::new();
                for (i, f) in fields.iter().enumerate() {
                    let n = nth.entry(f.name.as_str()).or_default();
                    c.execute(
                        "INSERT INTO header (artifact_id, ordinal, name, nth, value)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        (id, i as i64, f.name.as_str(), *n, f.value.as_str()),
                    )
                    .await?;
                    *n += 1;
                }
            }
            changed.push(id);
            messages.insert(msg);
        }
        for msg in messages {
            let mut rows = c
                .query(
                    "SELECT count(*) FROM artifact WHERE msg_id = ?1 AND kind IS NULL",
                    (msg,),
                )
                .await?;
            let mail: i64 = rows.next().await?.map_or(Ok(0), |r| r.get(0))?;
            drop(rows);
            if mail == 0 {
                c.execute("DELETE FROM part WHERE msg_id = ?1", (msg,))
                    .await?;
                c.execute(
                    "INSERT OR IGNORE INTO text_forget (msg_id) VALUES (?1)",
                    (msg,),
                )
                .await?;
            }
            c.execute(
                "UPDATE message SET parts_version = NULL, text_version = NULL WHERE msg_id = ?1",
                (msg,),
            )
            .await?;
        }
        Ok(changed)
    }
    .await;
    match r {
        Ok(changed) => {
            store.commit().await?;
            Ok(changed)
        }
        Err(e) => {
            store.rollback().await?;
            Err(e)
        }
    }
}

/// Record (or with `remove`, take back) that copy `copy` was `relation` each
/// of `others`. Returns how many links changed.
pub async fn link(
    store: &Store,
    copy: i64,
    relation: &str,
    others: &[i64],
    note: Option<&str>,
    remove: bool,
) -> Result<u64> {
    let c = &store.conn;
    for &id in std::iter::once(&copy).chain(others) {
        let mut rows = c
            .query("SELECT 1 FROM artifact WHERE artifact_id = ?1", (id,))
            .await?;
        if rows.next().await?.is_none() {
            bail!("no stored copy {id}");
        }
    }
    if others.contains(&copy) {
        bail!("copy {copy} can't be {relation} itself");
    }
    let mut n = 0;
    for &other in others {
        n += if remove {
            c.execute(
                "DELETE FROM copy_link WHERE artifact_id = ?1 AND relation = ?2 AND other_id = ?3",
                (copy, relation, other),
            )
            .await?
        } else {
            c.execute(
                "INSERT OR IGNORE INTO copy_link (artifact_id, relation, other_id, note, source, set_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (copy, relation, other, note, OWNER, now()),
            )
            .await?
        };
    }
    Ok(n)
}

/// A copy's kind as a person set it: the latest change.
#[derive(Debug, Clone)]
pub struct KindNote {
    pub copy: i64,
    pub msg: i64,
    pub kind: String,
    pub reason: String,
    pub source: String,
    pub set_at: i64,
}

/// A link between two copies, from `copy`'s side; `reverse` when `copy` is
/// the one the relation points to (e.g. it was repaired by `other`).
#[derive(Debug, Clone)]
pub struct LinkNote {
    pub copy: i64,
    pub relation: String,
    pub other: i64,
    pub other_msg: i64,
    pub reverse: bool,
    pub note: Option<String>,
    pub set_at: i64,
}

/// The kinds people set on `copies` (or on every copy, if `None`).
pub async fn kinds(store: &Store, copies: Option<&[i64]>) -> Result<Vec<KindNote>> {
    let mut rows = store
        .conn
        .query(
            "SELECT a.artifact_id, a.msg_id, a.kind, k.reason, k.source, k.set_at
             FROM artifact a JOIN copy_kind_change k ON k.artifact_id = a.artifact_id
             WHERE a.kind IS NOT NULL AND k.rowid =
               (SELECT max(rowid) FROM copy_kind_change WHERE artifact_id = a.artifact_id)
             ORDER BY a.artifact_id",
            (),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        let copy: i64 = r.get(0)?;
        if copies.is_none_or(|c| c.contains(&copy)) {
            out.push(KindNote {
                copy,
                msg: r.get(1)?,
                kind: r.get(2)?,
                reason: r.get(3)?,
                source: r.get(4)?,
                set_at: r.get(5)?,
            });
        }
    }
    Ok(out)
}

/// The links touching `copies` (or every link, if `None`), from each side.
pub async fn links(store: &Store, copies: Option<&[i64]>) -> Result<Vec<LinkNote>> {
    let mut rows = store
        .conn
        .query(
            "SELECT l.artifact_id, l.relation, l.other_id, l.note, l.set_at, a.msg_id, b.msg_id
             FROM copy_link l JOIN artifact a ON a.artifact_id = l.artifact_id
             JOIN artifact b ON b.artifact_id = l.other_id
             ORDER BY l.artifact_id, l.other_id",
            (),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        let (copy, relation, other, note, set_at): (i64, String, i64, Option<String>, i64) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
        let (msg, other_msg): (i64, i64) = (r.get(5)?, r.get(6)?);
        let want = |id: i64| copies.is_none_or(|c| c.contains(&id));
        if want(copy) {
            out.push(LinkNote {
                copy,
                relation: relation.clone(),
                other,
                other_msg,
                reverse: false,
                note: note.clone(),
                set_at,
            });
        }
        if want(other) && copies.is_some() {
            out.push(LinkNote {
                copy: other,
                relation,
                other: copy,
                other_msg: msg,
                reverse: true,
                note,
                set_at,
            });
        }
    }
    Ok(out)
}
