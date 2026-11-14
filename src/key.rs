//! What makes stored copies one message: the message key (`docs/identity.md`).

use sha2::{Digest, Sha256};

use crate::headers::{raw_fields, raw_name};

pub type Hash = [u8; 32];

/// The rule keys follow, recorded as `meta` `identity`; a new rule re-keys.
pub const RULE: &str = "lf-nul-eol+msgid+mime+hdr";

/// Header fields that tell deliveries apart when a copy has no valid Message-ID.
const EVIDENCE: [&str; 7] = [
    "from",
    "to",
    "cc",
    "subject",
    "date",
    "message-id",
    "received",
];

fn sha256(b: &[u8]) -> Hash {
    Sha256::digest(b).into()
}

/// A body's identity, the same rule for every source: its hash with each CRLF
/// read as LF, each NUL as 0x80, and a final newline added if it lacks one. A
/// store keeps the same email as LF (an mbox file) or CRLF (IMAP, Gmail);
/// ordinary IMAP literals can't carry NUL, which Dovecot serves as 0x80; and an
/// mbox must end every message with a newline, so one that had none (common in
/// mail composed in Gmail) gains one in the file. A body with LF endings, no NUL
/// and a final newline hashes as it is.
pub fn identity(body: &[u8]) -> Hash {
    let crlf = body.windows(2).any(|w| w == b"\r\n");
    if !crlf && !body.contains(&0) && body.ends_with(b"\n") {
        return sha256(body);
    }
    let mut canonical = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\r' && body.get(i + 1) == Some(&b'\n') {
            i += 1;
        }
        canonical.push(if body[i] == 0 { 0x80 } else { body[i] });
        i += 1;
    }
    if !canonical.ends_with(b"\n") {
        canonical.push(b'\n');
    }
    sha256(&canonical)
}

fn lf(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    for (i, &c) in b.iter().enumerate() {
        if !(c == b'\r' && b.get(i + 1) == Some(&b'\n')) {
            out.push(c);
        }
    }
    out
}

/// A field's value: after its colon, unfolded.
fn value(field: &[u8]) -> Vec<u8> {
    let v = field
        .iter()
        .position(|&b| b == b':')
        .map_or(&[][..], |i| &field[i + 1..]);
    v.iter()
        .copied()
        .filter(|&b| b != b'\r' && b != b'\n')
        .collect()
}

fn first<'a>(fields: &[&'a [u8]], name: &str) -> Option<&'a [u8]> {
    fields.iter().find(|f| raw_name(f) == name).copied()
}

/// `v` without RFC 5322 comments outside quoted strings.
fn uncomment(v: &[u8]) -> Vec<u8> {
    let (mut out, mut depth, mut quoted, mut escaped) = (Vec::new(), 0usize, false, false);
    for &b in v {
        if escaped {
            escaped = false;
        } else if b == b'\\' && (quoted || depth > 0) {
            escaped = true;
        } else if b == b'"' && depth == 0 {
            quoted = !quoted;
        } else if b == b'(' && !quoted {
            depth += 1;
            continue;
        } else if b == b')' && !quoted && depth > 0 {
            depth -= 1;
            continue;
        }
        if depth == 0 {
            out.push(b);
        }
    }
    out
}

/// The first `Message-ID` field parsed as one RFC 5322 `msg-id`: its
/// `id-left@id-right`, case kept, never RFC 2047-decoded. A bare `left@right`
/// counts; none, several or a value without exactly one `@` is no Message-ID.
pub fn message_id(header: &[u8]) -> Option<String> {
    let v = uncomment(&value(first(&raw_fields(header), "message-id")?));
    let v = v.trim_ascii();
    let id = match v.strip_prefix(b"<") {
        Some(rest) => rest.strip_suffix(b">")?,
        None => v,
    };
    let at = id.iter().position(|&b| b == b'@')?;
    let ok = at > 0
        && at + 1 < id.len()
        && id.iter().filter(|&&b| b == b'@').count() == 1
        && !id
            .iter()
            .any(|&b| b <= b' ' || b == 0x7f || matches!(b, b'<' | b'>' | b'(' | b')'));
    ok.then(|| String::from_utf8(id.to_vec()).ok()).flatten()
}

