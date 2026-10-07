//! Reorder an mbox by date, leaving UID assignment to Dovecot (see docs/mbox-sort.md).

use std::ops::Range;

use crate::headers;
use crate::mbox::{Mbox, Message};

/// Dovecot UID bookkeeping that is only valid in the original file order.
const UID_HEADERS: [&[u8]; 2] = [b"x-uid", b"x-imapbase"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SortKey {
    /// Delivery time from the `From ` line (IMAP INTERNALDATE, "arrival").
    Received,
    /// The `Date:` header; messages without a parseable one use the `From ` line.
    Date,
}

/// Header field ranges (name line plus continuation lines) within `r`.
fn field_ranges(buf: &[u8], r: &Range<usize>) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut pos = r.start;
    while pos < r.end {
        let end = buf[pos..r.end]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(r.end, |p| pos + p + 1);
        let line = &buf[pos..end];
        if line == b"\n" || line == b"\r\n" {
            break;
        }
        match (line.first(), out.last_mut()) {
            (Some(b' ' | b'\t'), Some(last)) => last.end = end,
            _ => out.push(pos..end),
        }
        pos = end;
    }
    out
}

fn field_name(field: &[u8]) -> &[u8] {
    let name = field.split(|&b| b == b':').next().unwrap_or(field);
    name.trim_ascii()
}

/// Dovecot's internal first message, which carries `X-IMAP`.
pub fn is_pseudo(buf: &[u8], m: &Message) -> bool {
    field_ranges(buf, &m.header)
        .iter()
        .any(|f| field_name(&buf[f.clone()]).eq_ignore_ascii_case(b"x-imap"))
}

/// The header block with `X-UID`/`X-IMAPbase` fields removed; everything else byte-exact.
pub fn strip_uid_headers(buf: &[u8], m: &Message) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(m.header.len());
    let mut removed = 0;
    let mut pos = m.header.start;
    for f in field_ranges(buf, &m.header) {
        let name = field_name(&buf[f.clone()]);
        if UID_HEADERS.iter().any(|h| name.eq_ignore_ascii_case(h)) {
            out.extend_from_slice(&buf[pos..f.start]);
            pos = f.end;
            removed += 1;
        }
    }
    out.extend_from_slice(&buf[pos..m.header.end]);
    (out, removed)
}

pub fn key(buf: &[u8], m: &Message, by: SortKey) -> i64 {
    match by {
        SortKey::Received => m.from.time,
        SortKey::Date => {
            let fields = headers::fields(&buf[m.header.clone()]);
            headers::summary(&fields).date.unwrap_or(m.from.time)
        }
    }
}

pub struct Plan {
    /// Message indexes in output order (pseudo message excluded).
    pub order: Vec<usize>,
    pub pseudo: Option<usize>,
    /// Pairs of adjacent messages that were out of order in the source.
    pub out_of_order: usize,
}

pub fn plan(buf: &[u8], mbox: &Mbox, by: SortKey) -> Plan {
    let pseudo = mbox
        .messages
        .first()
        .filter(|m| is_pseudo(buf, m))
        .map(|_| 0);
    let keyed: Vec<(i64, usize)> = (0..mbox.messages.len())
        .filter(|&i| Some(i) != pseudo)
        .map(|i| (key(buf, &mbox.messages[i], by), i))
        .collect();
    let out_of_order = keyed.windows(2).filter(|w| w[1].0 < w[0].0).count();
    let mut sorted = keyed;
    // Stable on equal keys, so ties keep their file order.
    sorted.sort_by_key(|&(k, _)| k);
    Plan {
        order: sorted.into_iter().map(|(_, i)| i).collect(),
        pseudo,
        out_of_order,
    }
}

/// Serialise messages in `plan` order. Each message keeps its own `From ` line;
/// a message that ended the source without a separator gets one if it is no
/// longer last, so the next `From ` line still starts a new message.
pub fn render(
    buf: &[u8],
    mbox: &Mbox,
    plan: &Plan,
    out: &mut impl std::io::Write,
) -> std::io::Result<usize> {
    let newline: &[u8] = match mbox.messages.first() {
        Some(m) if buf[m.from_line.clone()].ends_with(b"\r\n") => b"\r\n",
        _ => b"\n",
    };
    let mut stripped = 0;
    for (n, &i) in plan.order.iter().enumerate() {
        let m = &mbox.messages[i];
        let (header, removed) = strip_uid_headers(buf, m);
        stripped += removed;
        out.write_all(&buf[m.from_line.clone()])?;
        out.write_all(&header)?;
        out.write_all(&buf[m.body.clone()])?;
        let last = n + 1 == plan.order.len();
        if m.separator.is_empty() && !last {
            out.write_all(newline)?;
        } else {
            out.write_all(&buf[m.separator.clone()])?;
        }
    }
    Ok(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mbox::split;

    const SRC: &[u8] = b"From MAILER_DAEMON  Mon Oct  6 10:00:00 2026\n\
Date: Mon, 06 Oct 2026 10:00:00 +0000\n\
From: Mail System Internal Data <MAILER-DAEMON@x>\n\
Subject: DON'T DELETE THIS MESSAGE -- FOLDER INTERNAL DATA\n\
X-IMAP: 1700000000 0000000003\n\
Status: RO\n\
\n\
internal\n\
\n\
From a@x  Wed Oct  8 09:00:00 2026\n\
Subject: third\n\
X-UID: 1\n\
Status: RO\n\
\n\
body3\n\
\n\
From b@x  Mon Oct  6 09:00:00 2026\n\
Subject: first\n\
X-UID: 2\n\
\tcontinued\n\
X-Keywords: $Label1                                                  \n\
\n\
body1\n\
\n\
From c@x  Tue Oct  7 09:00:00 2026\n\
Subject: second\n\
X-UID: 3\n\
\n\
body2";

    #[test]
    fn sorts_strips_uids_and_drops_pseudo() {
        let mbox = split(SRC).unwrap();
        let p = plan(SRC, &mbox, SortKey::Received);
        assert_eq!(p.pseudo, Some(0));
        assert_eq!(p.order, vec![2, 3, 1]);
        assert_eq!(p.out_of_order, 1);
        let mut out = Vec::new();
        assert_eq!(render(SRC, &mbox, &p, &mut out).unwrap(), 3);
        let s = String::from_utf8(out.clone()).unwrap();
        assert!(!s.contains("X-UID") && !s.contains("X-IMAP") && !s.contains("continued"));
        assert!(s.contains("X-Keywords: $Label1                     "));
        let again = split(&out).unwrap();
        assert!(again.covers(out.len()));
        let subjects: Vec<_> = again
            .messages
            .iter()
            .map(|m| {
                headers::first(&headers::fields(&out[m.header.clone()]), "subject")
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(subjects, ["first", "second", "third"]);
        // The source's last message had no separator; it now needs one.
        assert_eq!(&out[again.messages[1].separator.clone()], b"\n");
        assert!(s.contains("body2\nFrom a@x"));
    }

    #[test]
    fn ties_keep_file_order() {
        let src = b"From a@x  Mon Oct  6 09:00:00 2026\nSubject: a\n\nA\n\nFrom b@x  Mon Oct  6 09:00:00 2026\nSubject: b\n\nB\n";
        let mbox = split(src).unwrap();
        let p = plan(src, &mbox, SortKey::Received);
        assert_eq!(p.order, vec![0, 1]);
        assert_eq!(p.out_of_order, 0);
    }
}
