use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use turso::Value;

use munchymail::import::Importer;
use munchymail::query::{Filter, sieve_to_like};
use munchymail::report::{self, Key};
use munchymail::store::{Store, now};
use munchymail::util::{hex, ymd};
use munchymail::{mbox, source};

#[derive(Parser)]
#[command(
    about = "Offline mail corpus: import, dedupe, search",
    after_help = "Logging goes to stderr; set MUNCHYMAIL_LOG (e.g. munchymail=debug) to change it."
)]
struct Cli {
    /// Data directory holding email.sqlite and content/.
    #[arg(long, env = "MUNCHYMAIL_DATA", default_value = "data", global = true)]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage origins: places mail is found (live trees, backups, exports).
    #[command(subcommand)]
    Origin(OriginCmd),
    /// Import every mbox under the given origins, or all origins if none given.
    ///
    /// Each file is read under a shared fcntl lock (as Dovecot readers do) and
    /// never written. Re-importing unchanged files only records a new sighting.
    Import {
        /// Origin labels; default all.
        labels: Vec<String>,
    },
    /// List matching messages, newest first.
    Search {
        #[command(flatten)]
        filter: FilterArgs,
        /// Print each message's raw header block instead of a summary line.
        #[arg(long, conflicts_with_all = ["raw", "count"])]
        headers: bool,
        /// Print each full raw message.
        #[arg(long, conflicts_with = "count")]
        raw: bool,
        /// Print only the number of matches.
        #[arg(long)]
        count: bool,
        /// Maximum messages listed.
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: i64,
    },
    /// Most common values of a header among matching messages (any occurrence).
    Top {
        /// Header name, e.g. list-id.
        name: String,
        #[command(flatten)]
        filter: FilterArgs,
        /// Maximum values listed.
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: i64,
    },
    /// Header names and how many matching messages carry each.
    Names {
        /// Only header names matching this Sieve pattern, e.g. 'x-*'.
        #[arg(long, value_name = "PATTERN")]
        like: Option<String>,
        #[command(flatten)]
        filter: FilterArgs,
        /// Maximum names listed.
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: i64,
    },
    /// Count matching messages grouped by one or more header keys.
    ///
    /// %scope is against every message in the --origin/--folder/--since/--before
    /// scope; %match is against those also passing the header filters.
    Report {
        /// Group key, repeatable: NAME, NAME:addr, NAME:domain, or a match/no-match test
        /// NAME~REGEX (Sieve :regex) or NAME=PATTERN (Sieve :matches).
        #[arg(long = "by", value_name = "KEY", required = true)]
        by: Vec<String>,
        #[command(flatten)]
        filter: FilterArgs,
        /// Maximum groups listed; the rest are summed on one line.
        #[arg(short = 'n', long, default_value_t = 40)]
        limit: usize,
        /// Tab-separated output without truncation.
        #[arg(long)]
        tsv: bool,
    },
    /// Every header among matching messages with its value distribution:
    /// categorical headers first, then constants, then near-unique identifiers.
    Profile {
        /// Only header names matching this Sieve pattern, e.g. 'x-*'.
        #[arg(long, value_name = "PATTERN")]
        like: Option<String>,
        #[command(flatten)]
        filter: FilterArgs,
        /// Values shown per header.
        #[arg(short = 'k', long, default_value_t = 6)]
        values: usize,
    },
    /// Show one message: its raw header block by default.
    Show {
        /// Message id, the first column of `search`.
        id: i64,
        /// Exact raw message bytes (pipe into sieve-test).
        #[arg(long)]
        raw: bool,
        /// Where this message was found.
        #[arg(long = "where")]
        provenance: bool,
    },
    /// Corpus counts.
    Stats,
    /// Run SQL against the index and print rows tab-separated.
    Sql {
        /// One SQL statement; see docs/index.md for the schema.
        query: String,
    },
    /// Compare two snapshots of an mbox message by message; no database.
    ///
    /// Messages pair up as unchanged (identical bytes), headers changed (same
    /// body and Message-ID), removed (moved if found in a --moved-to file,
    /// otherwise not found) or added.
    Diff {
        /// The older copy.
        before: PathBuf,
        /// The newer copy.
        after: PathBuf,
        /// Where removed messages may have gone; repeatable.
        #[arg(long = "moved-to", value_name = "MBOX")]
        moved_to: Vec<PathBuf>,
        /// Group key for the breakdowns, as in `report`; repeatable.
        #[arg(long = "by", value_name = "KEY", default_value = "from:addr")]
        by: Vec<String>,
        /// Groups shown per section.
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: usize,
        /// Also list every message in each section.
        #[arg(long)]
        list: bool,
    },
    /// Split mbox files and check they reproduce byte-for-byte; no database.
    Scan {
        /// Also list every message's offset, sizes and From_ line.
        #[arg(long)]
        list: bool,
        /// Mbox files to split.
        paths: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
enum OriginCmd {
    /// Register a place mail is found; nothing is read until `import`.
    Add {
        /// Short unique name used by --origin and import.
        label: String,
        /// Directory tree (walked, dot-entries skipped) or a single mbox file.
        root: PathBuf,
        /// Machine the origin lives on or came from.
        #[arg(long)]
        host: Option<String>,
        /// Free text, e.g. "2019 laptop backup".
        #[arg(long)]
        note: Option<String>,
    },
    /// Mark an origin finished (e.g. its path moved): `import` skips it and
    /// `--current` ignores it unless named with `--origin`; its runs stay queryable.
    Retire {
        label: String,
        /// Make it active again.
        #[arg(long)]
        undo: bool,
    },
    /// List origins and when each was last imported.
    List,
}

/// Filters; all given must match. PATTERNs use Sieve `:matches` (`*`, `?`, ASCII
/// case-insensitive) and mean `:contains` when they have no wildcard.
#[derive(Args, Clone, Default)]
struct FilterArgs {
    /// NAME:PATTERN on decoded header values (Sieve `header`); repeatable.
    #[arg(short = 'H', long = "header", value_name = "NAME:PATTERN")]
    header: Vec<String>,
    /// NAME:REGEX on decoded header values, case-insensitive (Sieve `header :regex`); repeatable.
    #[arg(short = 'R', long = "regex", value_name = "NAME:REGEX")]
    regex: Vec<String>,
    /// Header must be present; repeatable.
    #[arg(long, value_name = "NAME")]
    has: Vec<String>,
    /// Any address in From (Sieve `address :all`), e.g. '*@github.com'.
    #[arg(long, value_name = "PATTERN")]
    from: Option<String>,
    /// Any address in To.
    #[arg(long, value_name = "PATTERN")]
    to: Option<String>,
    /// Decoded Subject, same as -H subject:PATTERN.
    #[arg(long, value_name = "PATTERN")]
    subject: Option<String>,
    /// Path within an origin where a copy was ever found, e.g. 'lists/*'.
    #[arg(long, value_name = "PATTERN")]
    folder: Option<String>,
    /// Only messages found in this origin.
    #[arg(long, value_name = "LABEL")]
    origin: Option<String>,
    /// Only where mail is now: --origin/--folder match each origin's latest import, not any past one.
    #[arg(long)]
    current: bool,
    /// Date header on or after YYYY-MM-DD (UTC).
    #[arg(long)]
    since: Option<String>,
    /// Date header before YYYY-MM-DD (UTC).
    #[arg(long)]
    before: Option<String>,
}

impl FilterArgs {
    fn build(self) -> Result<Filter> {
        let mut headers = Vec::new();
        for h in self.header {
            let Some((name, pat)) = h.split_once(':') else {
                bail!("--header wants NAME:PATTERN, got {h:?}");
            };
            headers.push((name.trim().to_string(), pat.to_string()));
        }
        if let Some(p) = self.subject {
            headers.push(("subject".to_string(), p));
        }
        let mut regexes = Vec::new();
        for r in self.regex {
            let Some((name, re)) = r.split_once(':') else {
                bail!("--regex wants NAME:REGEX, got {r:?}");
            };
            regexes.push((name.trim().to_string(), re.to_string()));
        }
        let mut addresses = Vec::new();
        for (name, pat) in [("from", self.from), ("to", self.to)] {
            if let Some(p) = pat {
                addresses.push((name.to_string(), p));
            }
        }
        Ok(Filter {
            headers,
            addresses,
            regexes,
            has: self.has,
            folder: self.folder,
            origin: self.origin,
            current: self.current,
            since: self.since,
            before: self.before,
        })
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) if b.len() <= 32 => hex(b),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).chain(['…']).collect()
    }
}