/// How the body's bytes are to be read: transfer encoding, media type, charset
/// and boundary. Missing `Content-Type` is `text/plain; charset=us-ascii`;
/// missing, `7bit`, `8bit` and `binary` encodings all mean "not encoded".
pub fn mime(header: &[u8]) -> String {
    let fields = raw_fields(header);
    let text = |name| first(&fields, name).map(|f| String::from_utf8_lossy(&value(f)).into_owned());
    let cte = text("content-transfer-encoding")
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !matches!(v.as_str(), "" | "7bit" | "8bit" | "binary"))
        .unwrap_or_default();
    let ct = mailparse::parse_content_type(&text("content-type").unwrap_or_default());
    let boundary = ct.params.get("boundary").map_or("", String::as_str);
    let mimetype = match ct.mimetype.trim() {
        "" => "text/plain".to_string(),
        t => t.to_ascii_lowercase(),
    };
    format!(
        "{}\0{}\0{}\0{}",
        mimetype,
        ct.charset.to_ascii_lowercase(),
        boundary,
        cte
    )
}

/// What a copy without a valid Message-ID must share with another to be the
/// same message: its `EVIDENCE` fields as written, or, with none of them, the
/// whole header. Line endings read as LF.
pub fn header_key(header: &[u8]) -> Hash {
    let picked: Vec<u8> = raw_fields(header)
        .into_iter()
        .filter(|f| EVIDENCE.contains(&raw_name(f).as_str()))
        .flatten()
        .copied()
        .collect();
    sha256(&lf(if picked.is_empty() { header } else { &picked }))
}

/// One copy's message key and the parts kept on the message row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub hash: Hash,
    pub identity: Hash,
    pub message_id: Option<String>,
}

/// Copies with equal keys are one message: equal body identity, Message-ID,
/// Date and MIME reading, and, without a valid Message-ID, equal header evidence.
pub fn key(header: &[u8], body: &[u8], date: Option<i64>) -> Key {
    key_with(header, identity(body), date)
}

