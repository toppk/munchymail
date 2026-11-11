//! Decoding one stored message for reading: text body, MIME parts, and
//! sanitising mail-supplied text before it reaches a terminal or an agent.

use std::io::Cursor;

use mailparse::{DispositionType, MailHeaderMap, ParsedMail};
use sha2::{Digest, Sha256};

/// Most file names listed from one zip.
const MAX_MEMBERS: usize = 1000;

/// Most text kept from one part, in bytes.
pub const MAX_TEXT: usize = 1 << 20;

/// Attached messages decoded within attached messages, at most.
const MAX_DEPTH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// text/plain or text/html, not marked as a file: the message text.
    Body,
    /// Any other part not marked as a file, e.g. an image the HTML shows.
    Inline,
    /// Has a file name or `Content-Disposition: attachment`.
    Attachment,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Body => "body",
            Kind::Inline => "inline",
            Kind::Attachment => "attachment",
        }
    }
}

/// One MIME leaf part.
#[derive(Debug, Clone)]
pub struct Part {
    pub kind: Kind,
    pub filename: Option<String>,
    /// Content-Type as the sender declared it, lowercased.
    pub content_type: String,
    /// Type recognised from the decoded bytes (magic numbers), if any.
    pub detected: Option<&'static str>,
    /// Decoded size in bytes.
    pub size: usize,
    pub sha256: [u8; 32],
    /// File names in a zip, with uncompressed sizes; at most `MAX_MEMBERS`.
    pub members: Vec<(String, u64)>,
    /// Decoded text of a text/* part (HTML converted) or an attached message;
    /// at most `MAX_TEXT` bytes.
    pub text: Option<String>,
}

impl Part {
    /// Lowercased file name extension, without the dot.
    pub fn ext(&self) -> Option<String> {
        let name = self.filename.as_deref()?;
        let (stem, ext) = name.rsplit_once('.')?;
        (!stem.is_empty() && !ext.is_empty() && ext.len() <= 16).then(|| ext.to_ascii_lowercase())
    }

    /// The detected type, else the declared one.
    pub fn file_type(&self) -> &str {
        self.detected.unwrap_or(&self.content_type)
    }
}

fn zip_members(bytes: &[u8]) -> Vec<(String, u64)> {
    let Ok(mut z) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Vec::new();
    };
    (0..z.len().min(MAX_MEMBERS))
        .filter_map(|i| {
            let f = z.by_index_raw(i).ok()?;
            let name = String::from_utf8_lossy(f.name_raw()).into_owned();
            Some((name, f.size()))
        })
        .collect()
}

/// `infer`'s type, except where its test is too loose: it calls any
/// `-----BEGIN` text and any data starting 30 82 (DER) a certificate.
fn detect(bytes: &[u8]) -> Option<&'static str> {
    let t = infer::get(bytes)?.mime_type();
    if t != "application/x-x509-ca-cert" {
        return Some(t);
    }
    let label = bytes.strip_prefix(b"-----BEGIN ")?;
    Some(if label.starts_with(b"PGP SIGNATURE") {
        "application/pgp-signature"
    } else if label.starts_with(b"PGP PUBLIC KEY") {
        "application/pgp-keys"
    } else if label.starts_with(b"PGP MESSAGE") {
        "application/pgp-encrypted"
    } else if label.starts_with(b"CERTIFICATE") {
        t
    } else {
        "application/x-pem-file"
    })
}

