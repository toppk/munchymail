//! Matching messages as a `.tgz` someone else can keep: `NAME/index.md` and one
//! file per message under `NAME/maildir/`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;

use anyhow::{Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use turso::Value;

use crate::locations::{self, Location, Sightings};
use crate::query::{Filter, PREFERRED};
use crate::store::{Store, now};
use crate::util::{hex, ymd, ymd_hm};

pub struct Summary {
    pub messages: usize,
    /// Copies left out because they are other copies of an exported message.
    pub folded: usize,
}

struct Artifact {
    id: i64,
    date: Option<i64>,
    header_hash: Vec<u8>,
    body_hash: Vec<u8>,
    msg_id: i64,
    subject: String,
}

fn blob(v: Value) -> Result<Vec<u8>> {
    match v {
        Value::Blob(b) => Ok(b),
        v => bail!("expected a hash, got {v:?}"),
    }
}

/// Escape text for a Markdown table cell.
fn cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' | '\n' | '\t' => out.push(' '),
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// Line endings for the exported messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Newlines {
    /// As stored: each copy's bytes unchanged.
    #[default]
    Keep,
    /// Unix (LF), as mbox and maildir tools expect.
    Lf,
    /// Network (CRLF), as IMAP and RFC 5322 use.
    Crlf,
}

/// `bytes` with `to` line endings, if they are all one kind now; a message
/// that mixes them is left as it is.
pub fn newlines(bytes: Vec<u8>, to: Newlines) -> Vec<u8> {
    let lfs = bytes.iter().filter(|&&b| b == b'\n').count();
    let crlfs = bytes.windows(2).filter(|w| *w == b"\r\n").count();
    match to {
        Newlines::Lf if crlfs > 0 && crlfs == lfs => {
            let mut out = Vec::with_capacity(bytes.len() - crlfs);
            for (i, &b) in bytes.iter().enumerate() {
                if !(b == b'\r' && bytes.get(i + 1) == Some(&b'\n')) {
                    out.push(b);
                }
            }
            out
        }
        Newlines::Crlf if lfs > 0 && !bytes.contains(&b'\r') => {
            let mut out = Vec::with_capacity(bytes.len() + lfs);
            for &b in &bytes {
                if b == b'\n' {
                    out.push(b'\r');
                }
                out.push(b);
            }
            out
        }
        _ => bytes,
    }
}