/// `key` for a body whose `identity` is already known.
pub fn key_with(header: &[u8], identity: Hash, date: Option<i64>) -> Key {
    let message_id = message_id(header);
    let mut h = Sha256::new();
    h.update(RULE.as_bytes());
    h.update(identity);
    match &message_id {
        Some(id) => h.update([b"\0M".as_slice(), id.as_bytes()].concat()),
        None => h.update([b"\0H".as_slice(), &header_key(header)].concat()),
    }
    h.update(format!("\0D{date:?}\0").as_bytes());
    h.update(mime(header).as_bytes());
    Key {
        hash: h.finalize().into(),
        identity,
        message_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_reads_crlf_as_lf() {
        assert_eq!(identity(b"a\r\nb\r\n"), sha256(b"a\nb\n"));
        assert_eq!(identity(b"a\nb\n"), sha256(b"a\nb\n"));
        // A lone CR is content, not a line ending.
        assert_eq!(identity(b"a\rb\n"), sha256(b"a\rb\n"));
        // NUL reads as 0x80, as Dovecot serves it over IMAP.
        assert_eq!(identity(b"a\x00b\n"), identity(b"a\x80b\r\n"));
        assert_eq!(identity(b"a\x00b\n"), sha256(b"a\x80b\n"));
        // A missing final newline reads as present, as an mbox file stores it.
        assert_eq!(identity(b"a\r\nb"), identity(b"a\r\nb\r\n"));
        assert_eq!(identity(b"a\nb"), sha256(b"a\nb\n"));
        assert_ne!(identity(b"a\n"), identity(b"a\n\n"));
        assert_ne!(identity(b"a\r\n"), identity(b"a\r\r\n"));
    }

    #[test]
    fn message_id_is_parsed_not_decoded() {
        let id = |h: &str| message_id(h.as_bytes());
        assert_eq!(id("Message-ID: <a@b>\n\n"), Some("a@b".into()));
        assert_eq!(
            id("message-id:\r\n <A@B> (comment)\r\n\r\n"),
            Some("A@B".into())
        );
        assert_eq!(id("Message-ID: a@b\n\n"), Some("a@b".into()));
        assert_eq!(
            id("Message-ID: <=?utf-8?q?a?=@b>\n\n"),
            Some("=?utf-8?q?a?=@b".into())
        );
        for bad in [
            "Message-ID: unknown\n\n",
            "Message-ID: <a@b> <c@d>\n\n",
            "Message-ID: <a b@c>\n\n",
            "Message-ID: <@b>\n\n",
            "Message-ID: <a@b@c>\n\n",
            "Message-ID: <a@b\n\n",
            "Subject: x\n\n",
        ] {
            assert_eq!(id(bad), None, "{bad:?}");
        }
        assert_eq!(
            id("Message-ID: <a@b>\nMessage-ID: <c@d>\n\n"),
            Some("a@b".into())
        );
    }

    #[test]
    fn mime_keeps_boundary_case_and_folds_tokens() {
        let m = |h: &str| mime(h.as_bytes());
        assert_eq!(m("\n"), m("Content-Type: TEXT/Plain; charset=US-ASCII\n\n"));
        assert_eq!(m("\n"), m("Content-Transfer-Encoding: 8BIT\n\n"));
        assert_ne!(m("\n"), m("Content-Transfer-Encoding: base64\n\n"));
        assert_ne!(m("\n"), m("Content-Type: text/plain; charset=utf-8\n\n"));
        assert_ne!(
            m("Content-Type: multipart/mixed; boundary=AbC\n\n"),
            m("Content-Type: multipart/mixed; boundary=abc\n\n")
        );
        assert_eq!(
            m("Content-Type: multipart/mixed;\n boundary=\"AbC\"\n\n"),
            m("Content-Type: Multipart/Mixed; boundary=AbC\r\n\r\n")
        );
    }

    #[test]
    fn header_evidence_only_without_a_message_id() {
        let k = |h: &str, b: &str| key(h.as_bytes(), b.as_bytes(), None).hash;
        // A valid Message-ID: other header differences don't matter.
        assert_eq!(
            k("Message-ID: <a@b>\nReceived: one\n\n", "x\n"),
            k(
                "Message-ID: <a@b>\r\nReceived: two\r\nStatus: RO\r\n\r\n",
                "x\r\n"
            )
        );
        // None: the deliveries differ, so the copies do.
        assert_ne!(k("Received: one\n\n", "x\n"), k("Received: two\n\n", "x\n"));
        // The same delivery from an mbox file and over IMAP is one.
        assert_eq!(
            k("Received: one\nStatus: RO\nX-UID: 4\n\n", "x\n"),
            k("Received: one\r\n\r\n", "x\r\n")
        );
        // An invalid Message-ID is still evidence.
        assert_ne!(
            k("Message-ID: one\n\n", "\n"),
            k("Message-ID: two\n\n", "\n")
        );
        // With no evidence fields, the whole header must match.
        assert_ne!(k("X-Note: one\n\n", ""), k("X-Note: two\n\n", ""));
        assert_eq!(k("X-Note: one\n\n", ""), k("X-Note: one\r\n\r\n", ""));
        // Same bytes, read differently.
        assert_ne!(
            k("Message-ID: <a@b>\n\n", "aGVsbG8=\n"),
            k(
                "Message-ID: <a@b>\nContent-Transfer-Encoding: base64\n\n",
                "aGVsbG8=\n"
            )
        );
    }
}
