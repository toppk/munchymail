//! Observations about stored copies (`copy_mark`): what munchymail noticed,
//! such as signs of damage. A mark never changes whether a copy is mail
//! (`artifact.kind`), which message it is in, or which copy is shown; it is
//! evidence for a person to weigh, and states what was measured, not a verdict.
//! Copies a person called damaged keep theirs; copies munchymail classified
//! as not mail have none.

use anyhow::Result;
use tracing::info;

use crate::headers::{raw_fields, raw_name};
use crate::key;
use crate::store::{Store, now};

/// The detectors' version; marks they set name it in `source`, and a new
/// version replaces them (`meta` key `marks_version`).
pub const VERSION: i64 = 2;

/// SQL condition on copy `a`: it is checked (not one munchymail classified as not mail).
const CHECKED: &str = "a.kind IS NOT 'folder-data'";

fn source() -> String {
    format!("munchymail marks v{VERSION}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub name: &'static str,
    pub detail: String,
}

fn value(field: &[u8]) -> String {
    let v = field
        .iter()
        .position(|&b| b == b':')
        .map_or(&[][..], |i| &field[i + 1..]);
    String::from_utf8_lossy(v)
        .split(['\r', '\n'])
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

/// What one copy's own bytes show:
/// - `content-length`: a `Content-Length` field (an mbox server's record of
///   the body it saved) that disagrees with the body stored, line endings
///   read as stored and as LF;
/// - `unclosed-boundary`: a multipart body whose outer boundary is never
///   closed (`--boundary--`): truncation, or a sender that built it wrongly.
pub fn detect(header: &[u8], body: &[u8]) -> Vec<Mark> {
    let fields = raw_fields(header);
    let first = |name: &str| {
        fields
            .iter()
            .find(|f| raw_name(f) == name)
            .map(|f| value(f))
    };
    let mut out = Vec::new();
    // A copy served with CRLF line endings keeps the field its LF mbox wrote.
    if let Some(n) = first("content-length").and_then(|v| v.trim().parse::<usize>().ok())
        && n != body.len()
        && n != lf(body).len()
    {
        let way = if body.len() < n { "shorter" } else { "longer" };
        out.push(Mark {
            name: "content-length",
            detail: format!(
                "Content-Length {n}, body {} bytes: the body is {way}",
                body.len()
            ),
        });
    }
    if let Some(ct) = first("content-type") {
        let ct = mailparse::parse_content_type(&ct);
        if let Some(b) = ct.params.get("boundary")
            && ct.mimetype.to_ascii_lowercase().starts_with("multipart/")
            && !b.is_empty()
        {
            let close = [b"--".as_slice(), b.as_bytes(), b"--"].concat();
            if !body.windows(close.len()).any(|w| w == close.as_slice()) {
                out.push(Mark {
                    name: "unclosed-boundary",
                    detail: format!("multipart boundary {b:?} is never closed"),
                });
            }
        }
    }
    out
}

/// Record `marks` on copy `artifact_id`. The caller owns the transaction.
pub async fn record(store: &Store, artifact_id: i64, marks: &[Mark]) -> Result<()> {
    for m in marks {
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO copy_mark (artifact_id, mark, detail, source, set_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (artifact_id, m.name, m.detail.as_str(), source(), now()),
            )
            .await?;
    }
    Ok(())
}

fn lf(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    for (i, &c) in b.iter().enumerate() {
        if !(c == b'\r' && b.get(i + 1) == Some(&b'\n')) {
            out.push(c);
        }
    }
    out
}

/// Mark `prefix-of` on the shorter of copy `artifact_id` and each other mail
/// copy with the same valid Message-ID and Date whose body identity differs,
/// when one body (line endings read alike, nothing else) is a strict, non-empty
/// prefix of the other. The detail says how the shorter ends: mid-line (a sign
/// of truncation), at a line the longer continues past (e.g. a mailing list's
/// footer added to one delivery), or short only of newlines. Copies
/// already in one message (joined by identity or a fold) are not compared.
/// Caller owns the transaction.
pub async fn prefix_candidates(store: &Store, artifact_id: i64) -> Result<u64> {
    let c = &store.conn;
    let mut rows = c
        .query(
            &format!(
                "SELECT b.artifact_id FROM artifact a JOIN artifact b
               ON b.message_id = a.message_id AND b.date IS a.date AND b.body_hash <> a.body_hash
              AND b.msg_id IS NOT a.msg_id
             WHERE a.artifact_id = ?1 AND a.message_id IS NOT NULL
               AND {CHECKED} AND b.kind IS NOT 'folder-data'"
            ),
            (artifact_id,),
        )
        .await?;
    let mut others = Vec::new();
    while let Some(r) = rows.next().await? {
        others.push(r.get::<i64>(0)?);
    }
    drop(rows);
    if others.is_empty() {
        return Ok(0);
    }
    let load = async |id: i64| -> Result<(Vec<u8>, Vec<u8>)> {
        let mut rows = c
            .query(
                "SELECT header_hash, body_hash FROM artifact WHERE artifact_id = ?1",
                (id,),
            )
            .await?;
        let r = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no copy {id}"))?;
        let (h, b): (Vec<u8>, Vec<u8>) = (r.get(0)?, r.get(1)?);
        drop(rows);
        Ok((store.object(&h).await?, store.object(&b).await?))
    };
    let (h, b) = load(artifact_id).await?;
    let Some(mid) = key::message_id(&h) else {
        return Ok(0);
    };
    let mine = lf(&b);
    let mut n = 0;
    for other in others {
        let (oh, ob) = load(other).await?;
        if key::message_id(&oh).as_deref() != Some(mid.as_str())
            || key::identity(&ob) == key::identity(&b)
        {
            continue;
        }
        let theirs = lf(&ob);
        let (short, long, short_len, long_len) = if mine.len() < theirs.len() {
            (artifact_id, other, mine.len(), theirs.len())
        } else {
            (other, artifact_id, theirs.len(), mine.len())
        };
        let (s, l) = if short == artifact_id {
            (&mine, &theirs)
        } else {
            (&theirs, &mine)
        };
        if !s.is_empty() && s.len() < l.len() && l.starts_with(s) {
            let rest = &l[s.len()..];
            let how = if rest.iter().all(|&c| c == b'\n') {
                format!("the longer has only {} more newline(s)", rest.len())
            } else if s.ends_with(b"\n") {
                let lines = rest.iter().filter(|&&c| c == b'\n').count().max(1);
                format!("the longer continues with {lines} more line(s)")
            } else {
                "this body stops mid-line".to_string()
            };
            record(
                store,
                short,
                &[Mark {
                    name: "prefix-of",
                    detail: format!(
                        "copy {long}: this body is its first {short_len} of {long_len} bytes (line endings read alike); {how}"
                    ),
                }],
            )
            .await?;
            n += 1;
        }
    }
    Ok(n)
}

/// Run the detectors over copies stored before them (or under an older
/// version), replacing that version's marks. Once per `VERSION`.
pub async fn backfill(store: &Store) -> Result<Option<(u64, u64)>> {
    let c = &store.conn;
    let current: Option<i64> = c
        .query("SELECT value FROM meta WHERE key = 'marks_version'", ())
        .await?
        .next()
        .await?
        .map(|r| r.get::<String>(0))
        .transpose()?
        .and_then(|v| v.parse().ok());
    if current == Some(VERSION) {
        return Ok(None);
    }
    let t = std::time::Instant::now();
    let mut copies = Vec::new();
    let mut rows = c
        .query(
            &format!("SELECT artifact_id, header_hash, body_hash FROM artifact a WHERE {CHECKED} ORDER BY artifact_id"),
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        copies.push((r.get::<i64>(0)?, r.get::<Vec<u8>>(1)?, r.get::<Vec<u8>>(2)?));
    }
    drop(rows);
    store.begin().await?;
    c.execute(
        "DELETE FROM copy_mark WHERE source LIKE 'munchymail marks v%'",
        (),
    )
    .await?;
    let mut marked = 0;
    for (i, (id, h, b)) in copies.iter().enumerate() {
        let marks = detect(&store.object(h).await?, &store.object(b).await?);
        if !marks.is_empty() {
            record(store, *id, &marks).await?;
            marked += 1;
        }
        if (i + 1) % 50_000 == 0 {
            info!(done = i + 1, of = copies.len(), elapsed = ?t.elapsed(), "marking copies");
        }
    }
    // Prefix candidates: copies sharing a Message-ID and Date with different bodies.
    let mut rows = c
        .query(
            &format!(
                "SELECT a.artifact_id FROM artifact a JOIN
               (SELECT message_id, date FROM artifact a WHERE {CHECKED} AND message_id IS NOT NULL
                GROUP BY message_id, date HAVING count(DISTINCT body_hash) > 1) g
               ON g.message_id = a.message_id AND g.date IS a.date
             WHERE {CHECKED} ORDER BY a.artifact_id"
            ),
            (),
        )
        .await?;
    let mut candidates = Vec::new();
    while let Some(r) = rows.next().await? {
        candidates.push(r.get::<i64>(0)?);
    }
    drop(rows);

    for id in candidates {
        prefix_candidates(store, id).await?;
    }
    // Each pair is compared from both sides; count the marks, not the comparisons.
    let mut rows = c
        .query(
            "SELECT count(*) FROM copy_mark WHERE mark = 'prefix-of'",
            (),
        )
        .await?;
    let prefixes = match rows.next().await? {
        Some(r) => r.get::<i64>(0)? as u64,
        None => 0,
    };
    drop(rows);
    c.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('marks_version', ?1)",
        (VERSION.to_string(),),
    )
    .await?;
    store.commit().await?;
    info!(copies = copies.len(), marked, prefixes, elapsed = ?t.elapsed(), "copies marked");
    Ok(Some((marked, prefixes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detectors_report_what_they_measured() {
        let names = |h: &str, b: &str| -> Vec<&'static str> {
            detect(h.as_bytes(), b.as_bytes())
                .iter()
                .map(|m| m.name)
                .collect()
        };
        assert!(names("Content-Length: 5\n\n", "hello").is_empty());
        assert!(names("Content-Length: 6\r\n\r\n", "a\r\nb\r\nc\r\n").is_empty());
        let m = detect(b"Content-Length: 9\n\n", b"hello");
        assert_eq!(m[0].name, "content-length");
        assert!(m[0].detail.contains("shorter"));
        assert!(
            detect(b"Content-Length: 2\n\n", b"hello")[0]
                .detail
                .contains("longer")
        );
        let mp = "Content-Type: multipart/mixed;\n boundary=\"AbC\"\n\n";
        assert!(names(mp, "--AbC\n\nx\n--AbC--\n").is_empty());
        assert_eq!(names(mp, "--AbC\n\nx\n"), ["unclosed-boundary"]);
        // The boundary is matched exactly, case included.
        assert_eq!(names(mp, "--AbC\n\nx\n--abc--\n"), ["unclosed-boundary"]);
        assert!(names("Content-Type: text/plain\n\n", "no parts").is_empty());
    }
}
