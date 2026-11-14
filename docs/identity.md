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

- **Every copy, byte for byte**, as delivered by its origin: for an mbox,
  after it is cut out of the file (below), which removes the separator and,
  in mboxrd mode, one `>` from quoted `From ` lines. The file isn't kept
  as a file, but its hash, each message's offset, `From ` line, separator
  bytes and the format are, so it can be rebuilt (not yet checked by a
  command). Nothing is rewritten: no line-ending
  change, no header removal, no re-encoding. Header and body are stored
  as separate objects named by their SHA-256; identical objects are
  stored once.
- **Header/body split** (`split_message`, `header_end`): the header ends
  after the first empty line, i.e. at the first `\n` that follows `\n`,
  or follows `\r` that follows `\n` (so `\n\n` and `\r\n\r\n` both end
  it). Everything after is the body. A copy that starts with an empty line
  has an empty header; with no empty line, the whole copy is header and the
  body is empty.
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

A copy belongs to the message whose **key** equals its own; if no active
message has it, a new message is created. The key (`key::key`) is a hash
of five things, all of which must match:

- **Identity of the body** (`key::identity`): SHA-256 of the body after
  three readings, and nothing else:
  1. each `\r\n` is read as `\n`;
  2. each NUL byte (`0x00`) is read as `0x80`;
  3. if the result doesn't end with `\n`, one `\n` is added.

  A body with only `\n` line endings, no NUL and a final newline (most
  mbox mail) has identity = its plain SHA-256.
- **Message-ID** (`key::message_id`): the first `Message-ID` field's raw
  bytes, unfolded, comments removed, parsed as one RFC 5322 `msg-id`. The
  key is the text between the angle brackets, case kept, never RFC
  2047-decoded; a bare `left@right` without brackets counts. None, more
  than one, or a value without exactly one `@` (e.g. `unknown`) is **no
  valid Message-ID**.
- **Date**: the first `Date` field parsed to Unix seconds
  (`mailparse::dateparse`); missing or unparsable is "none", which equals
  "none".
- **MIME reading** (`key::mime`): how the body bytes are to be read:
  - `Content-Transfer-Encoding`, case-folded; missing, `7bit`, `8bit` and
    `binary` all mean "not encoded";
  - the media `type/subtype` and `charset`, case-folded; a missing
    `Content-Type` is `text/plain; charset=us-ascii`;
  - `boundary`, exactly as given (case kept).

  Other parameters (`format=flowed`, …) are not part of it.
