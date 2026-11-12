//! Byte-exact mbox splitting, following Dovecot's istream-raw-mbox rules.
//!
//! A file is `preamble ++ (from_line ++ header ++ body ++ separator)*`, and the
//! ranges reproduce the input exactly. The caller names the [`Variant`]; a file
//! doesn't say how its writer quoted `From ` lines, so there is no default.

use std::borrow::Cow;
use std::ops::Range;

use crate::util::days_from_civil;

/// How the file's writer quoted body lines that start with `From `.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Variant {
    /// As Dovecot reads it: `Content-Length` honoured, `>From` kept as written.
    Dovecot,
    /// mboxrd (e.g. Google Takeout): one `>` dropped from each `>…>From ` line.
    Mboxrd,
}

impl Variant {
    pub fn name(self) -> &'static str {
        match self {
            Variant::Dovecot => "dovecot",
            Variant::Mboxrd => "mboxrd",
        }
    }

    pub fn parse(s: &str) -> Option<Variant> {
        [Variant::Dovecot, Variant::Mboxrd]
            .into_iter()
            .find(|v| v.name() == s)
    }
}

/// A line starting `From ` that mboxrd would have quoted, so quoting the message back can't rebuild the file.
#[derive(Debug, PartialEq, Eq)]
pub struct Unquoted {
    pub offset: usize,
}

impl std::fmt::Display for Unquoted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unquoted 'From ' line at byte {}, so this is not mboxrd",
            self.offset
        )
    }
}

impl std::error::Error for Unquoted {}

/// Offsets within `b` where a line starts.
fn line_starts(b: &[u8]) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        b.iter()
            .enumerate()
            .filter(|&(i, &c)| c == b'\n' && i + 1 < b.len())
            .map(|(i, _)| i + 1),
    )
}

/// Length of the `>` run before `From ` at the start of `line`, if it is one.
fn quoted_from(line: &[u8]) -> Option<usize> {
    let n = line.iter().take_while(|&&c| c == b'>').count();
    line[n..].starts_with(b"From ").then_some(n)
}

/// `b` as `variant`'s writer quoted it.
pub fn quote(variant: Variant, b: &[u8]) -> Cow<'_, [u8]> {
    if variant == Variant::Dovecot {
        return Cow::Borrowed(b);
    }
    let at: Vec<usize> = line_starts(b)
        .filter(|&i| quoted_from(&b[i..]).is_some())
        .collect();
    if at.is_empty() {
        return Cow::Borrowed(b);
    }
    let mut out = Vec::with_capacity(b.len() + at.len());
    let mut pos = 0;
    for i in at {
        out.extend_from_slice(&b[pos..i]);
        out.push(b'>');
        pos = i;
    }
    out.extend_from_slice(&b[pos..]);
    Cow::Owned(out)
}

/// `b` (from file offset `base`) with `variant`'s quoting undone; `quote` gives `b` back.
pub fn unquote(variant: Variant, b: &[u8], base: usize) -> Result<Cow<'_, [u8]>, Unquoted> {
    if variant == Variant::Dovecot {
        return Ok(Cow::Borrowed(b));
    }
    let mut cut = Vec::new();
    for i in line_starts(b) {
        match quoted_from(&b[i..]) {
            Some(0) => return Err(Unquoted { offset: base + i }),
            Some(_) => cut.push(i),
            None => {}
        }
    }
    if cut.is_empty() {
        return Ok(Cow::Borrowed(b));
    }
    let mut out = Vec::with_capacity(b.len());
    let mut pos = 0;
    for i in cut {
        out.extend_from_slice(&b[pos..i]);
        pos = i + 1;
    }
    out.extend_from_slice(&b[pos..]);
    Ok(Cow::Owned(out))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromLine {
    pub sender: Vec<u8>,
    /// Seconds since the epoch; treated as UTC when the line carries no zone.
    pub time: i64,
    pub tz_offset_min: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// `From ` line including its line ending.
    pub from_line: Range<usize>,
    /// Header block including the empty line that ends it, if any.
    pub header: Range<usize>,
    pub body: Range<usize>,
    /// The newline before the next `From ` line (or at EOF) that belongs to mbox, not the message.
    pub separator: Range<usize>,
    pub from: FromLine,
    pub used_content_length: bool,
}

impl Message {
    pub fn raw(&self) -> Range<usize> {
        self.header.start..self.body.end
    }
}

pub type Bytes<'b> = Cow<'b, [u8]>;

#[derive(Debug)]
pub struct Mbox {
    pub variant: Variant,
    /// Bytes before the first `From ` line; Dovecot would refuse such a file.
    pub preamble: Range<usize>,
    pub messages: Vec<Message>,
}

