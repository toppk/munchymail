//! Derived header index: ordered fields, unfolded and RFC 2047-decoded.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Lowercased, trimmed.
    pub name: String,
    pub value: String,
}

/// Split a raw header block into fields. Tolerant: a leading continuation
/// line becomes a nameless field, and a line without a colon becomes a name
/// with an empty value, rather than failing the whole block.
pub fn fields(block: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut pos = 0;
    while pos < block.len() {
        let end = block[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(block.len(), |p| pos + p + 1);
        let line = &block[pos..end];
        let blank = line == b"\n" || line == b"\r\n";
        let continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if pos > start && !continuation {
            out.push(decode(&block[start..pos]));
            start = pos;
        }
        if blank {
            start = end;
            break;
        }
        pos = end;
    }
    if start < block.len() {
        out.push(decode(&block[start..]));
    }
    out
}

fn decode(field: &[u8]) -> Field {
    match field.first() {
        Some(b' ' | b'\t') => Field {
            name: String::new(),
            value: unfold_lossy(field),
        },
        _ => match mailparse::parse_header(field) {
            Ok((h, _)) => Field {
                name: h.get_key_ref().trim().to_ascii_lowercase(),
                value: h.get_value().trim().to_string(),
            },
            Err(_) => Field {
                name: String::new(),
                value: unfold_lossy(field),
            },
        },
    }
}

fn unfold_lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn first<'a>(fields: &'a [Field], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.value.as_str())
}

/// MIME parts with a filename or `Content-Disposition: attachment`; 0 if it doesn't parse.
pub fn attachments(message: &[u8]) -> usize {
    fn count(p: &mailparse::ParsedMail) -> usize {
        let d = p.get_content_disposition();
        let named = d.params.contains_key("filename") || p.ctype.params.contains_key("name");
        usize::from(named || d.disposition == mailparse::DispositionType::Attachment)
            + p.subparts.iter().map(count).sum::<usize>()
    }
    mailparse::parse_mail(message).map_or(0, |m| count(&m))
}

/// Every address in a header value, lowercased; empty if it doesn't parse.
pub fn addresses(value: &str) -> Vec<String> {
    use mailparse::MailAddr;
    let Ok(list) = mailparse::addrparse(value) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for a in list.iter() {
        match a {
            MailAddr::Single(s) => out.push(s.addr.trim().to_lowercase()),
            MailAddr::Group(g) => out.extend(g.addrs.iter().map(|s| s.addr.trim().to_lowercase())),
        }
    }
    out
}

#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub date: Option<i64>,
    pub from: Option<String>,
    pub subject: Option<String>,
    pub message_id: Option<String>,
}

pub fn summary(fields: &[Field]) -> Summary {
    Summary {
        date: first(fields, "date").and_then(|d| mailparse::dateparse(d).ok()),
        from: first(fields, "from").map(str::to_string),
        subject: first(fields, "subject").map(str::to_string),
        message_id: first(fields, "message-id").map(|m| m.trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_attachments() {
        let m =
            b"Content-Type: multipart/mixed; boundary=b\n\n--b\nContent-Type: text/plain\n\nhi\n\
--b\nContent-Type: application/pdf; name=a.pdf\n\nx\n\
--b\nContent-Disposition: attachment\n\ny\n--b--\n";
        assert_eq!(attachments(m), 2);
        assert_eq!(attachments(b"Subject: x\n\nplain\n"), 0);
    }

    #[test]
    fn splits_folds_and_decodes() {
        let block = b"Received: from a\r\n\tby b\r\nSubject: =?utf-8?q?caf=C3=A9?=\r\nX-Bogus\r\nSubject: second\r\n\r\n";
        let f = fields(block);
        assert_eq!(f.len(), 4);
        assert_eq!(f[0].name, "received");
        assert_eq!(f[0].value, "from a by b");
        assert_eq!(f[1].value, "café");
        assert_eq!(f[2].name, "x-bogus");
        assert_eq!(f[2].value, "");
        assert_eq!(first(&f, "subject"), Some("café"));
    }

    #[test]
    fn tolerates_leading_continuation_and_no_eoh() {
        let f = fields(b"  orphan\nFrom: a@b\nTo: c@d");
        assert_eq!(f[0].name, "");
        assert_eq!(f[0].value, "orphan");
        assert_eq!(f[2].value, "c@d");
        assert!(fields(b"\n").is_empty());
    }
}