- **Header evidence** (`key::header_key`), only for a copy with no valid
  Message-ID: the raw bytes of its `From`, `To`, `Cc`, `Subject`, `Date`,
  `Message-ID` and every `Received` field, in order, with `\r\n` read as
  `\n`. A copy with none of these fields uses its whole header. So copies
  without a Message-ID join only when they are one delivery: the same
  delivery from an mbox file (with Dovecot's `Status`, `X-UID`, …) and over
  IMAP shares all of these.

Other header fields are not part of the key: copies that differ only in a
mail server's added fields, Dovecot's own fields, line endings or Gmail's
Takeout fields are copies of one message.

### Why each reading

| Reading | Because | Where it shows |
|---|---|---|
| CRLF as LF | Mail's canonical form (RFC 5322) and IMAP's wire form (RFC 3501/9051) use CRLF; Unix mbox files store LF; Dovecot converts between them (`mail_save_crlf = no`, the default, stores LF). Gmail stores CRLF, so its API and Takeout copies are CRLF. | the same email from an mbox file and over IMAP |
| NUL as 0x80 | Ordinary IMAP literals can't carry NUL (only `literal8`, with the BINARY extension, can); Dovecot sends each NUL as `0x80` in a normal `BODY[]` fetch. | mail with NUL in its body, from an mbox file and over IMAP |
| missing final newline added | An mbox must end a message with a newline (above), so a message that had none (common in mail composed in Gmail's web interface) gains one in the file; the original (API) has none. | Gmail API vs Takeout. (Between Dovecot's mbox file and its IMAP view it joined only one pair, a damaged message IMAP serves one byte short: a coincidence, not support.) |

All three are conversions a store or protocol performs on the same email,
not differences between emails. One consequence: an empty body and a body
of a single `\n` have the same identity. They apply to every source, not
only to the pair that showed them: a deliberate tradeoff, since a false
merge through them also needs equal Message-ID, Date and MIME reading.
Every source goes through the same function: mbox and Takeout imports (`Importer`), Gmail API sync
(`gmail.rs`), IMAP sync (`imap.rs`); there is no other code path that
creates a message.

### What the key deliberately does not absorb

- Header fields outside the key: added, removed or rewritten fields, and
  header line endings.
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
  `\r\n` (`final-crlf`); the header and body tests each also pass if they
  hold after replacing invalid UTF-8 in the API copy with U+FFFD
  (`utf8-damage`, Takeout's own corruption).

The folded copy moves into the API copy's message; the message it leaves,
if then empty, is kept as merged into it (below), its derived indexes
dropped. With the
final-newline reading, `final-crlf` pairs now also share an identity, so
the Takeout copy joins at once; the fold is still recorded, marking the
API copy as the message's own.

## Which copy is shown

A message's copies are all kept and listed (`read` shows every place
each was seen). One copy, its **preferred copy**, is used for content: the
earliest stored copy that isn't folded (or, if all are, the earliest). It
changes only when the message gains a better copy, never because of when
copies were seen, and never by origin: a live source can hold the worse
copy.

Operations on messages (indexing of parts and text, `read mN`, `show mN`,
`search --unique`, `export`) find the matching copies, then their
messages, then each message's preferred copy, whichever copy matched;
`export` says in its index how many messages matched through another copy.
Operations on copies (`read N`, `show N --raw`, `search` without
`--unique`) use exactly the copies named or matched.

## Message ids, and when the rule changes

Messages are never deleted, so an id (`mN`) is never reused. A message
whose copies all join another keeps its row with `merged_into` set; it no
longer counts or appears in search, and every command given its `mN`
follows it to the message it joined and says so (`read`: "m4 was merged
into m10").

A database records the rule its keys follow (`meta` key `identity`, now
`key::RULE`). When a newer program opens it, before the next import, sync
or index stores anything, it regroups (`rekey`), in one transaction:

1. every copy that isn't folded is re-keyed (all keys first, so nothing is
   compared with a key under the old rule); folded copies follow the copy
   they are folded to;
2. copies with one key are one group;
3. each group takes the id of the old message whose preferred copy it
   holds; if it holds several, the lowest id survives and the others are
   `merged_into` it; a group holding none is a new message with
   `split_from` the old message most of its copies came from;
4. messages whose copies or preferred copy changed are indexed again.

An interrupted regrouping changes nothing; a second one changes nothing.
Stored bytes never change.

## Damaged and out-of-spec mail

munchymail keeps damaged mail as it finds it, and dedup never hides
damage: a damaged copy and an intact copy of one email have different
identities, so both are kept as separate messages unless explicitly
folded.

Detectors that have worked:

- **Dovecot's `Content-Length` vs the body.** Dovecot writes
  `Content-Length: N` (the body's bytes as saved) into each message it
  saves to an mbox. If the body munchymail cut out of the file is not N
  bytes, the file and Dovecot's record disagree; either can be the wrong
  one (a file that lost bytes after saving, or a wrong field with an
  intact body). Check the MIME structure and the IMAP view. (Not every
  message has the field: mail written into the file by other programs
  lacks it.)
- **An unclosed MIME boundary.** A complete `multipart/*` message ends
  with `--<boundary>--`. A missing close means truncation, or a sender
  that built the message wrongly (common in spam).
- **Comparing two views of one mailbox**, e.g. its mbox files and its IMAP
  server: after the three readings above they should agree copy for
  copy; any other difference is either a parsing disagreement or damage,
  on either side: the server can serve less than its file holds.
  Agreement shows the views are consistent, not that the copy is
  complete.

## Risks, as we see them

- **False merges** (two different emails made one message) need equal
  Message-ID, Date and MIME reading, and bodies that differ only in line
  endings, NUL vs 0x80, or one final newline. Without a valid Message-ID
  they also need equal header evidence (above). Before the header
  evidence, mail with no Message-ID and an identical body merged with
  nothing else to separate it; this happened (spam from a template,
  delivered four times months apart). A merge moves copies, never deletes
  bytes, and a regrouping can undo it.
- **Two deliveries that agree on every evidence field**, including every
  `Received` line, and lack a Message-ID are one message. In practice they
  are one email.
- **Missed merges** (one email as two messages) remain for re-encoded
  copies, damaged copies, and copies whose Message-ID or Date parse
  differently. They show as duplicates in search; nothing is lost.
- **mbox splitting** depends on `From ` lines and, in Dovecot mode, on
  `Content-Length` only to extend. A body with a valid `From ` separator
  line and no `Content-Length` is split in two.

## Open questions

1. NUL as 0x80 and the final newline: global (today) or only between the
   sources that showed them, as folds?
2. Should a detected length mismatch (`Content-Length` vs body, with its
   direction) or an unclosed boundary be recorded on the copy?
3. Should a copy whose body is a prefix of another's (same valid
   Message-ID and Date) be recorded as a candidate link to it?
4. Should a command rebuild each mbox file from its stored copies and
   framing, and check it against the file's recorded hash?