async fn query_rows(store: &Store, sql: &str, params: Vec<Value>) -> Result<Vec<Vec<Value>>> {
    let mut stmt = store.conn.prepare(sql).await?;
    let n = stmt.column_count();
    let mut rows = stmt.query(params).await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push(
            (0..n)
                .map(|i| r.get_value(i))
                .collect::<turso::Result<_>>()?,
        );
    }
    Ok(out)
}

async fn origin(store: &Store, cmd: OriginCmd, out: &mut impl Write) -> Result<()> {
    match cmd {
        OriginCmd::Add {
            label,
            root,
            host,
            note,
        } => {
            let root = root
                .canonicalize()
                .with_context(|| format!("resolving {}", root.display()))?;
            if root.starts_with(store.data_dir.canonicalize()?) {
                bail!("origin {} is inside the data directory", root.display());
            }
            store.begin().await?;
            store
                .conn
                .execute(
                    "INSERT INTO origin (label, root, host, note, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    (label.as_str(), root.to_string_lossy().as_ref(), host, note, now()),
                )
                .await
                .with_context(|| format!("adding origin {label:?}"))?;
            store.commit().await?;
            writeln!(out, "added origin {label} -> {}", root.display())?;
        }
        OriginCmd::Retire { label, undo } => {
            store.begin().await?;
            let n = store
                .conn
                .execute(
                    "UPDATE origin SET retired_at = ?1 WHERE label = ?2",
                    ((!undo).then(now), label.as_str()),
                )
                .await?;
            store.commit().await?;
            if n == 0 {
                bail!("no origin labelled {label:?}");
            }
            let state = if undo { "active again" } else { "retired" };
            writeln!(out, "origin {label} {state}")?;
        }
        OriginCmd::List => {
            let rows = query_rows(
                store,
                "SELECT g.label, g.root, g.host, g.note,
                        (SELECT max(started_at) FROM import_run r WHERE r.origin_id = g.origin_id),
                        g.retired_at
                 FROM origin g ORDER BY g.label",
                vec![],
            )
            .await?;
            for r in rows {
                let last = match r[4] {
                    Value::Integer(t) => ymd(t),
                    _ => "never".into(),
                };
                let retired = match r[5] {
                    Value::Integer(t) => format!("\tretired {}", ymd(t)),
                    _ => String::new(),
                };
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}\tlast import {last}{retired}",
                    text(&r[0]),
                    text(&r[1]),
                    text(&r[2]),
                    text(&r[3])
                )?;
            }
        }
    }
    Ok(())
}

