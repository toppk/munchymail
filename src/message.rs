//! Decoding one stored message for reading: text body, attachments, and
//! sanitising mail-supplied text before it reaches a terminal or an agent.

use mailparse::{DispositionType, ParsedMail};

#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: Option<String>,
    pub content_type: String,
    /// Decoded size in bytes.
    pub size: usize,
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
    pub attachments: Vec<Attachment>,
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

struct Walk {
    plain: Option<String>,
    html: Option<String>,
    attachments: Vec<Attachment>,
    problem: Option<String>,
}

fn walk(p: &ParsedMail, w: &mut Walk) {
    if p.subparts.is_empty() {
        let mime = p.ctype.mimetype.to_ascii_lowercase();
        if is_attachment(p) {
            w.attachments.push(Attachment {
                filename: filename(p),
                content_type: mime,
                size: p.get_body_raw().map_or(0, |b| b.len()),
            });
            return;
        }
        let slot = match mime.as_str() {
            "text/plain" => &mut w.plain,
            "text/html" => &mut w.html,
            _ => return,
        };
        if slot.is_none() {
            match p.get_body() {
                Ok(t) => *slot = Some(t),
                Err(e) => w.problem = Some(format!("{mime} part: {e}")),
            }
        }
        return;
    }
    for s in &p.subparts {
        walk(s, w);
    }
}

/// Decode a whole raw message (header block plus body).
pub fn decode(raw: &[u8], body: &[u8]) -> Decoded {
    let parsed = match mailparse::parse_mail(raw) {
        Ok(p) => p,
        Err(e) => {
            return Decoded {
                text: String::from_utf8_lossy(body).into_owned(),
                text_source: TextSource::Raw,
                attachments: Vec::new(),
                problem: Some(format!("MIME parse failed: {e}")),
            };
        }
    };
    let mut w = Walk {
        plain: None,
        html: None,
        attachments: Vec::new(),
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
        attachments: w.attachments,
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
        assert_eq!(d.attachments.len(), 2);
        assert_eq!(d.attachments[0].filename.as_deref(), Some("a.pdf"));
        assert_eq!(d.attachments[0].size, 3);
        assert_eq!(decode(b"Subject: x\n\nplain\n", b"").attachments.len(), 0);
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
