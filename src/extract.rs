//! Text from document attachments, once per part hash, into `extract`: PDF
//! via `pdftotext`, Word/Excel 97 and RTF via `catdoc` (else `antiword`),
//! `xls2csv` and `pandoc`,
//! all under `bwrap` (no network, read-only system, time and memory caps);
//! OOXML and OpenDocument read in-process from their XML.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use rayon::prelude::*;
use tracing::{info, warn};
use turso::Value;

use crate::message::{self, Decoded, MAX_TEXT};
use crate::parts::stored;
use crate::store::Store;

/// Bump when extraction changes, so `index` redoes every document.
pub const VERSION: i64 = 2;

const BATCH: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MEMORY: u64 = 1 << 30;
/// Most XML read from one member of an office zip, against zip bombs.
const MAX_XML: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    OpenDocument,
    Doc,
    Xls,
    Rtf,
}

/// The format to extract from a part of this type and file extension, if any.
pub fn format(file_type: &str, ext: Option<&str>) -> Option<Format> {
    const OOXML: &str = "application/vnd.openxmlformats-officedocument.";
    Some(match file_type {
        "application/pdf" => Format::Pdf,
        t if t.starts_with(OOXML) => match &t[OOXML.len()..] {
            "wordprocessingml.document" => Format::Docx,
            "spreadsheetml.sheet" => Format::Xlsx,
            "presentationml.presentation" => Format::Pptx,
            _ => return None,
        },
        t if t.starts_with("application/vnd.oasis.opendocument.") => Format::OpenDocument,
        "application/msword" => Format::Doc,
        "application/vnd.ms-excel" => Format::Xls,
        "application/rtf" | "text/rtf" => Format::Rtf,
        "application/x-ole-storage" | "application/octet-stream" | "application/zip" => {
            match ext? {
                "doc" => Format::Doc,
                "xls" => Format::Xls,
                "docx" | "docm" => Format::Docx,
                "xlsx" | "xlsm" => Format::Xlsx,
                "pptx" => Format::Pptx,
                "odt" | "ods" | "odp" => Format::OpenDocument,
                "rtf" => Format::Rtf,
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// What a tool made of one file.
#[derive(Debug, PartialEq)]
pub struct Extracted {
    pub tool: String,
    pub outcome: String,
    pub text: Option<String>,
}

fn done(tool: String, out: Result<Vec<u8>, String>) -> Extracted {
    match out {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let outcome = match text.trim().is_empty() {
                true => "empty",
                false if text.len() > MAX_TEXT => "truncated",
                false => "ok",
            };
            Extracted {
                tool,
                outcome: outcome.into(),
                text: (outcome != "empty").then(|| message::cap(text)),
            }
        }
        Err(e) => Extracted {
            tool,
            outcome: e,
            text: None,
        },
    }
}

/// The text of `bytes` in format `f`; `tmp` is a scratch directory for the
/// file handed to a sandboxed tool.
pub fn extract(f: Format, bytes: &[u8], tmp: &Path, tools: &Tools) -> Extracted {
    let (tool, args): (&str, &[&str]) = match f {
        Format::Docx | Format::Xlsx | Format::Pptx | Format::OpenDocument => {
            return done("office-xml".into(), office_xml(f, bytes));
        }
        Format::Pdf => ("pdftotext", &["-enc", "UTF-8", "/in", "-"]),
        Format::Doc => ("catdoc", &["-d", "utf-8", "/in"]),
        Format::Xls => ("xls2csv", &["-d", "utf-8", "/in"]),
        Format::Rtf => ("pandoc", &["-f", "rtf", "-t", "plain", "/in"]),
    };
    let mut cmd = vec![tool];
    cmd.extend_from_slice(args);
    let e = done(tools.name(tool), sandboxed(bytes, &cmd, tmp));
    if f == Format::Doc && e.text.is_none() {
        let again = done(
            "antiword".into(),
            sandboxed(bytes, &["antiword", "/in"], tmp),
        );
        if again.text.is_some() {
            return again;
        }
    }
    e
}

/// Tool names with versions, found once per run.
pub struct Tools(HashMap<&'static str, String>);

impl Tools {
    pub fn find() -> Tools {
        let probe = |tool: &'static str, arg: &str| {
            let out = Command::new(tool).arg(arg).output().ok()?;
            let all = [out.stdout, out.stderr].concat();
            let first = String::from_utf8_lossy(&all).lines().next()?.to_string();
            let v = first
                .split_whitespace()
                .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
            Some((tool, format!("{tool} {v}")))
        };
        let mut m: HashMap<_, _> = [
            ("pdftotext", "-v"),
            ("catdoc", "-V"),
            ("pandoc", "--version"),
        ]
        .into_iter()
        .filter_map(|(t, a)| probe(t, a))
        .collect();
        if Command::new("bwrap").arg("--version").output().is_err() {
            warn!("bwrap not found; PDF, .doc, .xls and .rtf attachments are not extracted");
            m.clear();
            m.insert("bwrap", String::new());
        }
        Tools(m)
    }

    fn name(&self, tool: &str) -> String {
        self.0
            .get(tool)
            .cloned()
            .unwrap_or_else(|| tool.to_string())
    }

    fn sandbox(&self) -> bool {
        !self.0.contains_key("bwrap")
    }
}

/// Run `cmd` under bwrap on a read-only copy of `bytes` at `/in`: no
/// network or other namespaces, only /usr, capped time, memory and output.
fn sandboxed(bytes: &[u8], cmd: &[&str], tmp: &Path) -> Result<Vec<u8>, String> {
    std::fs::write(tmp.join("in"), bytes).map_err(|e| format!("failed: writing input: {e}"))?;
    let input = tmp.join("in");
    let mut c = Command::new("bwrap");
    c.args(["--ro-bind", "/usr", "/usr"])
        .args([
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
        ])
        .args([
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/sbin",
            "/sbin",
        ])
        .args(["--ro-bind-try", "/etc/alternatives", "/etc/alternatives"])
        .args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"])
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ])
        .args(["--setenv", "PATH", "/usr/bin", "--setenv", "HOME", "/tmp"])
        .arg("--ro-bind")
        .arg(&input)
        .arg("/in")
        .args(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe setrlimit calls between fork and exec.
    unsafe {
        c.pre_exec(|| {
            let mem = libc::rlimit {
                rlim_cur: MAX_MEMORY,
                rlim_max: MAX_MEMORY,
            };
            let cpu = libc::rlimit {
                rlim_cur: TIMEOUT.as_secs(),
                rlim_max: TIMEOUT.as_secs(),
            };
            if libc::setrlimit(libc::RLIMIT_DATA, &mem) != 0
                || libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = c
        .spawn()
        .map_err(|e| format!("failed: starting bwrap: {e}"))?;
    let drain = |r: Option<Box<dyn Read + Send>>, cap: usize| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut r) = r {
                let mut buf = [0u8; 65536];
                while let Ok(n @ 1..) = r.read(&mut buf) {
                    if out.len() < cap {
                        out.extend_from_slice(&buf[..n]);
                    }
                }
            }
            out
        })
    };
    let stdout = drain(child.stdout.take().map(|s| Box::new(s) as _), MAX_TEXT + 1);
    let stderr = drain(child.stderr.take().map(|s| Box::new(s) as _), 4096);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("failed: {e}")),
        }
    };
    let _ = std::fs::remove_file(&input);
    let (out, err) = (
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    );
    match status {
        None => Err("timeout".into()),
        Some(s) if s.success() => Ok(out),
        Some(s) => {
            let msg = String::from_utf8_lossy(&err);
            let line = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            Err(format!(
                "failed: {s}: {}",
                line.chars().take(200).collect::<String>()
            ))
        }
    }
}

