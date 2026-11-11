//! Full-text index in `data/text/` (Tantivy): one document per message with
//! its Subject and the text of its text parts, attached messages and
//! documents (`extract`). Derived
//! from stored bytes; `message.text_version` says which messages are stale.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use rayon::prelude::*;
use tantivy::collector::DocSetCollector;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term};
use tracing::info;

use crate::extract;
use crate::message::{self, Decoded};
use crate::parts::{Stats, stored};
use crate::store::Store;

/// Bump when what gets indexed changes, so `index` redoes every message.
pub const VERSION: i64 = 1;

const BATCH: usize = 2000;
const WRITER_HEAP: usize = 256 << 20;
const SNIPPET_CHARS: usize = 160;

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("text")
}

struct Fields {
    msg: Field,
    subject: Field,
    text: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let words = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("en_stem")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let f = Fields {
        msg: b.add_u64_field("msg", INDEXED | FAST),
        subject: b.add_text_field("subject", words.clone()),
        text: b.add_text_field("text", words),
    };
    (b.build(), f)
}

fn open(path: &Path) -> Result<(Index, Fields)> {
    let (schema, f) = schema();
    let index = Index::open_in_dir(path)?;
    if index.schema() != schema {
        bail!(
            "{} has another schema; delete it and run `munchymail index`",
            path.display()
        );
    }
    Ok((index, f))
}

/// Messages not yet in the text index, or indexed by an older `VERSION`.
pub async fn pending(store: &Store) -> Result<i64> {
    let mut rows = store
        .conn
        .query(
            "SELECT count(*) FROM message WHERE text_version IS NOT ?1",
            (VERSION,),
        )
        .await?;
    Ok(rows.next().await?.map_or(Ok(0), |r| r.get(0))?)
}

/// The text worth searching: HTML bodies only where there is no plain one
/// (they are usually alternatives), then every other part's text.
pub fn texts(d: &Decoded) -> Vec<(Option<usize>, &str)> {
    use message::Kind::Body;
    if d.parts.is_empty() {
        return vec![(None, d.text.as_str())];
    }
    let plain = d
        .parts
        .iter()
        .any(|p| p.kind == Body && p.content_type == "text/plain");
    d.parts
        .iter()
        .enumerate()
        .filter(|(_, p)| !(plain && p.kind == Body && p.content_type == "text/html"))
        .filter_map(|(i, p)| Some((Some(i), p.text.as_deref()?)))
        .collect()
}

/// Bring the text index up to date, creating it if `data/text/` is missing.
pub async fn index(store: &Store) -> Result<Stats> {
    let t = Instant::now();
    let path = dir(&store.data_dir);
    let (index, f) = if path.join("meta.json").exists() {
        open(&path)?
    } else {
        store
            .conn
            .execute("UPDATE message SET text_version = NULL", ())
            .await?;
        std::fs::create_dir_all(&path)?;
        let (schema, f) = schema();
        (Index::create_in_dir(&path, schema)?, f)
    };
    let mut todo = Vec::new();
    let mut rows = store
        .conn
        .query(
            "SELECT a.msg_id, min(a.artifact_id) FROM artifact a
             JOIN message m ON m.msg_id = a.msg_id
             WHERE m.text_version IS NOT ?1 GROUP BY a.msg_id",
            (VERSION,),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        todo.push((r.get::<i64>(0)?, r.get::<i64>(1)?));
    }
    drop(rows);
    let mut stats = Stats::default();
    if todo.is_empty() {
        return Ok(stats);
    }
    let mut writer: IndexWriter = index.writer(WRITER_HEAP)?;
    let c = &store.conn;
    let mut sel = c
        .prepare("SELECT header_hash, body_hash, subject FROM artifact WHERE artifact_id = ?1")
        .await?;
    let mut done = c
        .prepare("UPDATE message SET text_version = ?1 WHERE msg_id = ?2")
        .await?;
    for batch in todo.chunks(BATCH) {
        let mut raw = Vec::with_capacity(batch.len());
        for &(msg, id) in batch {
            let (bytes, body, subject) = stored(store, &mut sel, id).await?;
            let extracted = extract::texts(store, msg).await?;
            raw.push((msg, bytes, body, subject, extracted));
        }
        let docs: Vec<(TantivyDocument, usize)> = raw
            .par_iter()
            .map(|(msg, bytes, body, subject, extracted)| {
                let mut d = message::decode(bytes, body);
                for (i, t) in extracted {
                    if let Some(p) = d.parts.get_mut(*i) {
                        p.text.get_or_insert_with(|| t.clone());
                    }
                }
                let mut doc = TantivyDocument::default();
                doc.add_u64(f.msg, *msg as u64);
                doc.add_text(f.subject, subject.as_deref().unwrap_or(""));
                let texts = texts(&d);
                for (_, t) in &texts {
                    doc.add_text(f.text, t);
                }
                (doc, texts.len())
            })
            .collect();
        for (&(msg, _), (doc, n)) in batch.iter().zip(docs) {
            writer.delete_term(Term::from_field_u64(f.msg, msg as u64));
            writer.add_document(doc)?;
            stats.parts += n as u64;
        }
        writer.commit()?;
        store.begin().await?;
        for &(msg, _) in batch {
            done.execute((VERSION, msg)).await?;
        }
        store.commit().await?;
        stats.messages += batch.len() as u64;
        if stats.messages % 20_000 < BATCH as u64 {
            info!(done = stats.messages, of = todo.len(), elapsed = ?t.elapsed(), "indexing text");
        }
    }
    writer.wait_merging_threads()?;
    Ok(stats)
}

