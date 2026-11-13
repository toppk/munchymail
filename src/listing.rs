//! The columns `search` lists, who a message is between, and fitting a row to
//! the terminal.

use std::cell::Cell;

use anyhow::{Result, bail};

use crate::query::sieve_match;

/// `search`'s columns unless `--columns` or the config say otherwise.
pub const DEFAULT: &str = "id,date,who,subject";
/// `search --tsv`'s columns unless `--columns` says otherwise.
pub const TSV_DEFAULT: &str =
    "id,message,time,folder,from,to,subject,size,attachments,message-id,snippet";
/// Built-in column names; any other name is that header's decoded value.
pub const BUILT_IN: &[&str] = &[
    "id",
    "message",
    "date",
    "time",
    "who",
    "subject",
    "folder",
    "size",
    "attachments",
    "message-id",
    "snippet",
];

#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    /// The stored copy's id, as `show` takes.
    Id,
    /// The message id, mN.
    Message,
    Date,
    Time,
    /// The other side: `→` recipients of mail the account sent, `←` the sender of the rest.
    Who,
    Subject,
    Folder,
    /// Body bytes.
    Size,
    Attachments,
    MessageId,
    Snippet,
    /// The first such header's decoded value; address headers list people.
    Header(String),
}

impl Column {
    /// Comma-separated column names, e.g. `date,who,cc,subject`.
    pub fn parse_list(spec: &str) -> Result<Vec<Column>> {
        let cols: Vec<Column> = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| match s.to_ascii_lowercase().as_str() {
                "id" => Column::Id,
                "message" => Column::Message,
                "date" => Column::Date,
                "time" => Column::Time,
                "who" => Column::Who,
                "subject" => Column::Subject,
                "folder" => Column::Folder,
                "size" => Column::Size,
                "attachments" => Column::Attachments,
                "message-id" => Column::MessageId,
                "snippet" => Column::Snippet,
                h => Column::Header(h.to_string()),
            })
            .collect();
        if cols.is_empty() {
            bail!("no columns in {spec:?}; e.g. {DEFAULT}");
        }
        Ok(cols)
    }

    pub fn name(&self) -> &str {
        match self {
            Column::Id => "id",
            Column::Message => "message",
            Column::Date => "date",
            Column::Time => "time",
            Column::Who => "who",
            Column::Subject => "subject",
            Column::Folder => "folder",
            Column::Size => "size",
            Column::Attachments => "attachments",
            Column::MessageId => "message-id",
            Column::Snippet => "snippet",
            Column::Header(h) => h,
        }
    }

    /// Whether it needs the decoded message or its locations, not just the index.
    pub fn needs_decoding(&self) -> bool {
        matches!(
            self,
            Column::Folder | Column::Size | Column::Attachments | Column::Snippet
        )
    }

    fn right_aligned(&self) -> bool {
        matches!(self, Column::Id | Column::Size | Column::Attachments)
    }

    /// Widest it gets before the last column, and narrowest when space is short.
    fn widths(&self) -> (usize, usize) {
        match self {
            Column::Id | Column::Message | Column::Date | Column::Time => (20, 20),
            Column::Size | Column::Attachments => (10, 10),
            Column::Who => (28, 12),
            Column::Subject | Column::Snippet => (60, 20),
            _ => (28, 10),
        }
    }
}

/// Whether a header is a list of addresses, shown as people.
pub fn is_address_header(name: &str) -> bool {
    matches!(
        name,
        "from" | "to" | "cc" | "bcc" | "reply-to" | "sender" | "resent-from" | "resent-to"
    )
}

/// Each address in `value`: its display name, or the address when it has
/// none or `addresses`; the value itself when it doesn't parse.
pub fn people(value: &str, addresses: bool) -> Vec<String> {
    use mailparse::MailAddr;
    let Ok(list) = mailparse::addrparse(value) else {
        return if value.trim().is_empty() {
            Vec::new()
        } else {
            vec![value.trim().to_string()]
        };
    };
    let one = |s: &mailparse::SingleInfo| match &s.display_name {
        Some(n) if !addresses && !n.trim().is_empty() => n.trim().to_string(),
        _ => s.addr.trim().to_string(),
    };
    let mut out = Vec::new();
    for a in list.iter() {
        match a {
            MailAddr::Single(s) => out.push(one(s)),
            MailAddr::Group(g) => out.extend(g.addrs.iter().map(one)),
        }
    }
    out
}

/// The `who` column: `→ recipients` when any From address is one of `own`
/// (Sieve patterns), else `← sender`; `compact` shows the first and `+N`.
pub fn who(
    from: &str,
    recipients: &[&str],
    own: &[String],
    addresses: bool,
    compact: bool,
) -> String {
    let mine = |a: &str| own.iter().any(|p| sieve_match(p, a));
    let sent = crate::headers::addresses(from).iter().any(|a| mine(a));
    if !sent {
        return format!("← {}", people(from, addresses).join(", "));
    }
    let to: Vec<String> = recipients
        .iter()
        .flat_map(|r| crate::headers::addresses(r))
        .collect();
    let names: Vec<String> = recipients
        .iter()
        .flat_map(|r| people(r, addresses))
        .collect();
    match names.len() {
        0 => "→ (no recipients)".into(),
        _ if !to.is_empty() && to.iter().all(|a| mine(a)) => "→ (self)".into(),
        // The first name is cut so the count survives a narrow column.
        n if compact && n > 1 => format!("→ {} +{}", cut(&names[0], 20), n - 1),
        _ => format!("→ {}", names.join(", ")),
    }
}

