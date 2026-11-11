//! OCR (English) of image parts and of PDFs with no text layer, into
//! `extract`: `tesseract` and `ocrmypdf`, sandboxed like `extract`'s tools.
//! Slow, so a command of its own that holds the database only to load and
//! to record each chunk, leaving it free for other commands in between.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rayon::prelude::*;
use tracing::{info, warn};
use turso::Value;

use crate::extract::{self, Extracted, Limits, PartLoader, Recorder, done, sandboxed};
use crate::store::Store;
use crate::text;

/// Smaller images are icons, logos and spacers.
pub const MIN_IMAGE: i64 = 10 * 1024;
/// Words tesseract is less sure of than this (0 to 100) are dropped: photos
/// otherwise come out as strings of junk.
const MIN_CONFIDENCE: f64 = 40.0;
const CHUNK: usize = 64;
const IMAGE: Limits = Limits {
    time: Duration::from_secs(120),
    memory: 2 << 30,
};
const PDF: Limits = Limits {
    time: Duration::from_secs(900),
    memory: 2 << 30,
};
/// How long to wait for another munchymail process to release the database.
const LOCK_WAIT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Pdf,
}

struct Job {
    hash: Vec<u8>,
    msg: i64,
    ordinal: usize,
    kind: Kind,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub queued: u64,
    pub done: u64,
    pub with_text: u64,
    pub failed: u64,
}