async fn search(
    store: &Store,
    filter: Filter,
    headers: bool,
    raw: bool,
    count: bool,
    limit: i64,
    out: &mut impl Write,
) -> Result<()> {
    let set = filter.materialize(&store.conn).await?;
    if count {
        let rows = query_rows(store, &format!("SELECT count(*) FROM {set}"), vec![]).await?;
        writeln!(out, "{}", text(&rows[0][0]))?;
        return Ok(());
    }
    let from = match set {
        "hit" => "hit JOIN artifact a ON a.artifact_id = hit.artifact_id",
        _ => "artifact a",
    };
    let rows = query_rows(
        store,
        &format!(
            "SELECT a.artifact_id, a.date, a.sender, a.subject, a.header_hash, a.body_hash
             FROM {from} ORDER BY a.date DESC LIMIT ?"
        ),
        vec![limit.into()],
    )
    .await?;
    for r in rows {
        if headers || raw {
            writeln!(out, "==> {} <==", text(&r[0]))?;
            if let (Value::Blob(h), Value::Blob(b)) = (&r[4], &r[5]) {
                out.write_all(&store.object(h).await?)?;
                if raw {
                    out.write_all(&store.object(b).await?)?;
                    writeln!(out)?;
                }
            }
        } else {
            let date = match r[1] {
                Value::Integer(t) => ymd(t),
                _ => "----------".into(),
            };
            writeln!(
                out,
                "{:>7}  {date}  {:<32}  {}",
                text(&r[0]),
                truncate(&text(&r[2]), 32),
                text(&r[3])
            )?;
        }
    }
    Ok(())
}

fn pct(n: u64, of: u64) -> String {
    if of == 0 {
        "-".into()
    } else {
        format!("{:.1}%", n as f64 * 100.0 / of as f64)
    }
}