/// Text of an OOXML or OpenDocument file from the XML members that hold it.
fn office_xml(f: Format, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("failed: {e}"))?;
    let mut names: Vec<String> = z
        .file_names()
        .filter_map(|n| n.ok().map(|n| n.into_owned()))
        .filter(|n| match f {
            Format::Docx => {
                let parts = [
                    "document",
                    "header",
                    "footer",
                    "footnotes",
                    "endnotes",
                    "comments",
                ];
                n.ends_with(".xml") && parts.iter().any(|p| n.starts_with(&format!("word/{p}")))
            }
            Format::Xlsx => n == "xl/sharedStrings.xml" || n.starts_with("xl/worksheets/sheet"),
            Format::Pptx => n.starts_with("ppt/slides/slide") && n.ends_with(".xml"),
            _ => n == "content.xml",
        })
        .collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        let mut xml = String::new();
        let file = z.by_name(&n).map_err(|e| format!("failed: {n}: {e}"))?;
        if file.take(MAX_XML).read_to_string(&mut xml).is_err() {
            continue;
        }
        // Worksheet cells hold numbers and indexes into sharedStrings; keep only inline strings.
        let only_t = f == Format::Xlsx && n != "xl/sharedStrings.xml";
        xml_text(&xml, only_t, &mut out);
        if out.len() > MAX_TEXT {
            break;
        }
    }
    Ok(out.into_bytes())
}

