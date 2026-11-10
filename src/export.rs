//! Matching messages as a `.tgz` someone else can keep: `NAME/index.md` and one
//! file per message under `NAME/maildir/`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use turso::Value;

use crate::locations::{self, Location, Sightings};
use crate::query::Filter;
use crate::store::{Store, now};
use crate::util::{hex, ymd, ymd_hm};

pub struct Summary {
    pub messages: usize,
    /// Copies left out because they differ from an exported one only in headers.
    pub folded: usize,
}

struct Artifact {
    id: i64,
    date: Option<i64>,
    header_hash: Vec<u8>,
    body_hash: Vec<u8>,
    message_id: Option<String>,
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

/// Write `out` (via a temporary file) with every message matching `filter`.
/// Copies with the same body and Message-ID are one message; the copy seen
/// most recently is exported. `command` is quoted in index.md as provenance.
pub async fn write(
    store: &Store,
    filter: &Filter,
    name: &str,
    title: &str,
    command: &str,
    out: &Path,
) -> Result<Summary> {
    if filter.materialize(&store.conn).await? != "hit" {
        bail!("export needs at least one filter");
    }
    let mut artifacts = Vec::new();
    let mut rows = store
        .conn
        .query(
            "SELECT a.artifact_id, a.date, a.header_hash, a.body_hash, a.message_id, a.subject
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
            message_id: r.get(4)?,
            subject: r.get::<Option<String>>(5)?.unwrap_or_default(),
        });
    }
    drop(rows);
    if artifacts.is_empty() {
        bail!("no messages match");
    }

    let mut sightings = Sightings::new(store).await?;
    // (body hash, Message-ID) -> where seen, and (last seen, index) of each copy.
    type Group = (Vec<Location>, Vec<(i64, usize)>);
    let mut groups: HashMap<(Vec<u8>, Option<String>), Group> = HashMap::new();
    for (i, a) in artifacts.iter().enumerate() {
        let (seen, members) = groups
            .entry((a.body_hash.clone(), a.message_id.clone()))
            .or_default();
        let locs = sightings.of(a.id).await?;
        let last = locs.iter().map(|l| l.last_seen).max().unwrap_or(0);
        locations::merge(seen, locs);
        members.push((last, i));
    }

    // One representative per message, in date order.
    let folded = artifacts.len() - groups.len();
    let mut chosen: Vec<(&Artifact, Vec<Location>)> = groups
        .into_values()
        .map(|(seen, members)| {
            let &(_, i) = members
                .iter()
                .max_by_key(|&&(run, i)| (run, artifacts[i].id))
                .unwrap();
            (&artifacts[i], seen)
        })
        .collect();
    chosen.sort_by_key(|(a, _)| (a.date.is_none(), a.date, a.id));

    let width = chosen.len().to_string().len().max(3);
    let tmp = out.with_extension("tmp");
    let mut tar = tar::Builder::new(GzEncoder::new(
        File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?,
        Compression::default(),
    ));
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
         call `.eml`), numbered by date and byte for byte as stored in the mailbox,\n\
         including the mail server's bookkeeping headers (`X-UID`, `Status`).\n\
         Dates are UTC. \"Folder\" is where the message is now.\n",
        chosen.len()
    );
    if folded > 0 {
        write!(
            index,
            "\n{folded} further copies differed only in such headers and are left out.\n"
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
    tar.into_inner()?.finish()?.sync_all()?;
    fs::rename(&tmp, out).with_context(|| format!("renaming to {}", out.display()))?;
    Ok(Summary {
        messages: chosen.len(),
        folded,
    })
}
