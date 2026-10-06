use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use munchymail::{mbox, source};
use sha2::{Digest, Sha256};

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn scan(path: &Path, list: bool) -> Result<(), Box<dyn std::error::Error>> {
    let snap = source::read_locked(path, LOCK_TIMEOUT)?;
    let buf = &snap.bytes;
    let parsed = mbox::split(buf)?;
    if !parsed.covers(buf.len()) {
        return Err("split does not reproduce the file byte-for-byte".into());
    }
    let cl = parsed
        .messages
        .iter()
        .filter(|m| m.used_content_length)
        .count();
    println!(
        "{}: {} bytes, {} messages, {} via Content-Length, preamble {} bytes{}",
        path.display(),
        buf.len(),
        parsed.messages.len(),
        cl,
        parsed.preamble.len(),
        if snap.changed_during_read {
            ", CHANGED DURING READ"
        } else {
            ""
        },
    );
    if list {
        for (i, m) in parsed.messages.iter().enumerate() {
            println!(
                "  {i:>6} @{:<10} hdr {:>7} {} body {:>9} {} sep {} t={} tz={:?} {}",
                m.from_line.start,
                m.header.len(),
                short_hash(&buf[m.header.clone()]),
                m.body.len(),
                short_hash(&buf[m.body.clone()]),
                m.separator.len(),
                m.from.time,
                m.from.tz_offset_min,
                String::from_utf8_lossy(&m.from.sender),
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("usage: munchymail scan [--list] <mbox>...");
        return ExitCode::FAILURE;
    };
    if cmd != "scan" {
        eprintln!("unknown command: {cmd}");
        return ExitCode::FAILURE;
    }
    let list = rest.iter().any(|a| a == "--list");
    let mut status = ExitCode::SUCCESS;
    for p in rest.iter().filter(|a| *a != "--list") {
        if let Err(e) = scan(Path::new(p), list) {
            eprintln!("{p}: {e}");
            status = ExitCode::FAILURE;
        }
    }
    status
}