impl Mbox {
    /// The message's header and body as its writer was given them.
    pub fn content<'b>(
        &self,
        buf: &'b [u8],
        m: &Message,
    ) -> Result<(Bytes<'b>, Bytes<'b>), Unquoted> {
        Ok((
            unquote(self.variant, &buf[m.header.clone()], m.header.start)?,
            unquote(self.variant, &buf[m.body.clone()], m.body.start)?,
        ))
    }

    /// True when the ranges tile `0..len` with no gaps or overlaps.
    pub fn covers(&self, len: usize) -> bool {
        let mut pos = self.preamble.end;
        for m in &self.messages {
            for r in [&m.from_line, &m.header, &m.body, &m.separator] {
                if r.start != pos || r.end < r.start {
                    return false;
                }
                pos = r.end;
            }
        }
        self.preamble.start == 0 && pos == len
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct NotMbox;

impl std::fmt::Display for NotMbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no valid 'From ' line found")
    }
}

impl std::error::Error for NotMbox {}

const MONTHS: [&[u8; 3]; 12] = [
    b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov", b"dec",
];

fn parse_month(s: &[u8], i: usize) -> Option<u32> {
    let m = s.get(i..i + 3)?;
    let month = match MONTHS.iter().position(|name| m.eq_ignore_ascii_case(*name)) {
        Some(n) => n as u32,
        None if m == b"???" => 0,
        None => return None,
    };
    (s.get(i + 3) == Some(&b' ')).then_some(month)
}

fn digits(s: &[u8], i: usize, n: usize) -> Option<i64> {
    let d = s.get(i..i + n)?;
    d.iter()
        .all(u8::is_ascii_digit)
        .then(|| d.iter().fold(0, |acc, b| acc * 10 + i64::from(b - b'0')))
}

fn numeric_tz(s: &[u8], i: usize) -> Option<i32> {
    let sign = match s.get(i)? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hh = digits(s, i + 1, 2)? as i32;
    let mm = digits(s, i + 3, 2)? as i32;
    Some(sign * (hh * 60 + mm))
}