async fn top(
    store: &Store,
    column: &str,
    name: Option<&str>,
    like: Option<&str>,
    filter: Filter,
    limit: i64,
    out: &mut impl Write,
) -> Result<()> {
    let table = filter.materialize(&store.conn).await?;
    let matched = query_rows(store, &format!("SELECT count(*) FROM {table}"), vec![]).await?;
    let matched = match matched[0][0] {
        Value::Integer(n) => n as u64,
        _ => 0,
    };
    let from = match table {
        // Per-name counts scan hits first; per-value counts let the planner start from the name index.
        "hit" if name.is_none() => "hit CROSS JOIN header h ON h.artifact_id = hit.artifact_id",
        "hit" => "hit JOIN header h ON h.artifact_id = hit.artifact_id",
        _ => "header h",
    };
    // Per-name counts use nth = 0 so each message counts once without DISTINCT.
    let counter = if name.is_some() {
        "count(DISTINCT h.artifact_id)"
    } else {
        "count(*)"
    };
    let mut params = Vec::new();
    let mut cond = match name {
        Some(n) => {
            params.push(n.to_ascii_lowercase().into());
            "h.name = ?".to_string()
        }
        None => "h.nth = 0".to_string(),
    };
    if let Some(l) = like {
        cond.push_str(" AND h.name LIKE ? ESCAPE '\\'");
        params.push(sieve_to_like(l).into());
    }
    params.push(limit.into());
    let rows = query_rows(
        store,
        &format!(
            "SELECT h.{column}, {counter} AS n FROM {from}
             WHERE {cond} GROUP BY h.{column} ORDER BY n DESC LIMIT ?"
        ),
        params,
    )
    .await?;
    writeln!(out, "{matched} matching messages")?;
    for r in rows {
        let n = match r[1] {
            Value::Integer(n) => n as u64,
            _ => 0,
        };
        writeln!(out, "{n:>7} {:>6}  {}", pct(n, matched), text(&r[0]))?;
    }
    Ok(())
}

async fn profile(
    store: &Store,
    like: Option<&str>,
    filter: Filter,
    k: usize,
    out: &mut impl Write,
) -> Result<()> {
    let (matched, headers) = report::profile(&store.conn, &filter, like).await?;
    writeln!(
        out,
        "{matched} matching messages, {} headers (first occurrence per message)",
        headers.len()
    )?;
    let mut section = None;
    for h in &headers {
        let s = h.kind().1;
        if section != Some(s) {
            writeln!(out, "\n== {s} ==")?;
            section = Some(s);
        }
        let unique = h.all_unique();
        writeln!(
            out,
            "{}  {} msgs ({}), {} distinct{}",
            h.name,
            h.carried,
            pct(h.carried, matched),
            h.distinct(),
            if unique { ", all unique" } else { "" }
        )?;
        let shown = if unique { 1 } else { k };
        for (v, n) in h.values.iter().take(shown) {
            let v = if v.is_empty() { "(empty)" } else { v.as_str() };
            writeln!(out, "  {n:>7} {:>6}  {}", pct(*n, matched), truncate(v, 90))?;
        }
        if h.distinct() > shown {
            let rest: u64 = h.values[shown..].iter().map(|(_, n)| n).sum();
            writeln!(
                out,
                "  {rest:>7} {:>6}  ({} more values)",
                pct(rest, matched),
                h.distinct() - shown
            )?;
        }
    }
    Ok(())
}

