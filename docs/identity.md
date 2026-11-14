# Identity, deduplication and damaged mail

How munchymail decides that two stored copies are the same email, what it
does with copies that differ, and how damaged or out-of-spec mail behaves.
Written for review; the rules here are what the code does as of 2026-11-14
(`src/import.rs` `identity`, `Artifacts::put`, `line_ending_identities`;
`src/mbox.rs` `split`; `src/takeout.rs` `compare`, `Linker`). Measurements
on a real archive are kept outside the repository; this document gives
the rules and the reasoning.

## Terms

| Term | Meaning |
|---|---|
| origin | a place mail is read from: an mbox tree or file, a Gmail account (API), an IMAP server, a Google Takeout file |
| copy (artifact) | one exact stored message: its header bytes and body bytes, each kept by SHA-256 |
| sighting | a copy seen at a path (folder) in an origin during an import or sync run |
| message | one email, however many copies of it were stored; what search counts and `mN` names |
| identity | the key that puts a copy into a message (below) |
| fold | an explicit, reasoned link putting a copy into a message although its identity differs |

## What is stored

- **Every copy, byte for byte.** Nothing is rewritten: no line-ending
  change, no header removal, no re-encoding. Header and body are stored
  as separate objects named by their SHA-256; identical objects are
  stored once.
- **Header/body split** (`split_message`, `header_end`): the header ends
  after the first empty line, i.e. at the first `\n` that follows `\n`,
  or follows `\r` that follows `\n` (so `\n\n` and `\r\n\r\n` both end
  it). Everything after is the body. With no empty line, the whole copy
  is header and the body is empty.
- **Two copies are the same copy** only if both header and body bytes are
  identical (`UNIQUE (header_hash, body_hash)`). A copy seen in many places
  is stored once with a sighting for each.

### How copies are cut out of mbox files (`mbox::split`)

- A message starts at a `From ` line that parses as a separator:
  `From <sender> <weekday> <month> <day> <hh:mm:ss> <year>` with an
  optional zone. A line starting `From ` that doesn't parse is body text.
- A message ends at the next such line that follows a `\n`. The single
  `\n` (or `\r\n`) before it is the separator and not part of the body;
  at end of file, a trailing empty line is the separator.
