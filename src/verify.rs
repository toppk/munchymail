//! Rebuild recorded mbox files from what the archive stores, and check each
//! against the file's recorded SHA-256: the stored copies, re-quoted as the
//! file's format quoted them, between their `From ` lines and separators.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::mbox::{self, Variant};
use crate::store::Store;

/// One file content that didn't rebuild, and why.
#[derive(Debug)]
pub struct Failure {
    pub content_id: i64,
    pub path: String,
    pub why: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub files: u64,
    pub bytes: u64,
    pub failures: Vec<Failure>,
}

/// Rebuild every mbox file content seen in origin `label`.
pub async fn origin(store: &Store, label: &str) -> Result<Report> {
    let c = &store.conn;
    let mut rows = c
        .query("SELECT origin_id FROM origin WHERE label = ?1", (label,))
        .await?;
    let Some(r) = rows.next().await? else {
        bail!("no origin labelled {label:?}; `mmadm origin list` lists them");
    };
    let origin_id: i64 = r.get(0)?;
    drop(rows);
    let mut contents = Vec::new();
    let mut rows = c
        .query(
            "SELECT f.content_id, f.hash, f.length, f.format, f.preamble_length, min(s.path)
             FROM file_sighting s JOIN file_content f ON f.content_id = s.content_id
             WHERE s.origin_id = ?1 AND f.format IN ('dovecot', 'mboxrd')
             GROUP BY f.content_id ORDER BY f.content_id",
            (origin_id,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        contents.push((
            r.get::<i64>(0)?,
            r.get::<Vec<u8>>(1)?,
            r.get::<i64>(2)? as u64,
            r.get::<String>(3)?,
            r.get::<i64>(4)?,
            r.get::<String>(5)?,
        ));
    }
    drop(rows);
    if contents.is_empty() {
        bail!("origin {label:?} has no recorded mbox files");
    }
    let mut report = Report::default();
    for (content_id, hash, length, format, preamble, path) in contents {
        report.files += 1;
        let why = match rebuild(store, content_id, &format, preamble).await? {
            Err(why) => Some(why),
            Ok((len, _)) if len != length => {
                Some(format!("rebuilt {len} bytes, the file had {length}"))
            }
            Ok((_, h)) if h.as_slice() != hash.as_slice() => {
                Some("same length, different bytes".into())
            }
            Ok(_) => None,
        };
        match why {
            Some(why) => report.failures.push(Failure {
                content_id,
                path,
                why,
            }),
            None => report.bytes += length,
        }
    }
    Ok(report)
}

/// The rebuilt file's length and SHA-256, or why it can't be rebuilt.
async fn rebuild(
    store: &Store,
    content_id: i64,
    format: &str,
    preamble: i64,
) -> Result<std::result::Result<(u64, [u8; 32]), String>> {
    let variant = Variant::parse(format).with_context(|| format!("unknown format {format}"))?;
    if preamble > 0 {
        return Ok(Err(format!(
            "its first {preamble} bytes come before any message and are not kept"
        )));
    }
    let mut rows = store
        .conn
        .query(
            "SELECT o.ordinal, o.offset, o.from_line, o.separator, a.header_hash, a.body_hash
             FROM observation o JOIN artifact a ON a.artifact_id = o.artifact_id
             WHERE o.content_id = ?1 ORDER BY o.ordinal",
            (content_id,),
        )
        .await?;
    let mut parts = Vec::new();
    while let Some(r) = rows.next().await? {
        parts.push((
            r.get::<i64>(0)?,
            r.get::<i64>(1)? as u64,
            r.get::<Vec<u8>>(2)?,
            r.get::<Vec<u8>>(3)?,
            r.get::<Vec<u8>>(4)?,
            r.get::<Vec<u8>>(5)?,
        ));
    }
    drop(rows);
    let mut h = Sha256::new();
    let mut len = 0u64;
    for (ordinal, offset, from_line, separator, header, body) in parts {
        if offset != len {
            return Ok(Err(format!(
                "message {ordinal} was at byte {offset}, the rebuild reaches it at {len}"
            )));
        }
        let header = store.object(&header).await?;
        let body = store.object(&body).await?;
        for piece in [
            &from_line[..],
            &mbox::quote(variant, &header),
            &mbox::quote(variant, &body),
            &separator[..],
        ] {
            h.update(piece);
            len += piece.len() as u64;
        }
    }
    Ok(Ok((len, h.finalize().into())))
}