/// Port of Dovecot's `mbox_from_parse`; `s` is the line after `From `, without its line ending.
/// Unlike Dovecot, `+HHMM` minutes are read as minutes rather than seconds.
pub fn parse_from_line(s: &[u8]) -> Option<FromLine> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let n = s.len();
    let mut i = 0;

    if at(0) == b'"' {
        let mut esc = false;
        i = 1;
        while i < n && (s[i] != b'"' || esc) {
            if matches!(s[i], b'\r' | b'\n') {
                return None;
            }
            esc = s[i] == b'\\';
            i += 1;
        }
        i += 1;
    }
    while i < n && s[i] != b' ' {
        if matches!(s[i], b'\r' | b'\n') {
            return None;
        }
        i += 1;
    }
    let sender_end = i.min(n);
    while i < n && s[i] == b' ' {
        i += 1;
    }
    if i + 21 > n {
        return None;
    }

    i += 4; // weekday
    let (month, mday, mut year, alt_stamp);
    if let Some(m) = parse_month(s, i) {
        alt_stamp = false;
        month = m;
        i += 4;
        if at(i) == b' ' {
            i += 1;
        }
        if at(i + 1) == b' ' {
            mday = digits(s, i, 1)?;
            i += 2;
        } else {
            mday = digits(s, i, 2)?;
            if at(i + 2) != b' ' {
                return None;
            }
            i += 3;
        }
        year = None;
    } else {
        // "Thu, 9 Nov 2002 22:33:52"
        alt_stamp = true;
        i += 1;
        let mut d = digits(s, i, 1)?;
        i += 1;
        if let Some(d2) = digits(s, i, 1) {
            d = d * 10 + d2;
            i += 1;
        }
        if at(i) != b' ' {
            return None;
        }
        mday = d;
        i += 1;
        month = parse_month(s, i)?;
        i += 4;
        year = Some(digits(s, i, 4)?);
        i += 5;
    }
    let mday = if mday == 0 { 1 } else { mday };

    if i + 5 > n || at(i + 2) != b':' {
        return None;
    }
    let hour = digits(s, i, 2)?;
    let min = digits(s, i + 3, 2)?;
    i += 5;

    let mut sec = 0;
    if i >= n {
    } else if at(i) == b':' {
        if i + 3 > n {
            return None;
        }
        sec = digits(s, i + 1, 2)?;
        i += 3;
        if !alt_stamp {
            if at(i) != b' ' {
                return None;
            }
            i += 1;
        }
    } else if !alt_stamp {
        if at(i) != b' ' {
            return None;
        }
        i += 1;
    }

    let mut tz = None;
    if alt_stamp {
    } else if i + 4 <= n && digits(s, i, 4).is_none() {
        if let Some(t) = numeric_tz(s, i).filter(|_| at(i + 5) == b' ') {
            tz = Some(t);
            i += 6;
        } else {
            while i < n && s[i] != b' ' {
                if matches!(s[i], b'\r' | b'\n') {
                    return None;
                }
                i += 1;
            }
            if i + 5 > n {
                return None;
            }
            i += 1;
        }
    }

    if !alt_stamp {
        if i + 4 > n {
            return None;
        }
        year = Some(digits(s, i, 4)?);
        i += 4;
    }
    if tz.is_none() && at(i) == b' ' {
        tz = numeric_tz(s, i + 1);
    }

    let days = days_from_civil(year?, month + 1, mday);
    let time = days * 86_400 + hour * 3600 + min * 60 + sec - i64::from(tz.unwrap_or(0)) * 60;
    Some(FromLine {
        sender: s[..sender_end].to_vec(),
        time,
        tz_offset_min: tz,
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn line_end(buf: &[u8], from: usize) -> usize {
    buf[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(buf.len(), |p| from + p + 1)
}

/// Parse a `From ` line starting at `pos`; returns the line's end and the parse.
fn from_line_at(buf: &[u8], pos: usize) -> Option<(usize, FromLine)> {
    let rest = buf.get(pos..)?.strip_prefix(b"From ")?;
    let end = line_end(buf, pos);
    let mut line = &rest[..end - pos - 5];
    line = line.strip_suffix(b"\n").unwrap_or(line);
    line = line.strip_suffix(b"\r").unwrap_or(line);
    parse_from_line(line).map(|f| (end, f))
}

/// The separator before the next valid `From ` line at or after `from`: (separator start, From start).
fn next_from(buf: &[u8], floor: usize, from: usize) -> Option<(usize, usize)> {
    let mut k = from;
    while let Some(nl) = find(buf, b"\nFrom ", k) {
        if from_line_at(buf, nl + 1).is_some() {
            let sep = if nl > floor && buf[nl - 1] == b'\r' {
                nl - 1
            } else {
                nl
            };
            return Some((sep, nl + 1));
        }
        k = nl + 1;
    }
    None
}

/// Header block (through its empty line) and body of one bare message.
pub fn split_message(raw: &[u8]) -> (Range<usize>, Range<usize>) {
    let h = header_end(raw, &(0..raw.len())).unwrap_or(raw.len());
    (0..h, h..raw.len())
}

/// End of the header block (exclusive), per Dovecot's empty-line test.
fn header_end(buf: &[u8], r: &Range<usize>) -> Option<usize> {
    (r.start..r.end)
        .find(|&i| {
            buf[i] == b'\n'
                && (i == r.start
                    || buf[i - 1] == b'\n'
                    || (buf[i - 1] == b'\r' && (i - 1 == r.start || buf[i - 2] == b'\n')))
        })
        .map(|i| i + 1)
}

fn content_length(header: &[u8]) -> Option<usize> {
    header.split(|&b| b == b'\n').find_map(|line| {
        let name = line.get(..15)?;
        if !name.eq_ignore_ascii_case(b"content-length:") {
            return None;
        }
        let v = line[15..].trim_ascii();
        if v.is_empty() || !v.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(v).ok()?.parse().ok()
    })
}

/// At EOF the trailing newline after a final empty line is the mbox separator.
fn eof_separator(buf: &[u8], r: &Range<usize>) -> usize {
    let m = &buf[r.clone()];
    if m == b"\n" || m == b"\r\n" {
        r.start
    } else if m.ends_with(b"\n\r\n") {
        r.end - 2
    } else if m.ends_with(b"\n\n") {
        r.end - 1
    } else {
        r.end
    }
}

/// Is `p` an exact message end: followed by a valid `From ` line, or only a newline before EOF?
fn valid_end(buf: &[u8], p: usize) -> Option<usize> {
    let tail = buf.get(p..)?;
    if tail == b"\n" || tail == b"\r\n" {
        return Some(buf.len());
    }
    let nl = if tail.starts_with(b"\r\n") { p + 1 } else { p };
    (buf.get(nl) == Some(&b'\n') && from_line_at(buf, nl + 1).is_some()).then_some(nl + 1)
}

pub fn split(buf: &[u8], variant: Variant) -> Result<Mbox, NotMbox> {
    if buf.is_empty() {
        return Ok(Mbox {
            variant,
            preamble: 0..0,
            messages: Vec::new(),
        });
    }
    let mut from_start = if from_line_at(buf, 0).is_some() {
        0
    } else {
        next_from(buf, 0, 0).ok_or(NotMbox)?.1
    };
    let mut mbox = Mbox {
        variant,
        preamble: 0..from_start,
        messages: Vec::new(),
    };

    loop {
        let (msg_start, from) = from_line_at(buf, from_start).expect("validated From line");
        let scanned = next_from(buf, msg_start, msg_start);
        let mut end = scanned.map_or(buf.len(), |(sep, _)| sep);
        let mut next = scanned.map(|(_, f)| f);

        let header_end_at = header_end(buf, &(msg_start..end)).unwrap_or(end);
        let mut used_content_length = false;
        // Content-Length only matters when the body hides a "From " line, so it may only extend.
        if variant == Variant::Dovecot
            && let Some(len) = content_length(&buf[msg_start..header_end_at])
        {
            let p = header_end_at.saturating_add(len);
            if p > end
                && let Some(after) = valid_end(buf, p)
            {
                end = p;
                next = (after < buf.len()).then_some(after);
                used_content_length = true;
            }
        }

        let (body_end, sep_end) = match next {
            Some(n) => (end, n),
            None if used_content_length => (end, buf.len()),
            None => (eof_separator(buf, &(msg_start..end)), buf.len()),
        };
        let header_end_at = header_end_at.min(body_end);
        mbox.messages.push(Message {
            from_line: from_start..msg_start,
            header: msg_start..header_end_at,
            body: header_end_at..body_end,
            separator: body_end..sep_end,
            from,
            used_content_length,
        });
        match next {
            Some(n) => from_start = n,
            None => return Ok(mbox),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebuild(buf: &[u8], m: &Mbox) -> Vec<u8> {
        let mut out = buf[m.preamble.clone()].to_vec();
        for msg in &m.messages {
            for r in [&msg.from_line, &msg.header, &msg.body, &msg.separator] {
                out.extend_from_slice(&buf[r.clone()]);
            }
        }
        out
    }

    fn check(buf: &[u8]) -> Mbox {
        let m = split(buf, Variant::Dovecot).unwrap();
        assert_eq!(rebuild(buf, &m), buf);
        assert!(m.covers(buf.len()));
        m
    }

    fn s<'a>(buf: &'a [u8], r: &Range<usize>) -> &'a [u8] {
        &buf[r.clone()]
    }

    #[test]
    fn from_line_formats() {
        let f = parse_from_line(b"MAILER-DAEMON Thu Nov  9 22:33:52 2001").unwrap();
        assert_eq!(f.sender, b"MAILER-DAEMON");
        assert_eq!(f.time, 1_005_345_232);
        assert_eq!(f.tz_offset_min, None);
        let f = parse_from_line(b"a@b Thu Nov  9 22:33:52 2001 +0130").unwrap();
        assert_eq!(f.time, 1_005_345_232 - 90 * 60);
        let f = parse_from_line(b"a@b Thu Nov 9 22:33:52 -0500 2001").unwrap();
        assert_eq!(f.tz_offset_min, Some(-300));
        assert!(parse_from_line(b"a@b Thu Nov  9 22:33 EST 2001").is_some());
        assert!(parse_from_line(b"a@b Thu, 9 Nov 2002 22:33:52").is_some());
        assert!(parse_from_line(b"\"x y\"@z Thu Nov 29 22:33:52 2001").is_some());
        assert!(parse_from_line(b"the depths of the sea, said she").is_none());
        assert!(parse_from_line(b"a@b").is_none());
    }

    #[test]
    fn splits_two_messages_exactly() {
        let buf = b"From a@x Thu Nov  9 22:33:52 2001\nSubject: one\n\nbody1\n\nFrom b@x Fri Nov 10 22:33:52 2001\nSubject: two\n\nbody2\n\n";
        let m = check(buf);
        assert_eq!(m.messages.len(), 2);
        assert_eq!(s(buf, &m.messages[0].header), b"Subject: one\n\n");
        assert_eq!(s(buf, &m.messages[0].body), b"body1\n");
        assert_eq!(s(buf, &m.messages[0].separator), b"\n");
        assert_eq!(s(buf, &m.messages[1].body), b"body2\n");
        assert_eq!(s(buf, &m.messages[1].separator), b"\n");
    }

    #[test]
    fn crlf_and_unescaped_bytes_kept() {
        let buf = b"From a@x Thu Nov  9 22:33:52 2001\r\nSubject: t\r\n\tfolded\r\n\r\n>From here\r\nFrom the depths\r\n\r\nFrom b@x Fri Nov 10 22:33:52 2001\r\n\r\n";
        let m = check(buf);
        assert_eq!(m.messages.len(), 2);
        assert_eq!(
            s(buf, &m.messages[0].header),
            b"Subject: t\r\n\tfolded\r\n\r\n"
        );
        assert_eq!(
            s(buf, &m.messages[0].body),
            b">From here\r\nFrom the depths\r\n"
        );
        assert_eq!(s(buf, &m.messages[0].separator), b"\r\n");
        assert!(m.messages[1].raw().is_empty());
        assert_eq!(s(buf, &m.messages[1].separator), b"\r\n");
    }

    #[test]
    fn content_length_spans_from_line_in_body() {
        let body = b"x\nFrom a@x Thu Nov  9 22:33:52 2001\ny\n";
        let mut buf = format!(
            "From a@x Thu Nov  9 22:33:52 2001\nContent-Length: {}\n\n",
            body.len()
        )
        .into_bytes();
        buf.extend_from_slice(body);
        buf.extend_from_slice(b"\nFrom b@x Fri Nov 10 22:33:52 2001\n\nz\n\n");
        let m = check(&buf);
        assert_eq!(m.messages.len(), 2);
        assert!(m.messages[0].used_content_length);
        assert_eq!(s(&buf, &m.messages[0].body), body);
    }

    #[test]
    fn bogus_content_length_falls_back_to_scan() {
        let buf = b"From a@x Thu Nov  9 22:33:52 2001\nContent-Length: 999\n\nhi\n\nFrom b@x Fri Nov 10 22:33:52 2001\n\nz\n";
        let m = check(buf);
        assert_eq!(m.messages.len(), 2);
        assert!(!m.messages[0].used_content_length);
        assert_eq!(s(buf, &m.messages[1].body), b"z\n");
        assert!(m.messages[1].separator.is_empty());
    }

    #[test]
    fn preamble_missing_eoh_and_garbage() {
        assert_eq!(
            split(b"not an mbox\n", Variant::Dovecot).unwrap_err(),
            NotMbox
        );
        let buf = b"junk\nFrom a@x Thu Nov  9 22:33:52 2001\nSubject: no body";
        let m = check(buf);
        assert_eq!(s(buf, &m.preamble), b"junk\n");
        assert_eq!(s(buf, &m.messages[0].header), b"Subject: no body");
        assert!(m.messages[0].body.is_empty());
    }

    #[test]
    fn mboxrd_unquotes_and_quotes_back() {
        let buf = b"From a@x Thu Nov  9 22:33:52 2001\r\nSubject: t\r\n\r\n>From here\r\n>>From there\r\n>not\r\n\r\n";
        let m = split(buf, Variant::Mboxrd).unwrap();
        let (h, b) = m.content(buf, &m.messages[0]).unwrap();
        assert_eq!(&*h, b"Subject: t\r\n\r\n");
        assert_eq!(&*b, b"From here\r\n>From there\r\n>not\r\n");
        assert_eq!(
            &*quote(Variant::Mboxrd, &b),
            &buf[m.messages[0].body.clone()]
        );
        assert!(matches!(
            m.content(buf, &m.messages[0]).unwrap().1,
            Cow::Owned(_)
        ));
        let d = split(buf, Variant::Dovecot).unwrap();
        assert_eq!(
            &*d.content(buf, &d.messages[0]).unwrap().1,
            &buf[d.messages[0].body.clone()]
        );
        let bad = b"From a@x Thu Nov  9 22:33:52 2001\n\nFrom the depths\n";
        let m = split(bad, Variant::Mboxrd).unwrap();
        assert_eq!(
            m.content(bad, &m.messages[0]).unwrap_err(),
            Unquoted { offset: 35 }
        );
    }

    #[test]
    fn mboxrd_ignores_content_length() {
        let body = b"x\nFrom a@x Thu Nov  9 22:33:52 2001\ny\n";
        let mut buf = format!(
            "From a@x Thu Nov  9 22:33:52 2001\nContent-Length: {}\n\n",
            body.len()
        )
        .into_bytes();
        buf.extend_from_slice(body);
        buf.extend_from_slice(b"\nFrom b@x Fri Nov 10 22:33:52 2001\n\nz\n");
        assert_eq!(split(&buf, Variant::Dovecot).unwrap().messages.len(), 2);
        let m = split(&buf, Variant::Mboxrd).unwrap();
        assert_eq!(m.messages.len(), 3);
        assert!(!m.messages[0].used_content_length);
    }
}