async fn open(data_dir: &Path) -> Result<Store> {
    let start = Instant::now();
    let mut warned = false;
    loop {
        match Store::open(data_dir).await {
            Ok(s) => return Ok(s),
            Err(e) if start.elapsed() < LOCK_WAIT => {
                if !warned {
                    warn!("waiting for the database: {e:#}");
                    warned = true;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => return Err(e),
        }
    }
}

/// PDFs whose text layer came out empty, then image parts of at least
/// `MIN_IMAGE` bytes, neither yet OCRed by this `extract::VERSION`.
async fn queue(store: &Store) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    let mut seen = HashSet::new();
    for (kind, sql, params) in [
        (
            Kind::Pdf,
            "SELECT p.hash, p.msg_id, p.ordinal FROM extract e JOIN part p ON p.hash = e.hash
             WHERE e.version = ?1 AND e.tool LIKE 'pdftotext%' AND e.outcome = 'empty'",
            vec![Value::from(extract::VERSION)],
        ),
        (
            Kind::Image,
            "SELECT p.hash, p.msg_id, p.ordinal FROM part p
             LEFT JOIN extract e ON e.hash = p.hash
             WHERE p.kind != 'body' AND p.size >= ?2 AND e.version IS NOT ?1
               AND coalesce(p.detected_type, p.declared_type) IN
                   ('image/jpeg', 'image/png', 'image/gif', 'image/tiff', 'image/bmp', 'image/webp')",
            vec![extract::VERSION.into(), MIN_IMAGE.into()],
        ),
    ] {
        let mut rows = store.conn.query(sql, params).await?;
        while let Some(r) = rows.next().await? {
            let Value::Blob(hash) = r.get_value(0)? else {
                continue;
            };
            if seen.insert(hash.clone()) {
                jobs.push(Job {
                    hash,
                    msg: r.get(1)?,
                    ordinal: r.get::<i64>(2)? as usize,
                    kind,
                });
            }
        }
    }
    Ok(jobs)
}

/// Words of tesseract's TSV at `MIN_CONFIDENCE` or above, a line per line.
fn tsv_text(tsv: &str) -> String {
    let mut out = String::new();
    let mut line = None;
    for row in tsv.lines().skip(1) {
        let f: Vec<&str> = row.split('\t').collect();
        let [_, page, block, par, ln, _, _, _, _, _, conf, word] = f[..] else {
            continue;
        };
        if word.trim().is_empty() || conf.parse::<f64>().is_ok_and(|c| c < MIN_CONFIDENCE) {
            continue;
        }
        let key = (page, block, par, ln);
        if line.is_some_and(|l| l != key) {
            out.push('\n');
        } else if line.is_some() {
            out.push(' ');
        }
        line = Some(key);
        out.push_str(word);
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn ocr(job: &Job, bytes: &[u8], tmp: &Path, tools: &(String, String)) -> Extracted {
    match job.kind {
        Kind::Image => {
            let cmd = ["tesseract", "/in", "-", "-l", "eng", "tsv"];
            let out = sandboxed(bytes, &cmd, tmp, IMAGE)
                .map(|tsv| tsv_text(&String::from_utf8_lossy(&tsv)).into_bytes());
            done(tools.0.clone(), out)
        }
        Kind::Pdf => {
            let script = "ocrmypdf -q -j 1 -l eng --force-ocr --output-type none \
                          --sidecar /tmp/text /in - && cat /tmp/text";
            done(
                tools.1.clone(),
                sandboxed(bytes, &["sh", "-c", script], tmp, PDF),
            )
        }
    }
}

/// OCR up to `limit` queued parts, `jobs` at a time, recording each chunk and
/// bringing the text index up to date after it.
pub async fn run(data_dir: &Path, limit: Option<usize>, jobs: usize) -> Result<Stats> {
    let t = Instant::now();
    let mut queue = {
        let store = open(data_dir).await?;
        queue(&store).await?
    };
    if let Some(n) = limit {
        queue.truncate(n);
    }
    let mut stats = Stats {
        queued: queue.len() as u64,
        ..Stats::default()
    };
    if queue.is_empty() {
        return Ok(stats);
    }
    if extract::version("bwrap", "--version").is_none() {
        bail!("ocr needs bwrap (bubblewrap) to sandbox tesseract and ocrmypdf");
    }
    let tools = (
        extract::version("tesseract", "--version").unwrap_or_else(|| "tesseract".into()),
        extract::version("ocrmypdf", "--version").unwrap_or_else(|| "ocrmypdf".into()),
    );
    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;
    let tmp = data_dir.join("tmp");
    for chunk in queue.chunks(CHUNK) {
        let mut inputs = Vec::with_capacity(chunk.len());
        {
            let store = open(data_dir).await?;
            let mut load = PartLoader::new(&store).await?;
            for j in chunk {
                inputs.push(load.bytes(j.msg, j.ordinal).await?);
            }
        }
        let results: Vec<Extracted> = pool.install(|| {
            chunk
                .par_iter()
                .zip(&inputs)
                .enumerate()
                .map(|(i, (j, bytes))| {
                    let dir = tmp.join(format!("ocr-{}-{i}", std::process::id()));
                    let e = std::fs::create_dir_all(&dir)
                        .map_err(|e| format!("failed: {}: {e}", dir.display()))
                        .map(|()| ocr(j, bytes, &dir, &tools));
                    let _ = std::fs::remove_dir(&dir);
                    e.unwrap_or_else(|e| done("munchymail".into(), Err(e)))
                })
                .collect()
        });
        let store = open(data_dir).await?;
        let mut rec = Recorder::new(&store).await?;
        store.begin().await?;
        for (j, e) in chunk.iter().zip(&results) {
            rec.record(&j.hash, e).await?;
            stats.with_text += e.text.is_some() as u64;
            stats.failed += (e.outcome.starts_with("failed") || e.outcome == "timeout") as u64;
        }
        store.commit().await?;
        text::index(&store).await?;
        stats.done += chunk.len() as u64;
        info!(done = stats.done, of = stats.queued, with_text = stats.with_text, elapsed = ?t.elapsed(), "ocr");
    }
    let _ = std::fs::remove_dir(&tmp);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_confident_words_by_line() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
1\t1\t0\t0\t0\t0\t0\t0\t9\t9\t-1\t\n\
5\t1\t1\t1\t1\t1\t0\t0\t9\t9\t96.1\tHello\n\
5\t1\t1\t1\t1\t2\t0\t0\t9\t9\t12.0\t~~j\n\
5\t1\t1\t1\t1\t3\t0\t0\t9\t9\t91.0\tworld\n\
5\t1\t1\t1\t2\t1\t0\t0\t9\t9\t88.0\tnext\n";
        assert_eq!(tsv_text(tsv), "Hello world\nnext\n");
        assert_eq!(tsv_text("header\n"), "");
    }
}