/// `s` with CRLF as LF, cut to at most `MAX_TEXT` bytes.
pub fn cap(s: String) -> String {
    let mut s = if s.contains('\r') {
        s.replace("\r\n", "\n")
    } else {
        s
    };
    if s.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// An attached message's Subject and every text part in it.
fn message_text(bytes: &[u8], depth: usize) -> String {
    let d = decode_at(bytes, bytes, depth + 1);
    let subject = mailparse::parse_headers(bytes)
        .ok()
        .and_then(|(h, _)| h.get_first_value("Subject"));
    let texts = d.parts.into_iter().filter_map(|p| p.text);
    subject
        .into_iter()
        .chain(texts)
        .collect::<Vec<_>>()
        .join("\n")
}

fn part(p: &ParsedMail, kind: Kind, bytes: &[u8], text: Option<String>) -> Part {
    let detected = detect(bytes);
    Part {
        kind,
        filename: filename(p),
        content_type: p.ctype.mimetype.to_ascii_lowercase(),
        detected,
        size: bytes.len(),
        sha256: Sha256::digest(bytes).into(),
        members: if detected == Some("application/zip") {
            zip_members(bytes)
        } else {
            Vec::new()
        },
        text: text.map(cap),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSource {
    Plain,
    /// No text/plain part; converted from text/html.
    Html,
    /// The MIME structure didn't parse; the raw body bytes, lossily decoded.
    Raw,
    None,
}

impl TextSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TextSource::Plain => "plain",
            TextSource::Html => "html",
            TextSource::Raw => "raw",
            TextSource::None => "none",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: String,
    pub text_source: TextSource,
    /// Every leaf part, in MIME order, the text body's included.
    pub parts: Vec<Part>,
    /// Why the message (or a part of it) couldn't be decoded, if it couldn't.
    pub problem: Option<String>,
}

fn is_attachment(p: &ParsedMail) -> bool {
    let d = p.get_content_disposition();
    d.disposition == DispositionType::Attachment
        || d.params.contains_key("filename")
        || p.ctype.params.contains_key("name")
}

fn filename(p: &ParsedMail) -> Option<String> {
    let d = p.get_content_disposition();
    d.params
        .get("filename")
        .or_else(|| p.ctype.params.get("name"))
        .cloned()
}

impl Decoded {
    /// Parts other than the message text: files and inline parts.
    pub fn attachments(&self) -> impl Iterator<Item = &Part> {
        self.parts.iter().filter(|p| p.kind != Kind::Body)
    }

    /// Parts marked as files.
    pub fn files(&self) -> usize {
        self.parts
            .iter()
            .filter(|p| p.kind == Kind::Attachment)
            .count()
    }
}

struct Walk {
    depth: usize,
    plain: Option<String>,
    html: Option<String>,
    parts: Vec<Part>,
    problem: Option<String>,
}

fn walk(p: &ParsedMail, w: &mut Walk) {
    if p.subparts.is_empty() {
        let mime = p.ctype.mimetype.to_ascii_lowercase();
        let bytes = p.get_body_raw().unwrap_or_default();
        let kind = match mime.as_str() {
            _ if is_attachment(p) => Kind::Attachment,
            "text/plain" | "text/html" => Kind::Body,
            _ => Kind::Inline,
        };
        let body = match mime.as_str() {
            m if m.starts_with("text/") => match p.get_body() {
                Ok(t) => Some(t),
                Err(e) => {
                    w.problem = Some(format!("{mime} part: {e}"));
                    None
                }
            },
            _ => None,
        };
        let text = match mime.as_str() {
            "text/html" => body.as_deref().map(html_to_text),
            "message/rfc822" | "message/global" if w.depth < MAX_DEPTH => {
                Some(message_text(&bytes, w.depth))
            }
            _ => body.clone(),
        };
        w.parts.push(part(p, kind, &bytes, text));
        let slot = match (kind, mime.as_str()) {
            (Kind::Body, "text/plain") => &mut w.plain,
            (Kind::Body, "text/html") => &mut w.html,
            _ => return,
        };
        if slot.is_none() {
            *slot = body;
        }
        return;
    }
    for s in &p.subparts {
        walk(s, w);
    }
}

fn nth_leaf(p: &ParsedMail, n: &mut usize) -> Option<Vec<u8>> {
    if p.subparts.is_empty() {
        if *n == 0 {
            return Some(p.get_body_raw().unwrap_or_default());
        }
        *n -= 1;
        return None;
    }
    p.subparts.iter().find_map(|s| nth_leaf(s, n))
}

/// The decoded bytes of leaf part `n` (`Decoded::parts[n]`).
pub fn leaf(raw: &[u8], mut n: usize) -> Option<Vec<u8>> {
    nth_leaf(&mailparse::parse_mail(raw).ok()?, &mut n)
}

/// Decode a whole raw message (header block plus body).
pub fn decode(raw: &[u8], body: &[u8]) -> Decoded {
    decode_at(raw, body, 0)
}

fn decode_at(raw: &[u8], body: &[u8], depth: usize) -> Decoded {
    let parsed = match mailparse::parse_mail(raw) {
        Ok(p) => p,
        Err(e) => {
            return Decoded {
                text: String::from_utf8_lossy(body).into_owned(),
                text_source: TextSource::Raw,
                parts: Vec::new(),
                problem: Some(format!("MIME parse failed: {e}")),
            };
        }
    };
    let mut w = Walk {
        depth,
        plain: None,
        html: None,
        parts: Vec::new(),
        problem: None,
    };
    walk(&parsed, &mut w);
    let (text, text_source) = match (w.plain, w.html) {
        (Some(t), _) => (t, TextSource::Plain),
        (None, Some(h)) => (html_to_text(&h), TextSource::Html),
        (None, None) => (String::new(), TextSource::None),
    };
    Decoded {
        text: text.replace("\r\n", "\n"),
        text_source,
        parts: w.parts,
        problem: w.problem,
    }
}

/// The first `n` characters of `text` with whitespace collapsed, skipping quoted lines.
pub fn snippet(text: &str, n: usize) -> String {
    let mut out = String::new();
    for word in text
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .flat_map(str::split_whitespace)
    {
        if out.chars().count() >= n {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.chars().count() > n {
        out = out.chars().take(n - 1).chain(['…']).collect();
    }
    out
}

/// Replace control characters (except newline and tab) and bidi overrides,
/// so mail text can't drive a terminal or hide what it says.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => '\u{FFFD}',
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// Rough HTML to text: drops script/style, breaks lines at block tags, decodes entities.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        if html[i..].starts_with('<') {
            let end = html[i..].find('>').map_or(html.len(), |e| i + e + 1);
            let tag = lower[i + 1..end.saturating_sub(1).max(i + 1)]
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("")
                .to_string();
            i = end;
            if tag == "script" || tag == "style" {
                if let Some(close) = lower[i..].find(&format!("</{tag}")) {
                    i += close;
                }
                continue;
            }
            if matches!(
                tag.as_str(),
                "br" | "p"
                    | "div"
                    | "tr"
                    | "li"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "table"
                    | "blockquote"
            ) {
                out.push('\n');
            }
        } else if html[i..].starts_with('&') {
            let semi = html[i..].find(';').filter(|&e| e <= 10);
            match semi.and_then(|e| entity(&html[i + 1..i + e]).map(|c| (c, e))) {
                Some((c, e)) => {
                    out.push(c);
                    i += e + 1;
                }
                None => {
                    out.push('&');
                    i += 1;
                }
            }
        } else {
            let c = html[i..].chars().next().unwrap();
            out.push(if c == '\n' || c == '\r' { ' ' } else { c });
            i += c.len_utf8();
        }
    }
    let lines: Vec<String> = out
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let mut text = String::new();
    let mut blank = 0;
    for l in lines {
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        text.push_str(&l);
        text.push('\n');
    }
    text.trim().to_string()
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let n = name.strip_prefix('#')?;
            let v = match n.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => n.parse().ok()?,
            };
            return char::from_u32(v);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_parts() {
        let m = b"Content-Type: multipart/mixed; boundary=b\n\n--b\nContent-Type: text/plain; charset=utf-8\nContent-Transfer-Encoding: quoted-printable\n\ncaf=C3=A9\n\
--b\nContent-Type: application/pdf; name=a.pdf\nContent-Transfer-Encoding: base64\n\neHl6\n\
--b\nContent-Disposition: attachment\n\ny\n--b--\n";
        let d = decode(m, b"");
        assert_eq!(d.text.trim(), "café");
        assert_eq!(d.text_source, TextSource::Plain);
        let a: Vec<&Part> = d.attachments().collect();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].filename.as_deref(), Some("a.pdf"));
        assert_eq!(a[0].ext().as_deref(), Some("pdf"));
        assert_eq!(a[0].size, 3);
        assert_eq!(d.parts[0].kind, Kind::Body);
        assert_eq!(
            decode(b"Subject: x\n\nplain\n", b"").attachments().count(),
            0
        );
    }

    #[test]
    fn detects_content_and_lists_zips() {
        use std::io::Write;
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        z.start_file("dir/scan.pdf", opts).unwrap();
        z.write_all(b"%PDF-1.4").unwrap();
        let zip = z.finish().unwrap().into_inner();
        let b64 = |b: &[u8]| data_encoding::BASE64.encode(b);
        let m = format!(
            "Content-Type: multipart/mixed; boundary=b\n\n--b\nContent-Type: text/plain\n\nhi\n\
--b\nContent-Type: application/octet-stream; name=x.bin\nContent-Transfer-Encoding: base64\n\n{}\n\
--b\nContent-Type: application/zip; name=a.zip\nContent-Transfer-Encoding: base64\n\n{}\n\
--b\nContent-Type: image/png\nContent-ID: <logo>\n\nnot really a png\n--b--\n",
            b64(b"%PDF-1.4 hello"),
            b64(&zip)
        );
        let d = decode(m.as_bytes(), b"");
        let a: Vec<&Part> = d.attachments().collect();
        assert_eq!(a[0].content_type, "application/octet-stream");
        assert_eq!(a[0].detected, Some("application/pdf"));
        assert_eq!(a[0].file_type(), "application/pdf");
        assert_eq!(a[1].members, [("dir/scan.pdf".to_string(), 8)]);
        assert_eq!((a[2].kind, a[2].detected), (Kind::Inline, None));
        assert_eq!(d.files(), 2);
        assert_eq!(
            detect(b"-----BEGIN PGP SIGNATURE-----\n"),
            Some("application/pgp-signature")
        );
        assert_eq!(detect(b"\x30\x82\x01\x02 pkcs7"), None);
    }

    #[test]
    fn html_fallback_and_safety() {
        let m =
            b"Content-Type: text/html\n\n<style>p{}</style><p>Hi &amp; <b>bye</b></p><br>&#x263A;";
        let d = decode(m, b"");
        assert_eq!(d.text_source, TextSource::Html);
        assert_eq!(d.text, "Hi & bye\n\n\u{263A}");
        assert_eq!(
            sanitize("a\x1b[31mb\u{202E}c\n"),
            "a\u{FFFD}[31mb\u{FFFD}c\n"
        );
        assert_eq!(snippet("> quoted\nhello   there\nworld", 8), "hello t…");
        assert_eq!(snippet("hello   there", 11), "hello there");
    }
}