thread_local! {
    static WIDTH: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The width rows are fitted to on this thread: the terminal of whoever
/// asked, or none when output isn't a terminal (so also whether it is one).
pub fn width() -> Option<usize> {
    WIDTH.get()
}

pub fn set_width(w: Option<usize>) {
    WIDTH.set(w);
}

/// Columns of the terminal on `fd` (80 if it doesn't say); none if it isn't one.
pub fn terminal_width(fd: std::os::fd::RawFd) -> Option<usize> {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one winsize, which ws is.
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut ws) };
    Some(if r == 0 && ws.ws_col > 0 {
        usize::from(ws.ws_col)
    } else {
        80
    })
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    // Keep who's "+N" count; cut the name before it.
    if let Some((head, count)) = s.rsplit_once(" +")
        && !count.is_empty()
        && count.bytes().all(|b| b.is_ascii_digit())
        && n > count.len() + 4
    {
        return format!("{} +{count}", cut(head, n - count.len() - 2));
    }
    if n == 0 {
        String::new()
    } else {
        s.chars().take(n - 1).chain(['…']).collect()
    }
}

/// `rows` as aligned lines two spaces apart, the last column taking what
/// `width` leaves (all of itself without one); others shrink to fit.
pub fn layout(cols: &[Column], rows: &[Vec<String>], width: Option<usize>) -> Vec<String> {
    let n = cols.len();
    let len = |s: &String| s.chars().count();
    let mut w: Vec<usize> = (0..n)
        .map(|i| {
            let longest = rows.iter().map(|r| len(&r[i])).max().unwrap_or(0);
            longest.min(cols[i].widths().0)
        })
        .collect();
    if let Some(width) = width {
        let last_min = cols[n - 1].widths().1.min(w[n - 1]);
        loop {
            let used: usize = w[..n - 1].iter().sum::<usize>() + 2 * (n - 1);
            if used + last_min <= width {
                break;
            }
            let shrink = (0..n - 1)
                .filter(|&i| w[i] > cols[i].widths().1)
                .max_by_key(|&i| w[i] - cols[i].widths().1);
            let Some(i) = shrink else { break };
            w[i] -= 1;
        }
    }
    let used: usize = w[..n.saturating_sub(1)].iter().sum::<usize>() + 2 * (n - 1);
    rows.iter()
        .map(|r| {
            let mut line = String::new();
            for i in 0..n {
                let last = i == n - 1;
                let cell = match (last, width) {
                    (false, _) => cut(&r[i], w[i]),
                    (true, Some(width)) => cut(&r[i], width.saturating_sub(used).max(1)),
                    (true, None) => r[i].clone(),
                };
                if last {
                    line.push_str(&cell);
                } else if cols[i].right_aligned() {
                    line.push_str(&format!("{cell:>0$}  ", w[i]));
                } else {
                    line.push_str(&format!("{cell:<0$}  ", w[i]));
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_is_the_other_side() {
        let own = vec!["me@x.org".to_string()];
        let to = ["\"Ann B\" <ann@y.org>, bob@z.org", "Cy <cy@w.org>"];
        assert_eq!(who("Me <me@x.org>", &to, &own, false, true), "→ Ann B +2");
        assert_eq!(
            who("Me <me@x.org>", &to, &own, false, false),
            "→ Ann B, bob@z.org, Cy"
        );
        assert_eq!(
            who("Ann B <ann@y.org>", &to, &own, true, true),
            "← ann@y.org"
        );
        assert_eq!(
            who("me@x.org", &["Me <ME@x.org>"], &own, false, true),
            "→ (self)"
        );
        assert_eq!(
            who("me@x.org", &[""], &own, false, true),
            "→ (no recipients)"
        );
    }

    #[test]
    fn layout_fits_the_width() {
        let cols = Column::parse_list("id,date,who,subject").unwrap();
        let rows = vec![vec![
            "19513".to_string(),
            "2026-11-12".to_string(),
            "→ Jamie P. Example Esquire of Counsel".to_string(),
            "Project Example: where things stand with the vendor and the contract".to_string(),
        ]];
        let lines = layout(&cols, &rows, Some(80));
        assert_eq!(lines[0].chars().count(), 80);
        assert!(lines[0].starts_with("19513  2026-11-12  → Jamie P. Example Esquire …  Project"));
        let narrow = layout(&cols, &rows, Some(60));
        assert_eq!(narrow[0].chars().count(), 60);
        assert_eq!(cut("→ Ann Long-Surname Ex… +2", 16), "→ Ann Long-S… +2");
        assert_eq!(
            layout(&cols, &rows, None)[0].chars().count(),
            19 + 28 + 2 + rows[0][3].len()
        );
        assert_eq!(
            Column::parse_list("Date, X-Mailer").unwrap()[1],
            Column::Header("x-mailer".into())
        );
    }
}
