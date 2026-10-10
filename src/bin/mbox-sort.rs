use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use sha2::{Digest, Sha256};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use munchymail::sort::{self, SortKey};
use munchymail::{mbox, source};

/// Write a copy of an mbox with its messages sorted by date.
///
/// SRC is only read, under a shared fcntl lock. OUT must not exist. Dovecot's
/// X-UID and X-IMAPbase headers and its internal first message are dropped so
/// Dovecot renumbers the folder; every other byte of every message is kept.
#[derive(Parser)]
#[command(after_help = "Installing OUT over a Dovecot folder: see docs/mbox-sort.md.")]
struct Cli {
    /// Mbox to read.
    src: PathBuf,
    /// New file to write; refused if it exists.
    out: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = SortKey::Received)]
    key: SortKey,
    /// Report how out of order SRC is without writing anything.
    #[arg(long)]
    dry_run: bool,
}

fn sha(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// (header, body) hashes of every message, sorted, for an order-independent comparison.
fn fingerprint(
    buf: &[u8],
    parsed: &mbox::Mbox,
    skip: Option<usize>,
    strip: bool,
) -> Vec<([u8; 32], [u8; 32])> {
    let mut v: Vec<_> = parsed
        .messages
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != skip)
        .map(|(_, m)| {
            let header = if strip {
                sort::strip_uid_headers(buf, m).0
            } else {
                buf[m.header.clone()].to_vec()
            };
            (sha(&header), sha(&buf[m.body.clone()]))
        })
        .collect();
    v.sort_unstable();
    v
}

fn write_new(
    out: &Path,
    mode: fs::Permissions,
    write: impl FnOnce(&mut BufWriter<&File>) -> io::Result<()>,
) -> Result<()> {
    if out.exists() {
        bail!("{} exists; refusing to overwrite", out.display());
    }
    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = out
        .file_name()
        .context("OUT has no file name")?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.mbox-sort.{}", std::process::id()));
    let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    // Same mode as SRC (e.g. 0660 for INBOX); ownership is left to the installer.
    file.set_permissions(mode)?;
    let result = (|| -> Result<()> {
        let mut w = BufWriter::new(&file);
        write(&mut w)?;
        w.flush()?;
        drop(w);
        file.sync_all()?;
        // link() fails if OUT appeared meanwhile, so nothing is ever replaced.
        fs::hard_link(&tmp, out).with_context(|| format!("creating {}", out.display()))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    fs::remove_file(&tmp).ok();
    result
}

fn run(cli: Cli) -> Result<()> {
    if !cli.dry_run && cli.out.is_none() {
        bail!("OUT is required unless --dry-run");
    }
    let snap = source::read_locked(&cli.src, Duration::from_secs(60))?;
    let buf = &snap.bytes;
    let parsed = mbox::split(buf).with_context(|| cli.src.display().to_string())?;
    if !parsed.covers(buf.len()) {
        bail!("split of {} is not byte-exact", cli.src.display());
    }
    if !parsed.preamble.is_empty() {
        bail!(
            "{} has {} bytes before its first From line",
            cli.src.display(),
            parsed.preamble.len()
        );
    }
    let plan = sort::plan(buf, &parsed, cli.key);
    println!(
        "{}: {} messages{}, {} adjacent pairs out of order by {:?}",
        cli.src.display(),
        plan.order.len(),
        if plan.pseudo.is_some() {
            " + Dovecot internal message"
        } else {
            ""
        },
        plan.out_of_order,
        cli.key,
    );
    let Some(out) = cli.out.filter(|_| !cli.dry_run) else {
        return Ok(());
    };
    let src_id = fs::canonicalize(&cli.src)?;
    if out
        .parent()
        .and_then(|p| fs::canonicalize(p).ok())
        .map(|p| p.join(out.file_name().unwrap_or_default()))
        == Some(src_id)
    {
        bail!("OUT is SRC");
    }

    let mut stripped = 0;
    write_new(&out, fs::metadata(&cli.src)?.permissions(), |w| {
        stripped = sort::render(buf, &parsed, &plan, w)?;
        Ok(())
    })?;
    info!(out = %out.display(), stripped_uid_headers = stripped, "written");

    let written = fs::read(&out)?;
    let reparsed = mbox::split(&written)?;
    if !reparsed.covers(written.len()) || reparsed.messages.len() != plan.order.len() {
        bail!(
            "verification failed: {} does not split into {} messages",
            out.display(),
            plan.order.len()
        );
    }
    if fingerprint(buf, &parsed, plan.pseudo, true) != fingerprint(&written, &reparsed, None, false)
    {
        bail!(
            "verification failed: message bytes differ in {}",
            out.display()
        );
    }
    let left = sort::plan(&written, &reparsed, cli.key).out_of_order;
    println!(
        "{}: {} messages written, {} UID headers dropped{}, verified; {} out of order",
        out.display(),
        reparsed.messages.len(),
        stripped,
        if plan.pseudo.is_some() {
            ", internal message dropped"
        } else {
            ""
        },
        left,
    );
    Ok(())
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("MUNCHYMAIL_LOG")
        .unwrap_or_else(|_| EnvFilter::new("munchymail=info,mbox_sort=info,warn"));
    tracing_subscriber::fmt()
        .with_ansi(io::stderr().is_terminal())
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .init();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
