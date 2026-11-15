---
title: Overview
hero: true
hero-eyebrow: Mail archive · self-hosted · built for people and agents
hero-title: Every copy of every message, kept.
hero-lede: munchymail gathers mail from live mailboxes, old backups and Gmail into one archive and keeps it current as mail arrives. It keeps each message byte for byte, remembers every place it was ever found, and lets you search all of it, from your terminal or through an AI agent.
hero-image: assets/logo-512.webp
hero-links:
  - { label: Start the tutorial, href: tutorial.html, kind: primary }
  - { label: Read the design, href: design.html, kind: secondary }
  - { label: Source on GitHub, href: "https://github.com/toppk/munchymail", kind: secondary }
description: munchymail is a self-hosted mail archive that keeps every copy of every message, follows live mailboxes as mail arrives, records where each copy was found, and makes the whole history searchable.
---

Mailboxes forget. Folders get reorganised, filters move things, a backup
from three years ago holds the only copy of something that mattered. munchymail
reads all of it and forgets nothing.

::: cards
::: card
[History]{.label}

### Search the whole history

Every message, every folder it was ever in, and when it arrived or left.
Search headers, text, attachments and scanned documents across every account.
:::

::: card
[Live]{.label}

### Follow mail as it arrives

Gmail and IMAP mailboxes are kept current within seconds to minutes. Every
arrival, move, deletion and flag change becomes history, and backups sit
alongside as snapshots.
:::

::: card
[Index]{.label}

### Index the bytes, not the folders

Messages are stored by the hash of their exact bytes. Folders, labels and
duplicates are interpretations laid on top, and can be rebuilt.
:::
:::

::: safety
munchymail never writes to a mail source. It reads IMAP folders with EXAMINE
and BODY.PEEK (not even `\Seen` changes), mbox files under the same shared
lock a Dovecot reader takes, and Gmail with a read-only sign-in, and writes
only its own data directory.
:::

## Searchable history

Everything munchymail has seen stays searchable, including mail that has
since been deleted, moved or filtered away.

- **Headers, the way Sieve sees them.** Filter on decoded header values with
  Sieve's wildcard matching: `--from '*@github.com'`, `--participant`,
  `-H list-id:*announce*`, or a regular expression. Tools for studying headers
  (`top`, `report`, `profile`) show which values dominate, for writing filter
  rules against real mail.
- **Full text, attachments included.** Stemmed search over bodies, text
  attachments and attached messages, plus text pulled from documents and OCR of
  images and scanned PDFs. The extractors run in a sandbox with no network.
- **What changed, and where it went.** `changes` compares any two imports of
  a mailbox: what was added, removed or moved between folders. `removals`
  lists messages that left a folder and where they turned up.
- **Every account at once.** Without `--account`, a search covers every
  account in turn, the one with the newest match first.

```
munchymail search --participant '*@accountant.example' --since 2024-01-01
munchymail search --text '"tracking number" -amazon' --attachment pdf
munchymail changes --origin mailhost-dovecot
munchymail read m1834
```

## Live syncing

The service keeps every live origin current by itself, and remembers what
it saw on the way.

- **Mail as it lands.** It holds IMAP IDLE on the INBOX and checks about
  five seconds after anything happens there; other folders and Gmail are
  checked every three minutes. `munchymail sync` checks everything now, so
  an agent can ask for the latest mail before it searches.
- **Only what changed.** Each IMAP check asks the server what changed since
  the last one (CONDSTORE/QRESYNC), so a quiet check takes a fraction of a
  second, even after a restart. Gmail is followed through its history API.
- **History that costs what changed.** Each message's presence in a folder
  or label is an interval: from the check it appeared in to the check it
  left. A new mail is one row, however big the folder. Flags and labels,
  read state included, are kept at every change.
- **Failures stay small.** Network trouble is retried; a refused login stops
  only that mailbox and says so in `overview`; a broken database stops the
  service instead of looping.

```
munchymail sync
munchymail overview
munchymail removals --folder INBOX
```

## Snapshots of live sources

A source is an *origin*: a live mbox tree, a backup found on an old disk, a
Google Takeout, a Gmail account. Each has a role (`live`, `snapshot` or
`export`), and each import of it is recorded as a run.

- **Cheap, so you can do it often.** A file identical to one already seen,
  in any origin, is recognised by its hash and only recorded as seen again.
  A mailbox that has grown since the last import is recognised as the old file
  plus new mail. Re-importing an unchanged mailbox takes about a second.
- **Backups join the same history.** A copy of a mailbox from years ago is
  just another origin. Its messages fold into the ones you already have, and
  the archive records that they were there too.
- **Gmail without giving it the keys.** Gmail comes in through its API with a
  read-only sign-in. Each sync records the labels as they were, and when Gmail
  deletes a message, the archive notes the date and keeps the message.

## A different kind of index

Most mail tools treat the folder tree as the truth. munchymail treats the exact
bytes of each message as the evidence, and everything else as an
interpretation of them.

| Layer | What munchymail keeps |
|---|---|
| object | bytes by SHA-256; small ones inside SQLite, large ones in a content store |
| artifact | one exact message: its header bytes and body bytes, hashed separately |
| observation | where a copy sat: the file, the byte offset, its mbox `From ` line |
| message | one email however many copies exist; its id (`m1834`) survives Dovecot rewriting its headers |
| derived | decoded headers, MIME parts, full text, OCR: versioned and rebuildable |

And because every copy is kept with how it was framed, the original mbox
files can be rebuilt from the archive alone: `mmadm verify-files` rebuilds
each one and checks it against the hash recorded when it was read.

Damage is noted, not hidden: a copy whose body is shorter than its server
recorded, a multipart message cut off before its end, or a copy that is
the start of a longer one is marked as such (`search --mark`), with what
was measured, so the intact copy, if the archive has one, can be found.
Once its owner has judged a copy damaged (`mmadm copy damaged`), it leaves
search and the indexes but is still kept and readable, with the reason and
the copy that repaired it.

Because storage is deduplicated but history is not, a message seen in nine
backups is stored once and remembered nine times. When copies differ only in
the headers a mail server adds, they are still one message; when an export
damaged a copy, the archive records why the two were joined.

The same email read from an mbox file, over IMAP, from the Gmail API and from
a Takeout is one message: the differences each store makes (line endings, how
a NUL byte travels, the newline an mbox adds) are read alike, and nothing
else is. Message ids never change meaning: a message whose copies joined
another points to it, and no id is reused. The rules, and why they are safe,
are in [Identity and duplicates](identity.html).

## Made for agents too

An agent can search and read the archive as easily as a person can, and
can't change anything it wasn't asked to.

::: agent
`munchymail describe --json` lists every command, its arguments and whether it
only reads or writes. `search --json` returns JSON Lines with stable paging,
and `read --json` marks every message's content as untrusted, so an agent
treats mail as data, never as instructions.
:::

The archive runs as a small service on a Unix socket, so searches answer while
a long import or OCR run carries on, and the setup commands live in a separate
program, `mmadm`, that agents leave alone unless asked.

## Just getting started

munchymail already keeps, searches and exports a lifetime of mail. Next:

- **Every folder at once:** IMAP NOTIFY, so mail filed by server-side rules
  into any folder arrives as quickly as INBOX mail.
- **Rules that can be replayed:** classification as rules over the whole
  history, not one-time moves, with every result naming the rule that made it.
- **New mailboxes from old mail:** reconstructed folders exported as Maildir or
  mbox, built from the archive rather than edited in place.
- **A web interface,** and later a phone.
