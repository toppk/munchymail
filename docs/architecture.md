# Architecture

The design munchymail was built from: why its storage looks the way it
does, what it keeps and why. It was written before the code, in 2026, as a
Word document; this is its text, lightly edited. Most of it is built; for
where things stand see [design.md](design.md), and for the schema as it
really is, [index.md](index.md). Two parts are their own documents:
[MIME extraction](mime-extraction.md) (future) and
[repository surgery](surgery.md) (rules, views and export, mostly future).

> **Design thesis:** Treat exact mail bytes as immutable evidence; treat organization as a reproducible interpretation. SQLite owns the corpus, indexes, provenance, classifications, and small objects. Large immutable objects can live in a content-addressed store. MIME extraction is optional and must never be required for correctness.

## Executive Summary

The immediate project is an offline mail corpus tool: something in the spirit of notmuch, but designed to ingest mbox, Maildir, individual RFC 5322 messages, backups, exports, and potentially live IMAP snapshots. It should allow a mail archive to be deduplicated, queried, classified, reorganized, and exported without treating the historical folder hierarchy as the source of truth.

The motivating use case is recovery and restructuring after mailbox loss: ingest every surviving copy of every sent and received message, preserve where each copy came from, deduplicate them without losing provenance, derive classifications with replayable rules, and then export reconstructed mailbox views. The same architecture can later become a permanent mail store or a custom Dovecot backend, but that is deliberately not required for the first implementation.

- The canonical unit is an exact message artifact, represented by exact raw header bytes plus exact raw body bytes.
- Header and body objects are independently content-addressed, allowing exact duplicate portions to collapse naturally.
- Small objects can be stored inline in SQLite; large immutable objects can be stored externally by content hash.
- Parsed fields such as Subject, From, To, Date, Message-ID, MIME structure, and search terms are indexes over the canonical bytes, not replacements for them.
- Provenance is first-class: every observed occurrence of a message records the source, path/folder, filename, timestamps, flags, and import run.
- Classification is derived and replayable. “Sent,” “Family,” “Shopping,” “Work,” and folder placement should be consequences of durable rules, not destructive one-time moves.
- MIME attachment extraction is an optimization. Safe, conventional parts can be externalized and deduplicated; pathological or ambiguous parts remain raw.
- The design remains useful as an offline archive even if live Dovecot integration proves awkward.

## Problem Statement and Goals

### Why an offline corpus?

Traditional mail stores conflate several different concerns: the exact message bytes, the mailbox or folder in which a message happens to appear, mutable IMAP state, search indexes, and historical provenance. That is convenient for a mail client but poor for forensic recovery and large-scale reorganization.

A recovery corpus should instead make it possible to ingest everything first and decide later what it means. The tool should be able to answer both “what is this message?” and “where did I find this particular copy?” without forcing those answers to be the same thing.

### Primary goals

- Ingest heterogeneous sources: mbox, Maildir, individual .eml/RFC 5322 files, archive exports, old backups, and later potentially IMAP.
- Preserve exact raw bytes for evidentiary and cryptographic fidelity.
- Deduplicate exact duplicates and, where safe, large repeated MIME content.
- Retain every source occurrence and historical folder placement as provenance.
- Provide fast structured search and SQL-accessible metadata.
- Support durable, replayable classification rules over the complete historical corpus.
- Permit offline “repository surgery”: restructuring mailboxes repeatedly without mutating or losing the underlying corpus.
- Export reconstructed views to Maildir, mbox, RFC 5322 files, or IMAP/Dovecot.
- Keep a path open for later live operation, without designing the first version around live-server constraints.

### Non-goals for the first implementation

- Replacing Dovecot immediately.
- Cross-user/global deduplication.
- Perfect semantic normalization of every malformed MIME message.
- Treating attachment extraction as mandatory.
- Reconstructing every historical folder placement solely from heuristics; preserved provenance remains available when rules cannot infer intent.

## How We Arrived Here: Existing Mailbox Formats

The design discussion began with Dovecot storage formats because they expose the trade-offs that a new backend must resolve.

### mbox

mbox concatenates many messages into a mailbox file. It minimizes inode count, but message deletion and mutation create rewrite/locking complexity. In current Dovecot it is a legacy format rather than the preferred basis for new installations.

### Maildir

Maildir stores one message per file. Its major virtue is operational simplicity: messages are naturally isolated and filesystem operations are easy to reason about. Its cost is filesystem scale. A mailbox with 100,000 messages means roughly 100,000 message files, plus indexes and directories. At large scale this becomes a metadata/inode problem.

