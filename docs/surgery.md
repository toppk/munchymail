# Repository surgery

Mostly not built. Rebuilding mailboxes from the archive: rules that classify
the whole history and can be replayed, views made from them, and exports to
Maildir, mbox or IMAP. Part of the [architecture](architecture.md). Built so
far: the search side (most of "Search and a notmuch-like interface") and
`export`, a `.tgz` of matching messages.

## Classification as Durable, Replayable Rules

The corpus should treat folders and labels as derived views whenever possible. A rule is not a one-time destructive action; it is a durable predicate that can be re-run over the entire archive whenever the corpus grows or the rules change.

```
rule: from_address IN my_addresses
  -> class = sent
rule: class = sent AND message_date = 2026-10
  -> view = Sent/2026/10
rule: sender IN family_addresses
  -> class = family
rule: merchant classifier matches
  -> class = shopping
```

Each classification result should record which rule produced it and, ideally, the rule version. That makes classifications explainable: “why is this message in this view?” can be answered mechanically.

### Facts, inferences, and views

| Concept | Role |
|---|---|
| Fact | Directly extracted or observed: raw From field, source path, Message-ID, exact body hash. |
| Inference | A rule-derived statement: this is sent mail; this belongs to family; this is a shopping receipt. |
| View | A presentation decision: place sent mail in Sent/YYYY/MM; expose shopping mail under Shopping. |

Keeping these distinct prevents a historical folder tree from becoming the definition of reality. The old folder can remain valuable provenance while a new ruleset produces a cleaner mailbox view.

## Offline “Repository Surgeon” Workflow

The most immediate application is an offline mail rewriter analogous in spirit to repository-surgeon tools: ingest messy historical repositories, preserve every input, derive a canonical graph of messages and provenance, transform classifications declaratively, inspect the result, and export a new repository.

1. Discover sources and register them without modifying them.
2. Parse source containers (mbox, Maildir, individual messages) into exact artifacts.
3. Split each artifact into exact raw header and body objects and hash them.
4. Store small objects inline; externalize large objects by hash when appropriate.
5. Record an observation for every source occurrence.
6. Build derived indexes: parsed headers, normalized addresses, dates, MIME structure, full-text terms.
7. Perform conservative exact dedupe first; optionally perform higher-level logical-message grouping later.
8. Run classification rules over the entire corpus.
9. Review anomalies, unclassified mail, conflicting provenance, and suspected duplicate groups.
10. Materialize desired mailbox views.
11. Export to Maildir, mbox, RFC 5322 files, or a live IMAP server.
12. Repeat without re-ingesting or destroying the recovered corpus.

## Search and a notmuch-like Interface

The tool should feel notmuch-like in one important respect: the user queries a corpus rather than traversing a folder tree. But the corpus can expose more provenance and restructuring primitives than a normal mail index.

- Search by parsed mail fields, full text, source, historical folder, classification, attachment hash, or import run.
- Ask for all observations of one logical message.
- Ask for messages whose current classification disagrees with historical placement.
- Ask for duplicate attachment objects and the messages that reference them.
- Apply or preview rule changes transactionally.
- Materialize an export view without altering canonical objects.
- Produce explainability: show which rule put a message into a given class/view.

## Export and Synchronization

### Offline export

For the initial tool, export can be explicit and one-way. A selected rule-generated view can be written to Maildir, mbox, or individual RFC 5322 files. Because the corpus is preserved, exports are disposable and repeatable.

### IMAP export

An IMAP server can be treated as a presentation target: create folders from the desired view and upload messages. Historical source folders need not become output folders unless the chosen rules say so.

### If the tool later becomes live

A live system changes the authority question. If users read, flag, delete, move, and receive messages through IMAP, those mutations must be reflected into the corpus. There are two possible paths: synchronize a separate corpus with the live server, or make the corpus itself the storage backend. The second is architecturally cleaner but requires deeper integration with the mail server.
