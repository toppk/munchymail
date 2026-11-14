# MIME extraction

Not built. Storing a large attachment once, however many messages carry it,
without ever risking a message's exact bytes. Part of the
[architecture](architecture.md); today munchymail indexes MIME parts
(`attachments`, `--attachment`) but stores every body whole.

## The opportunity

In a corporate or personal archive, the same large attachment may appear repeatedly: sent copies, received copies, forwards, folder copies, and repeated distributions. Whole-message dedupe cannot capture this if transport headers differ. MIME-aware extraction can.

## The correctness problem

The obvious optimization is to decode a MIME attachment, hash the decoded content, store it once, and reconstruct the message on demand. The difficulty is that the same semantic attachment can have multiple valid wire representations. Base64 characters are deterministic for a given byte stream, but MIME allows arbitrary permitted whitespace and line wrapping. A deliberately pathological message could vary those break positions stochastically.

Quoted-printable and malformed MIME are even less attractive. Cryptographic structures such as S/MIME or PGP/MIME further raise the cost of any transformation that is not byte-perfect.

## The chosen rule: optimize only when safe

The key design decision is to make MIME-aware deduplication an optimization, never a correctness requirement. If a MIME part is conventional and the system can externalize it while guaranteeing exact reconstruction with a compact recipe, do so. If the part is malformed, ambiguous, cryptographically risky, or simply strange, leave the original bytes intact.

- Conventional Base64 attachment: candidate for extraction.
- Arbitrary or adversarial whitespace pattern: keep raw unless the recipe is still worthwhile.
- Malformed MIME or parser disagreement risk: keep raw.
- Signed/encrypted structure where byte identity matters: default to raw.
- Unknown content: raw storage remains fully valid.

> **Architectural consequence:** MIME parsing is an optimization pass, not part of the minimum storage contract. Failure to understand a message never means failure to preserve it.

## Extent representation for extracted bodies

Once MIME extraction exists, a body no longer needs to be represented as one raw object. It can become an ordered sequence of exact literal byte extents and referenced immutable objects. This is a rope-like representation.

```
body representation:
  extent 0: literal bytes
  extent 1: object hash A + reconstruction recipe
  extent 2: literal bytes
  extent 3: object hash B + reconstruction recipe
  extent 4: literal bytes
```

The extent model also keeps the initial implementation simple: version 1 can represent every body as one raw extent. MIME extraction can be added later without changing the higher-level email model.

## Schema sketch

If MIME extraction is added, body_hash can resolve to a body representation that contains ordered extents. A raw body remains representable as one literal/object extent, so migration is straightforward.

```sql
CREATE TABLE body_representation (
  body_id INTEGER PRIMARY KEY,
  exact_length INTEGER NOT NULL
);
CREATE TABLE body_extent (
  body_id INTEGER NOT NULL REFERENCES body_representation(body_id),
  ordinal INTEGER NOT NULL,
  kind TEXT NOT NULL, -- literal | object | encoded_object
  object_hash BLOB REFERENCES object(hash),
  literal_bytes BLOB,
  recipe BLOB,
  logical_length INTEGER NOT NULL,
  PRIMARY KEY(body_id, ordinal)
);
```