async fn report(
    store: &Store,
    by: &[String],
    filter: Filter,
    limit: usize,
    tsv: bool,
    out: &mut impl Write,
) -> Result<()> {
    let keys = by
        .iter()
        .map(|k| Key::parse(k))
        .collect::<Result<Vec<_>>>()?;
    let r = report::run(&store.conn, &filter, &keys).await?;
    let labels: Vec<String> = keys.iter().map(Key::label).collect();
    if tsv {
        writeln!(out, "count\tpct_scope\tpct_match\t{}", labels.join("\t"))?;
        for (group, n) in &r.rows {
            writeln!(
                out,
                "{n}\t{:.4}\t{:.4}\t{}",
                *n as f64 * 100.0 / r.scope.max(1) as f64,
                *n as f64 * 100.0 / r.matched.max(1) as f64,
                group.join("\t")
            )?;
        }
        return Ok(());
    }
    writeln!(
        out,
        "scope {} messages, matched {} ({}), {} groups",
        r.scope,
        r.matched,
        pct(r.matched, r.scope),
        r.rows.len()
    )?;
    const W: usize = 48;
    let widths: Vec<usize> = (0..keys.len())
        .map(|i| {
            r.rows
                .iter()
                .take(limit)
                .map(|(g, _)| g[i].chars().count().min(W))
                .chain([labels[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<String>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let c = truncate(c, W);
                if i + 1 == cells.len() {
                    c
                } else {
                    format!("{c:<w$}", w = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    writeln!(
        out,
        "{:>7} {:>6} {:>6}  {}",
        "count",
        "%scope",
        "%match",
        line(labels.clone())
    )?;
    let mut shown = 0;
    for (group, n) in r.rows.iter().take(limit) {
        writeln!(
            out,
            "{n:>7} {:>6} {:>6}  {}",
            pct(*n, r.scope),
            pct(*n, r.matched),
            line(group.clone())
        )?;
        shown += n;
    }
    if r.rows.len() > limit {
        let rest = r.matched - shown;
        writeln!(
            out,
            "{rest:>7} {:>6} {:>6}  ({} more groups)",
            pct(rest, r.scope),
            pct(rest, r.matched),
            r.rows.len() - limit
        )?;
    }
    Ok(())
}

async fn show(
    store: &Store,
    id: i64,
    raw: bool,
    provenance: bool,
    out: &mut impl Write,
) -> Result<()> {
    let rows = query_rows(
        store,
        "SELECT header_hash, body_hash FROM artifact WHERE artifact_id = ?",
        vec![id.into()],
    )
    .await?;
    let Some(r) = rows.first() else {
        bail!("no message {id}")
    };
    let (Value::Blob(h), Value::Blob(b)) = (&r[0], &r[1]) else {
        bail!("bad hashes for {id}")
    };
    if provenance {
        let rows = query_rows(
            store,
            "SELECT g.label, s.path, o.offset, count(*), max(r.started_at)
             FROM observation o
             JOIN file_sighting s ON s.content_id = o.content_id
             JOIN origin g ON g.origin_id = s.origin_id
             JOIN import_run r ON r.run_id = s.run_id
             WHERE o.artifact_id = ?
             GROUP BY g.label, s.path, o.offset ORDER BY g.label, s.path",
            vec![id.into()],
        )
        .await?;
        for r in rows {
            let seen = match r[4] {
                Value::Integer(t) => ymd(t),
                _ => String::new(),
            };
            writeln!(
                out,
                "{}:{} @{}  seen {}x, last {seen}",
                text(&r[0]),
                text(&r[1]),
                text(&r[2]),
                text(&r[3])
            )?;
        }
        return Ok(());
    }
    out.write_all(&store.object(h).await?)?;
    if raw {
        out.write_all(&store.object(b).await?)?;
    }
    Ok(())
}

async fn stats(store: &Store, out: &mut impl Write) -> Result<()> {
    for (label, sql) in [
        ("origins", "SELECT count(*) FROM origin"),
        ("import runs", "SELECT count(*) FROM import_run"),
        ("file sightings", "SELECT count(*) FROM file_sighting"),
        ("file contents", "SELECT count(*) FROM file_content"),
        ("observations", "SELECT count(*) FROM observation"),
        ("artifacts", "SELECT count(*) FROM artifact"),
        (
            "distinct bodies",
            "SELECT count(DISTINCT body_hash) FROM artifact",
        ),
        (
            "distinct message-ids",
            "SELECT count(DISTINCT message_id) FROM artifact",
        ),
        ("header rows", "SELECT count(*) FROM header"),
        (
            "objects inline",
            "SELECT count(*) || ' / ' || coalesce(sum(length), 0) || ' bytes' FROM object WHERE inline_bytes IS NOT NULL",
        ),
        (
            "objects external",
            "SELECT count(*) || ' / ' || coalesce(sum(length), 0) || ' bytes' FROM object WHERE inline_bytes IS NULL",
        ),
    ] {
        let rows = query_rows(store, sql, vec![]).await?;
        writeln!(out, "{label:>22}: {}", text(&rows[0][0]))?;
    }
    Ok(())
}

fn diff(
    before: &Path,
    after: &Path,
    moved_to: &[PathBuf],
    by: &[String],
    limit: usize,
    list: bool,
    out: &mut impl Write,
) -> Result<()> {
    use munchymail::diff::{self, Msg};
    let keys = by
        .iter()
        .map(|k| Key::parse(k))
        .collect::<Result<Vec<_>>>()?;
    let a = diff::load(before)?;
    let b = diff::load(after)?;
    let mut elsewhere = Vec::new();
    for p in moved_to {
        elsewhere.extend(diff::load(p)?);
    }
    let d = diff::compare(&a, &b, &elsewhere);

    writeln!(out, "before {:>7}  {}", a.len(), before.display())?;
    writeln!(out, "after  {:>7}  {}", b.len(), after.display())?;
    writeln!(out, "  unchanged        {:>7}", d.unchanged)?;
    writeln!(out, "  headers changed  {:>7}", d.changed.len())?;
    writeln!(out, "  removed, moved   {:>7}", d.moved.len())?;
    writeln!(out, "  removed, NOT FOUND {:>5}", d.lost.len())?;
    writeln!(out, "  added            {:>7}", d.added.len())?;
    if !moved_to.is_empty() {
        writeln!(
            out,
            "  in --moved-to but not from before: {}",
            d.moved_to_extra
        )?;
    }

    let line = |m: &Msg| {
        let get = |n: &str| {
            m.values(n)
                .and_then(|v| v.into_iter().next())
                .unwrap_or_default()
        };
        format!(
            "    {}  {}  {}",
            truncate(&get("date"), 31),
            truncate(&get("from"), 40),
            truncate(&get("subject"), 70)
        )
    };
    let section = |title: &str, msgs: Vec<&Msg>, out: &mut dyn Write| -> Result<()> {
        if msgs.is_empty() {
            return Ok(());
        }
        writeln!(out, "\n== {title} ({}) by {} ==", msgs.len(), by.join(", "))?;
        let mut groups: HashMap<Vec<String>, u64> = HashMap::new();
        for m in &msgs {
            let g = keys
                .iter()
                .map(|k| k.group(m.values(&k.header).as_ref()))
                .collect();
            *groups.entry(g).or_default() += 1;
        }
        let mut rows: Vec<_> = groups.into_iter().collect();
        rows.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
        for (g, n) in rows.iter().take(limit) {
            writeln!(out, "  {n:>7}  {}", g.join("  "))?;
        }
        if rows.len() > limit {
            let rest: u64 = rows[limit..].iter().map(|(_, n)| n).sum();
            writeln!(out, "  {rest:>7}  ({} more groups)", rows.len() - limit)?;
        }
        if list {
            for m in &msgs {
                writeln!(out, "{}", line(m))?;
            }
        }
        Ok(())
    };
    section(
        "removed, NOT FOUND",
        d.lost.iter().map(|&i| &a[i]).collect(),
        out,
    )?;
    section(
        "removed, moved",
        d.moved.iter().map(|&i| &a[i]).collect(),
        out,
    )?;
    section("added", d.added.iter().map(|&i| &b[i]).collect(), out)?;

    if !d.changed.is_empty() {
        let mut by_names: HashMap<String, u64> = HashMap::new();
        for &(i, j) in &d.changed {
            *by_names
                .entry(diff::changed_headers(&a[i], &b[j]).join(", "))
                .or_default() += 1;
        }
        let mut rows: Vec<_> = by_names.into_iter().collect();
        rows.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
        writeln!(
            out,
            "\n== headers changed ({}) by which headers differ ==",
            d.changed.len()
        )?;
        for (names, n) in rows.iter().take(limit) {
            writeln!(out, "  {n:>7}  {names}")?;
        }
    }
    Ok(())
}

fn scan(path: &Path, list: bool, out: &mut impl Write) -> Result<()> {
    let snap = source::read_locked(path, Duration::from_secs(30))?;
    let buf = &snap.bytes;
    let parsed = mbox::split(buf)?;
    if !parsed.covers(buf.len()) {
        bail!("split does not reproduce the file byte-for-byte");
    }
    let cl = parsed
        .messages
        .iter()
        .filter(|m| m.used_content_length)
        .count();
    writeln!(
        out,
        "{}: {} bytes, {} messages, {cl} via Content-Length, preamble {} bytes{}",
        path.display(),
        buf.len(),
        parsed.messages.len(),
        parsed.preamble.len(),
        if snap.changed_during_read {
            ", CHANGED DURING READ"
        } else {
            ""
        },
    )?;
    if list {
        for (i, m) in parsed.messages.iter().enumerate() {
            writeln!(
                out,
                "  {i:>6} @{:<10} hdr {:>7} body {:>9} sep {} t={} tz={:?} {}",
                m.from_line.start,
                m.header.len(),
                m.body.len(),
                m.separator.len(),
                m.from.time,
                m.from.tz_offset_min,
                String::from_utf8_lossy(&m.from.sender),
            )?;
        }
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if let Cmd::Diff {
        before,
        after,
        moved_to,
        by,
        limit,
        list,
    } = &cli.cmd
    {
        diff(before, after, moved_to, by, *limit, *list, &mut out)?;
        return Ok(out.flush()?);
    }
    if let Cmd::Scan { list, paths } = &cli.cmd {
        for p in paths {
            scan(p, *list, &mut out).with_context(|| p.display().to_string())?;
        }
        return Ok(out.flush()?);
    }
    let store = Store::open(&cli.data).await?;
    let read_txn = matches!(
        cli.cmd,
        Cmd::Search { .. }
            | Cmd::Top { .. }
            | Cmd::Names { .. }
            | Cmd::Report { .. }
            | Cmd::Profile { .. }
            | Cmd::Show { .. }
            | Cmd::Stats
    );
    if read_txn {
        store.begin().await?;
    }
    match cli.cmd {
        Cmd::Origin(cmd) => origin(&store, cmd, &mut out).await?,
        Cmd::Import { mut labels } => {
            if labels.is_empty() {
                for r in query_rows(
                    &store,
                    "SELECT label FROM origin WHERE retired_at IS NULL ORDER BY origin_id",
                    vec![],
                )
                .await?
                {
                    labels.push(text(&r[0]));
                }
            }
            for label in labels {
                let t = Instant::now();
                let mut imp = Importer::new(&store, &label).await?;
                imp.run().await?;
                let s = &imp.stats;
                info!(
                    origin = label.as_str(),
                    files = s.files,
                    unchanged = s.known_files,
                    not_mbox = s.skipped,
                    messages = s.messages,
                    new_artifacts = s.new_artifacts,
                    bytes = s.bytes,
                    elapsed = ?t.elapsed(),
                    "import finished"
                );
            }
            let t = Instant::now();
            store.checkpoint().await?;
            info!(elapsed = ?t.elapsed(), "wal checkpoint");
        }
        Cmd::Search {
            filter,
            headers,
            raw,
            count,
            limit,
        } => {
            search(
                &store,
                filter.build()?,
                headers,
                raw,
                count,
                limit,
                &mut out,
            )
            .await?
        }
        Cmd::Top {
            name,
            filter,
            limit,
        } => {
            top(
                &store,
                "value",
                Some(&name),
                None,
                filter.build()?,
                limit,
                &mut out,
            )
            .await?
        }
        Cmd::Names {
            like,
            filter,
            limit,
        } => {
            top(
                &store,
                "name",
                None,
                like.as_deref(),
                filter.build()?,
                limit,
                &mut out,
            )
            .await?
        }
        Cmd::Report {
            by,
            filter,
            limit,
            tsv,
        } => report(&store, &by, filter.build()?, limit, tsv, &mut out).await?,
        Cmd::Profile {
            like,
            filter,
            values,
        } => profile(&store, like.as_deref(), filter.build()?, values, &mut out).await?,
        Cmd::Show {
            id,
            raw,
            provenance,
        } => show(&store, id, raw, provenance, &mut out).await?,
        Cmd::Stats => stats(&store, &mut out).await?,
        Cmd::Sql { query } => {
            for r in query_rows(&store, &query, vec![]).await? {
                writeln!(out, "{}", r.iter().map(text).collect::<Vec<_>>().join("\t"))?;
            }
        }
        Cmd::Scan { .. } | Cmd::Diff { .. } => unreachable!(),
    }
    if read_txn {
        store.commit().await?;
    }
    Ok(out.flush()?)
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("MUNCHYMAIL_LOG")
        .unwrap_or_else(|_| EnvFilter::new("munchymail=info,warn"));
    tracing_subscriber::fmt()
        .with_ansi(io::stderr().is_terminal())
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .init();
    match pollster::block_on(run(Cli::parse())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