/// Write a .tgz of every message matching `filter` to `out`. A message is
/// exported once, as its preferred copy (not folded, then the earliest stored),
/// whichever of its copies matched. `command` is quoted in index.md as provenance.
#[allow(clippy::too_many_arguments)]
pub async fn write(
    store: &Store,
    filter: &Filter,
    name: &str,
    title: &str,
    command: &str,
    line_endings: Newlines,
    out: &mut impl Write,
) -> Result<Summary> {
    if filter.is_empty()? {
        bail!("export needs at least one filter");
    }
    filter.materialize(&store.conn).await?;
    let mut artifacts = Vec::new();
    let mut rows = store
        .conn
        .query(
            "SELECT a.artifact_id, a.date, a.header_hash, a.body_hash, a.msg_id, a.subject
             FROM hit JOIN artifact a ON a.artifact_id = hit.artifact_id",
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        artifacts.push(Artifact {
            id: r.get(0)?,
            date: r.get(1)?,
            header_hash: blob(r.get_value(2)?)?,
            body_hash: blob(r.get_value(3)?)?,
            msg_id: r.get(4)?,
            subject: r.get::<Option<String>>(5)?.unwrap_or_default(),
        });
    }
    drop(rows);
    if artifacts.is_empty() {
        bail!("no messages match; `munchymail search --count` with the same filters checks them");
    }

    let mut sightings = Sightings::new(store).await?;
    // Message -> where its matching copies were seen.
    let mut groups: HashMap<i64, Vec<Location>> = HashMap::new();
    for a in &artifacts {
        locations::merge(
            groups.entry(a.msg_id).or_default(),
            sightings.of(a.id).await?,
        );
    }
    let folded = artifacts.len() - groups.len();

    // Each message's preferred copy, in date order.
    let mut rows = store
        .conn
        .query(
            &format!(
                "SELECT p.artifact_id, p.date, p.header_hash, p.body_hash, p.msg_id, p.subject
                 FROM (SELECT {PREFERRED} AS id FROM artifact a
                       LEFT JOIN fold f ON f.artifact_id = a.artifact_id
                       WHERE a.msg_id IN (SELECT m.msg_id FROM hit JOIN artifact m
                                          ON m.artifact_id = hit.artifact_id)
                       GROUP BY a.msg_id) q
                 JOIN artifact p ON p.artifact_id = q.id"
            ),
            (),
        )
        .await?;
    let mut preferred = Vec::new();
    while let Some(r) = rows.next().await? {
        preferred.push(Artifact {
            id: r.get(0)?,
            date: r.get(1)?,
            header_hash: blob(r.get_value(2)?)?,
            body_hash: blob(r.get_value(3)?)?,
            msg_id: r.get(4)?,
            subject: r.get::<Option<String>>(5)?.unwrap_or_default(),
        });
    }
    drop(rows);
    let matched: std::collections::HashSet<i64> = artifacts.iter().map(|a| a.id).collect();
    let unmatched = preferred
        .iter()
        .filter(|a| !matched.contains(&a.id))
        .count();
    let mut chosen: Vec<(&Artifact, Vec<Location>)> = preferred
        .iter()
        .map(|a| (a, groups.remove(&a.msg_id).unwrap_or_default()))
        .collect();
    chosen.sort_by_key(|(a, _)| (a.date.is_none(), a.date, a.id));

    let width = chosen.len().to_string().len().max(3);
    let mut tar = tar::Builder::new(GzEncoder::new(out, Compression::default()));
    let t = now();
    for dir in [format!("{name}/"), format!("{name}/maildir/")] {
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_mtime(t as u64);
        h.set_size(0);
        tar.append_data(&mut h, dir, std::io::empty())?;
    }

    let mut table = String::new();
    let mut sums = String::new();
    for (n, (a, seen)) in chosen.iter().enumerate() {
        let file = format!("maildir/{:0width$}", n + 1);
        let mut bytes = store.object(&a.header_hash).await?;
        bytes.extend(store.object(&a.body_hash).await?);
        let bytes = newlines(bytes, line_endings);
        let mut h = tar::Header::new_ustar();
        h.set_mode(0o644);
        h.set_mtime(a.date.unwrap_or(t).max(0) as u64);
        h.set_size(bytes.len() as u64);
        tar.append_data(&mut h, format!("{name}/{file}"), bytes.as_slice())?;
        writeln!(sums, "{}  {file}", hex(&Sha256::digest(&bytes)))?;

        writeln!(
            table,
            "| [{}]({file}) | {} | {} | {} | {} | {} |",
            n + 1,
            a.date.map_or("?".into(), ymd_hm),
            cell(&store.first_header(a.id, "from").await?),
            cell(&store.first_header(a.id, "to").await?),
            cell(&a.subject),
            cell(&locations::folder(seen))
        )?;
    }

    let dates: Vec<i64> = chosen.iter().filter_map(|(a, _)| a.date).collect();
    let span = match (dates.first(), dates.last()) {
        (Some(f), Some(l)) => format!(", {} to {}", ymd(*f), ymd(*l)),
        _ => String::new(),
    };
    let mut index = format!(
        "# {title}\n\n{} messages{span}.\n\n\
         Each file in `maildir/` is one whole message (RFC 5322, what mail programs\n\
         call `.eml`), numbered by date and {}\n\
         including the mail server's bookkeeping headers (`X-UID`, `Status`).\n\
         Dates are UTC. \"Folder\" is where the message is now.\n",
        chosen.len(),
        match line_endings {
            Newlines::Keep => "byte for byte as stored in the mailbox,",
            Newlines::Lf =>
                "as stored, except that line endings are LF\n(a message stored with only CRLF was converted),",
            Newlines::Crlf =>
                "as stored, except that line endings are CRLF\n(a message stored with only LF was converted),",
        }
    );
    if folded > 0 {
        write!(
            index,
            "\n{folded} further stored copies of these messages (the same email stored\n\
             again, e.g. with headers a mail server added) are left out.\n"
        )?;
    }
    if unmatched > 0 {
        write!(
            index,
            "\n{unmatched} of these messages matched through another copy; each message is\n\
             exported as its preferred copy (not a lesser one, then the earliest stored).\n"
        )?;
    }
    write!(
        index,
        "\nExported {} with munchymail:\n\n    {command}\n\n\
         | # | Date | From | To | Subject | Folder |\n|---|---|---|---|---|---|\n{table}\n\
         ## SHA-256\n\n```\n{sums}```\n",
        ymd(t)
    )?;
    let mut h = tar::Header::new_ustar();
    h.set_mode(0o644);
    h.set_mtime(t as u64);
    h.set_size(index.len() as u64);
    tar.append_data(&mut h, format!("{name}/index.md"), index.as_bytes())?;
    tar.into_inner()?.finish()?.flush()?;
    Ok(Summary {
        messages: chosen.len(),
        folded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_convert_only_uniform_messages() {
        let crlf = b"Subject: a\r\n\r\nline\r\n".to_vec();
        let lf = b"Subject: a\n\nline\n".to_vec();
        assert_eq!(newlines(crlf.clone(), Newlines::Lf), lf);
        assert_eq!(newlines(lf.clone(), Newlines::Crlf), crlf);
        assert_eq!(newlines(lf.clone(), Newlines::Lf), lf);
        assert_eq!(newlines(crlf.clone(), Newlines::Crlf), crlf);
        let mixed = b"Subject: a\r\n\nline\n".to_vec();
        assert_eq!(newlines(mixed.clone(), Newlines::Lf), mixed);
        assert_eq!(newlines(mixed.clone(), Newlines::Crlf), mixed);
    }
}