/// A parsed `--text` query against the index, for filtering and snippets.
pub struct TextQuery {
    pub query: String,
    searcher: Searcher,
    parsed: Box<dyn Query>,
    snippets: SnippetGenerator,
    subject_snippets: SnippetGenerator,
}

impl fmt::Debug for TextQuery {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TextQuery({:?})", self.query)
    }
}

/// Where a `--text` query matched in one message.
pub struct Match {
    /// Index into `Decoded::parts`; None for the subject or an unparsed body.
    pub part: Option<usize>,
    pub subject: bool,
    pub fragment: String,
    /// Byte ranges of `fragment` that matched.
    pub highlighted: Vec<Range<usize>>,
}

impl TextQuery {
    pub fn new(data_dir: &Path, query: &str) -> Result<TextQuery> {
        let path = dir(data_dir);
        if !path.join("meta.json").exists() {
            bail!(
                "no text index in {}; run `munchymail index`",
                path.display()
            );
        }
        let (index, f) = open(&path)?;
        let mut parser = QueryParser::for_index(&index, vec![f.subject, f.text]);
        parser.set_conjunction_by_default();
        let parsed = parser.parse_query(query).map_err(|e| {
            anyhow!(
                "bad --text query {query:?}: {e}; words all match by default, \
                 also \"a phrase\", a OR b, -word, subject:word"
            )
        })?;
        let reader: IndexReader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let searcher = reader.searcher();
        let mut snippets = SnippetGenerator::create(&searcher, &*parsed, f.text)?;
        snippets.set_max_num_chars(SNIPPET_CHARS);
        let subject_snippets = SnippetGenerator::create(&searcher, &*parsed, f.subject)?;
        Ok(TextQuery {
            query: query.to_string(),
            searcher,
            parsed,
            snippets,
            subject_snippets,
        })
    }

    /// Every matching message id.
    pub fn messages(&self) -> Result<Vec<i64>> {
        let docs = self.searcher.search(&*self.parsed, &DocSetCollector)?;
        let mut cols = Vec::new();
        for s in self.searcher.segment_readers() {
            cols.push(s.fast_fields().u64("msg")?);
        }
        let mut out: Vec<i64> = docs
            .into_iter()
            .filter_map(|d| cols[d.segment_ord as usize].first(d.doc_id))
            .map(|m| m as i64)
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The best fragment among a message's texts (already sanitised), first
    /// text first; else the subject's.
    pub fn find(&self, texts: &[(Option<usize>, String)], subject: &str) -> Option<Match> {
        let hit = |part, subject, text: &str| {
            let g = if subject {
                &self.subject_snippets
            } else {
                &self.snippets
            };
            let s = g.snippet(text);
            (!s.is_empty()).then(|| Match {
                part,
                subject,
                fragment: s.fragment().to_string(),
                highlighted: s.highlighted().to_vec(),
            })
        };
        texts
            .iter()
            .find_map(|(p, t)| hit(*p, false, t))
            .or_else(|| hit(None, true, subject))
    }
}