- **Dovecot mode** (`mbox = "dovecot"`): bodies are kept as written
  (Dovecot doesn't quote `From ` lines). If the header has
  `Content-Length: N` and the scanned end comes *before* header-end + N,
  and header-end + N is exactly a valid end (a newline then a valid
  `From ` line, or end of file), the message extends to it: this is how a
  body that contains a `From ` separator line stays one message.
  `Content-Length` never shortens a message.
- **mboxrd mode** (`mbox = "mboxrd"`, e.g. Google Takeout): one `>` is
  removed from each `>…>From ` body line; `Content-Length` is ignored.
- Consequence: an mbox cannot record whether a message ended with a
  newline. A writer must put a newline before the next `From ` line, so a
  message that had none gains one in the file, and a reader gets it back as
  part of the body.

## Message identity

A copy belongs to the message whose **(identity of the body, Message-ID,
Date)** equals its own; if none exists, a new message is created. All
three must match.

- **Message-ID**: the value of the first `Message-ID` header field,
  unfolded, RFC 2047-decoded and trimmed; compared as a case-sensitive
  string. Missing is NULL, and NULL matches NULL.
- **Date**: the first `Date` header field parsed to Unix seconds
  (`mailparse::dateparse`). Missing or unparsable is NULL, matching NULL.
- **Identity of the body** (`import::identity`): SHA-256 of the body after
  three readings, and nothing else:
  1. each `\r\n` is read as `\n`;
  2. each NUL byte (`0x00`) is read as `0x80`;
  3. if the result doesn't end with `\n`, one `\n` is added.

  A body with only `\n` line endings, no NUL and a final newline (most
  mbox mail) has identity = its plain SHA-256.
- **Headers are not part of identity.** Copies that differ only in header
  bytes (a mail server's added fields, Dovecot's own fields, line endings,
  Gmail's Takeout fields) are different copies of one message.

### Why each reading

| Reading | Because | Where it shows |
|---|---|---|
| CRLF as LF | Mail's canonical form (RFC 5322) and IMAP's wire form (RFC 3501/9051) use CRLF; Unix mbox files store LF; Dovecot converts between them (`mail_save_crlf = no`, the default, stores LF). Gmail stores CRLF, so its API and Takeout copies are CRLF. | the same email from an mbox file and over IMAP |
| NUL as 0x80 | IMAP literals can't carry NUL; Dovecot sends each NUL as `0x80`. | mail with NUL in its body, from an mbox file and over IMAP |
| missing final newline added | An mbox must end a message with a newline (above), so a message that had none (common in mail composed in Gmail's web interface) gains one in the file; the original (API, IMAP) has none. | Gmail API vs Takeout; Dovecot IMAP vs its mbox file |

All three are conversions a store or protocol performs on the same email,
not differences between emails. Every source goes through the same
function: mbox and Takeout imports (`Importer`), Gmail API sync
(`gmail.rs`), IMAP sync (`imap.rs`); there is no other code path that
creates a message.

### What identity deliberately does not absorb

- Anything in headers (they aren't part of it).
- Re-encoding: base64 vs quoted-printable, different line lengths,
  different MIME boundary strings, charset conversion. Gmail's own copies
  of one sent email can differ this way (its Sent copy and the delivered
  copy had different boundaries); those are separate messages.
- Lost or damaged bytes: truncation, bytes replaced by U+FFFD. A damaged
  copy and an intact copy of one email have different identities.
- Whitespace other than the final newline (trailing spaces, an extra
  blank line at the end).

## Folds: explicit links

Where identity can't join two copies of one email, a fold can, with a
recorded reason. Today only Google Takeout copies are folded, to the
Gmail API copy of the same message (`takeout::compare`, run at Takeout
import and Gmail sync, and by `mmadm reconcile`). A Takeout copy folds to
an API copy when:

- it is a candidate: an API copy in the same Gmail thread (Takeout's
  `X-GM-THRID`, as hex, equals the API thread id);
- Message-IDs are equal;
- the Takeout header is the API header preceded only by Takeout's
  `X-GM-THRID`, `X-GM-*`, `X-Gmail-*` lines; and
- the bodies are equal, or the Takeout body is the API body plus a final
  `\r\n` (`final-crlf`), or either holds after replacing invalid UTF-8 in
  the API copy with U+FFFD (`utf8-damage`, Takeout's own corruption).

The folded copy moves into the API copy's message; the message it leaves,
if then empty, is deleted (its derived indexes dropped). With the
final-newline reading, `final-crlf` pairs now also share an identity, so
the Takeout copy joins at once; the fold is still recorded, marking the
API copy as the message's own.

## Which copy is shown

A message's copies are all kept and listed (`read` shows every place
each was seen). One copy is used for content:

- indexing (parts, attachments, text): the earliest stored copy that isn't
  folded;
- `read` and `show`: the copy that isn't folded and was seen most
  recently;
- `export`: the copy seen most recently, folded or not.

These differ; see the open questions.

## When the rule changes

A database records the rule its keys follow (`meta` key `identity`,
now `lf-nul-eol`). When a newer program opens it, before the next import,
sync or index stores anything, every message's identity is recomputed
from its own copy (one that isn't folded), and messages whose (identity,
Message-ID, Date) now coincide are merged into one: copies moved, the
emptied message and its derived indexes deleted. Message ids (`mN`) of
surviving messages don't change; stored bytes never change.

## Damaged and out-of-spec mail

munchymail keeps damaged mail as it finds it, and dedup never hides
damage: a damaged copy and an intact copy of one email have different
identities, so both are kept as separate messages unless explicitly
folded.

Detectors that have worked:

- **Dovecot's `Content-Length` vs the body.** Dovecot writes
  `Content-Length: N` (the body's bytes as saved) into each message it
  saves to an mbox. If the body munchymail cut out of the file is not N
  bytes, the file changed after Dovecot saved the message. (Not every
  message has the field: mail written into the file by other programs
  lacks it.)
- **An unclosed MIME boundary.** A complete `multipart/*` message ends
  with `--<boundary>--`. A missing close means truncation, or a sender
  that built the message wrongly (common in spam).
- **Comparing two views of one mailbox**, e.g. its mbox files and its IMAP
  server: after the three readings above they should agree copy for
  copy; any other difference is either a parsing disagreement or damage.

## Risks, as we see them

- **False merges** (two different emails made one message) need equal
  Message-ID, equal Date and bodies that differ only in line endings,
  NUL vs 0x80, or one final newline. We consider this implausible for
  distinct emails. A merge moves copies, never deletes bytes.
- **Missed merges** (one email as two messages) remain for re-encoded
  copies, damaged copies, and copies whose Message-ID or Date parse
  differently. They show as duplicates in search; nothing is lost.
- **mbox splitting** depends on `From ` lines and, in Dovecot mode, on
  `Content-Length` only to extend. A body with a valid `From ` separator
  line and no `Content-Length` is split in two.

## Open questions for review

1. Should identity also read each NUL as 0x80 for sources other than
   IMAP, or only when comparing with IMAP? (Today: always.)
2. Is adding a missing final newline safe for every source, e.g. binary
   or `Content-Transfer-Encoding: binary` bodies?
3. Should Message-ID be compared case-insensitively, or without angle
   brackets? Date with a tolerance?
4. Should `read`, `export` and indexing pick the same copy? Which:
   earliest, most recently seen, or by origin (live over snapshot)?
5. Should a detected truncation (e.g. `Content-Length` larger than the
   body, or an unclosed boundary) be recorded on the copy, so search can
   show or exclude damaged copies?
6. Should copies whose bytes are a prefix of another copy of the same
   Message-ID (truncation) be linked to the complete one, as Takeout
   damage is?