/// Character data of `xml`, a newline after each paragraph, row or string
/// item; with `only_t`, only inside `<t>` elements.
fn xml_text(xml: &str, only_t: bool, out: &mut String) {
    let mut rest = xml;
    let mut in_t = false;
    while let Some(lt) = rest.find('<') {
        if !only_t || in_t {
            push_unescaped(&rest[..lt], out);
        }
        let Some(gt) = rest[lt..].find('>') else {
            return;
        };
        let tag = &rest[lt + 1..lt + gt];
        let name = tag
            .trim_start_matches('/')
            .split([' ', '/', '\t', '\n'])
            .next()
            .unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == "t" {
            in_t = !tag.starts_with('/') && !tag.ends_with('/');
        }
        if tag.starts_with('/') && matches!(local, "p" | "h" | "si" | "row" | "tr" | "table-row") {
            out.push('\n');
        } else if matches!(local, "tab" | "br" | "c" | "table-cell" | "s") {
            out.push(' ');
        }
        rest = &rest[lt + gt + 1..];
    }
}

fn push_unescaped(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let end = after.find(';').filter(|&e| e <= 10);
        let c = end.and_then(|e| match &after[..e] {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            n => {
                let n = n.strip_prefix('#')?;
                let v = match n.strip_prefix('x') {
                    Some(h) => u32::from_str_radix(h, 16).ok()?,
                    None => n.parse().ok()?,
                };
                char::from_u32(v)
            }
        });
        match (c, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
}

#[derive(Debug, Default)]
pub struct Stats {
    pub documents: u64,
    pub with_text: u64,
    pub failed: u64,
}

/// Extract every document part not yet extracted by this `VERSION`, and mark
/// the messages holding them for re-indexing of their text.
pub async fn run(store: &Store) -> Result<Stats> {
    let t = Instant::now();
    let mut rows = store
        .conn
        .query(
            "SELECT p.hash, p.msg_id, p.ordinal, coalesce(p.detected_type, p.declared_type), p.ext
             FROM part p LEFT JOIN extract e ON e.hash = p.hash
             WHERE p.kind != 'body' AND e.version IS NOT ?1",
            (VERSION,),
        )
        .await?;
    let mut seen = HashSet::new();
    let mut todo = Vec::new();
    while let Some(r) = rows.next().await? {
        let Value::Blob(hash) = r.get_value(0)? else {
            continue;
        };
        let (t, ext) = (r.get::<String>(3)?, r.get::<Option<String>>(4)?);
        if let Some(f) = format(&t, ext.as_deref())
            && seen.insert(hash.clone())
        {
            todo.push((hash, r.get::<i64>(1)?, r.get::<i64>(2)? as usize, f));
        }
    }
    drop(rows);
    let mut stats = Stats::default();
    if todo.is_empty() {
        return Ok(stats);
    }
    let tools = Tools::find();
    let tmp = store.data_dir.join("tmp");
    let c = &store.conn;
    let mut first = c
        .prepare("SELECT min(artifact_id) FROM artifact WHERE msg_id = ?1")
        .await?;
    let mut sel = c
        .prepare("SELECT header_hash, body_hash, subject FROM artifact WHERE artifact_id = ?1")
        .await?;
    let mut ins = c
        .prepare(
            "INSERT OR REPLACE INTO extract (hash, version, tool, outcome, text)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .await?;
    let mut stale = c
        .prepare(
            "UPDATE message SET text_version = NULL
             WHERE msg_id IN (SELECT msg_id FROM part WHERE hash = ?1)",
        )
        .await?;
    for batch in todo.chunks(BATCH) {
        let mut docs = Vec::with_capacity(batch.len());
        for (hash, msg, ordinal, f) in batch {
            let mut rows = first.query((*msg,)).await?;
            let id: i64 = rows.next().await?.expect("message has a copy").get(0)?;
            drop(rows);
            let (raw, _, _) = stored(store, &mut sel, id).await?;
            docs.push((hash, *f, message::leaf(&raw, *ordinal).unwrap_or_default()));
        }
        let results: Vec<Extracted> = docs
            .par_iter()
            .enumerate()
            .map(|(i, (_, f, bytes))| {
                if !tools.sandbox()
                    && !matches!(
                        f,
                        Format::Docx | Format::Xlsx | Format::Pptx | Format::OpenDocument
                    )
                {
                    return done("bwrap".into(), Err("failed: bwrap not found".into()));
                }
                let dir = tmp.join(format!("extract-{}-{i}", std::process::id()));
                let e = std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("failed: {}: {e}", dir.display()))
                    .map(|()| extract(*f, bytes, &dir, &tools));
                let _ = std::fs::remove_dir(&dir);
                e.unwrap_or_else(|e| done("munchymail".into(), Err(e)))
            })
            .collect();
        store.begin().await?;
        for ((hash, _, _), e) in docs.iter().zip(&results) {
            ins.execute((
                hash.to_vec(),
                VERSION,
                e.tool.as_str(),
                e.outcome.as_str(),
                e.text.clone(),
            ))
            .await?;
            stale.execute((hash.to_vec(),)).await?;
            stats.with_text += e.text.is_some() as u64;
            stats.failed += (e.outcome.starts_with("failed") || e.outcome == "timeout") as u64;
        }
        store.commit().await?;
        stats.documents += batch.len() as u64;
        info!(done = stats.documents, of = todo.len(), elapsed = ?t.elapsed(), "extracting documents");
    }
    let _ = std::fs::remove_dir(&tmp);
    Ok(stats)
}

/// Fill the text of `d`'s parts (message `msg`) from `extract`.
pub async fn fill(store: &Store, msg: i64, d: &mut Decoded) -> Result<()> {
    for (i, text) in texts(store, msg).await? {
        if let Some(p) = d.parts.get_mut(i) {
            p.text.get_or_insert(text);
        }
    }
    Ok(())
}

/// Extracted text of message `msg`'s parts, by part ordinal.
pub async fn texts(store: &Store, msg: i64) -> Result<Vec<(usize, String)>> {
    let mut rows = store
        .conn
        .query(
            "SELECT p.ordinal, e.text FROM part p JOIN extract e ON e.hash = p.hash
             WHERE p.msg_id = ?1 AND e.text IS NOT NULL",
            (msg,),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push((r.get::<i64>(0)? as usize, r.get::<String>(1)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in files {
            z.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    #[test]
    fn office_documents() {
        let docx = zip_of(&[(
            "word/document.xml",
            "<w:document><w:body><w:p><w:r><w:t>Buy</w:t></w:r><w:r><w:t xml:space=\"preserve\"> kumquats &amp; figs</w:t></w:r></w:p><w:p><w:r><w:t>Next</w:t></w:r></w:p></w:body></w:document>",
        )]);
        let t = office_xml(Format::Docx, &docx).unwrap();
        assert_eq!(String::from_utf8(t).unwrap(), "Buy kumquats & figs\nNext\n");
        let xlsx = zip_of(&[
            (
                "xl/sharedStrings.xml",
                "<sst><si><t>name</t></si><si><t>quokka</t></si></sst>",
            ),
            (
                "xl/worksheets/sheet1.xml",
                "<sheetData><row><c t=\"s\"><v>0</v></c><c t=\"inlineStr\"><is><t>inline</t></is></c><c><v>42</v></c></row></sheetData>",
            ),
        ]);
        let t = String::from_utf8(office_xml(Format::Xlsx, &xlsx).unwrap()).unwrap();
        assert_eq!(
            t.split_whitespace().collect::<Vec<_>>(),
            ["name", "quokka", "inline"]
        );
        let e = done("x".into(), office_xml(Format::Docx, b"not a zip"));
        assert!(e.outcome.starts_with("failed"), "{e:?}");
        assert_eq!(done("x".into(), Ok(b"  \n".to_vec())).outcome, "empty");
        assert_eq!(
            format("application/octet-stream", Some("doc")),
            Some(Format::Doc)
        );
        assert_eq!(format("application/octet-stream", Some("js")), None);
        assert_eq!(
            format("application/vnd.oasis.opendocument.text", None),
            Some(Format::OpenDocument)
        );
    }
}