### sdbox and mdbox

Dovecot’s dbox family separates message storage from Dovecot’s authoritative mailbox indexes. sdbox keeps one message per file; mdbox packs multiple messages into larger container files. mdbox reduces inode count, but then needs its own mapping layer and purge/compaction machinery because deletions leave dead records inside append-oriented containers.

That led to the central observation: mdbox has already built a small specialized database. It uses one subsystem for mailbox/index state and another for packed message data. The proposed design asks whether a general embedded database plus immutable content storage can provide a cleaner version of the same separation.

| Concept | Role |
|---|---|
| mbox | Few files; expensive mutation/rewrite; mailbox file itself carries much of the storage representation. |
| Maildir | One file per message; simple and rebuildable; inode-heavy at scale. |
| sdbox | One message per file, but Dovecot indexes are authoritative for mailbox semantics. |
| mdbox | Many messages per container; fewer inodes; requires mapping plus purge/compaction. |
| Proposed corpus | SQLite for logical state and small immutable objects; hash-addressed external objects for large data; no application-level container compaction. |

## Fundamental Data Model

> **Core rule:** Raw bytes are truth; SQL structure is an interpretation of the truth.

### Split the RFC 5322 artifact into headers and body

An email artifact has two natural rigid byte sequences separated by the RFC 5322 header/body separator: the raw header block and the raw body. The proposed base representation therefore does not need to store a monolithic message blob. It can compose a message from independently content-addressed header and body objects.

```
email artifact
  -> header_hash -> exact raw header bytes
  -> body_hash   -> exact raw body bytes
```

This split immediately enables partial deduplication. Two copies with different Received or filtering headers can still share an identical body. Byte-identical copies share both objects.

### Why raw headers remain bytes, not a key/value table

RFC 5322 headers are not a dictionary. They are an ordered sequence. Duplicates are legal and often significant. Exact capitalization, folding, whitespace, and line endings may matter for forensic fidelity, signatures, and simple reproducibility. A canonical headers(name, value) table cannot regenerate an arbitrary original header block.

Therefore the raw header block remains the canonical object. Parsed header rows and convenient columns are derived indexes. An ordinal can preserve order for query purposes, but the raw bytes remain authoritative.

### Parsed fields are materialized indexes

Columns such as subject, message_id, parsed date, normalized sender, or recipient are conveniences. They can be regenerated from the canonical header bytes if parsers improve. The same applies to normalized address tables, thread relationships, MIME structure, and full-text search indexes.

## Physical Storage: SQLite Plus Content-Addressed Objects

### Small objects inline

SQLite is not merely a metadata catalog. It can be the primary storage for small immutable header and body objects. This is the cleanest way to avoid recreating the Maildir inode problem for tiny messages.

A threshold such as 10 KiB is an implementation starting point, not a principle. The threshold should eventually be measured. SQLite can store much larger BLOBs; externalization is about operational behavior, dedupe opportunity, backup amplification, and keeping very large payloads out of the database’s page/WAL lifecycle—not about a hard inability to store large rows.

### Large objects external by hash

When an object crosses the externalization threshold, the database retains the hash, logical length, storage class, and other metadata, while the bytes live in an immutable content-addressed object store. The content store does not need to understand whether an object is a whole body, a MIME part, a PDF, or another byte extent.

```
object(hash PRIMARY KEY,
       length,
       storage_class,  -- inline | external
       inline_bytes,   -- NULL when external
       media_hint,     -- optional / derived
       created_at)

filesystem:
  objects/ab/cd/<full-hash>
```

### Why this avoids mdbox-style compaction

An external object is immutable. It is either referenced or it is not. Deleting mail does not punch holes in a shared append-only container. Garbage collection can remove an object when no corpus records reference it. SQLite internally handles its own free pages and vacuum/checkpoint policy; the application does not need to implement a mail-specific compactor.

> **Important distinction:** There may still be SQLite maintenance and object garbage collection, but that is not the same problem as rewriting packed mail containers to eliminate dead records.

## Provenance: Preserve Every Observation

Deduplication must never erase history. If the same artifact is found in five backups and three folders, the corpus should collapse storage while preserving all eight observations.

| Concept | Role |
|---|---|
| source | A backup set, mbox file, Maildir tree, filesystem directory, export, IMAP account snapshot, etc. |
| artifact | An exact header/body byte representation. |
| observation | The fact that a particular artifact was found in a particular source at a particular location. |
| logical message | An optional higher-level identity grouping artifacts believed to represent the same email. |
| classification | A derived interpretation such as sent, received, family, shopping, work, project X. |

Useful observation fields include original path, mbox offset, Maildir filename, historical folder, IMAP UID when known, source timestamps, original Maildir flags, and the import run that discovered it.

## Message Identity and Deduplication

### Exact artifact identity

Hashing the exact header and body objects gives a strong identity for exact artifacts. Two artifacts with identical header_hash and body_hash are byte-identical messages and can be collapsed immediately.

### Logical message identity

Exact identity is not enough for recovery. The same logical email may acquire new Received lines, spam annotations, delivery metadata, or other headers as it moves through systems. A later phase can group multiple artifacts into one logical message using evidence such as Message-ID, stable signed headers, body identity, dates, normalized participants, and transport chronology.

This grouping should be explicit and reversible. It is better to retain two suspiciously similar logical messages than to destroy provenance through an over-aggressive merge.

### DKIM and mutable headers

Mail infrastructure routinely adds headers. DKIM reflects this reality by signing the body and a specified list of headers rather than the entire evolving header set. This supports the corpus model: multiple transport artifacts may legitimately represent the same logical message.

Archive-specific headers are therefore acceptable as derived/export representations, but the originally ingested raw artifact should remain addressable. Provenance may also simply remain in SQL rather than being physically injected into raw messages.

## Dovecot Integration: Why It Is Optional

Dovecot is attractive because it already provides mature IMAP, LMTP/LDA, Sieve, authentication, concurrency handling, QRESYNC/CONDSTORE, and mailbox semantics. The discussion explored replacing its message storage with the proposed corpus.

The complication is Dovecot’s index architecture. Its indexes are not merely disposable search accelerators in dbox formats; they carry mailbox membership, UIDs, flags, keywords, change state, and backend-specific mappings. A normal storage driver can plug in message-storage behavior, but replacing Dovecot’s index engine with SQLite does not appear to be a first-class public plugin seam.

> **Practical conclusion:** Do not make Dovecot integration a prerequisite for the corpus. Build the corpus and offline repository-surgeon first. Later source-level investigation can determine whether Dovecot can use it cleanly as a backend or whether an adapter/synchronizer is preferable.

## Failure Modes and Safety Rules

- Never delete or rewrite the only exact copy of an ingested artifact during import or normalization.
- Treat parser output as derived. A parser bug must be repairable by re-indexing canonical bytes.
- Never require MIME extraction for readability or exportability.
- Prefer false negatives to false positives in logical-message merging; uncertain dedupe should remain reversible.
- Make every import idempotent. Re-importing the same source should add no duplicate artifacts, only missing provenance observations.
- Use transactional staging for external objects so a committed database reference never points to a missing file.
- Garbage-collect only objects proven unreachable from committed corpus state.
- Preserve enough source metadata to audit why two records were considered duplicates or why a message was classified a certain way.

## Final Architectural Position

The project should begin as a mail corpus and repository-surgeon, not as a Dovecot plugin. Its durable value comes from separating exact evidence from interpretation: exact header and body bytes are immutable objects; provenance records where every copy was found; SQL indexes expose parsed semantics; rules produce repeatable classifications and mailbox views; and large immutable content may be deduplicated externally without forcing complex packed-container compaction.

This design retains the best property of Maildir—simple immutable message content—without requiring one filesystem inode per small message. It retains the best idea behind mdbox—separating mutable mailbox state from bulk content—without requiring a bespoke packed-file database and purge cycle. It also goes beyond both by making provenance, historical recovery, replayable classification, and content-level deduplication first-class concerns.

Most importantly, the corpus remains useful even if it never becomes the live mail store. It can repair, normalize, deduplicate, explain, reorganize, and export an entire lifetime of mail while preserving the raw evidence needed to revisit every decision later.

## Principles

1. Preserve first; interpret second.
2. Hash exact bytes; do not normalize away evidence.
3. Store small immutable objects in SQLite; store large immutable objects by content hash.
4. Make provenance multiplicative: dedupe storage without deduping history.
5. Treat parsed headers, full-text terms, and MIME structure as rebuildable indexes.
6. Treat classifications as replayable rules, not destructive moves.
7. Treat MIME extraction as an optimization that may decline any input.
8. Prefer whole-object garbage collection over packed-container compaction.
9. Keep offline usefulness independent of live-server integration.
10. Design every transformation to be explainable and reversible.
