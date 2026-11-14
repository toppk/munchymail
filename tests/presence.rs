use std::fs;

use munchymail::api;
use munchymail::changes;
use munchymail::import::Importer;
use munchymail::locations::Sightings;
use munchymail::presence::{self, Listing};
use munchymail::query::Filter;
use munchymail::store::Store;

async fn store_with_copies(dir: &std::path::Path) -> Store {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let mbox: Vec<String> = (1..=3)
        .map(|n| {
            format!(
                "From x  Mon Oct  6 09:00:00 2026\nMessage-ID: <{n}@x>\nSubject: s{n}\n\nbody {n}\n"
            )
        })
        .collect();
    fs::write(tree.join("INBOX"), mbox.join("\n")).unwrap();
    let store = Store::open(&dir.join("data")).await.unwrap();
    for (label, root) in [("m", tree.to_str().unwrap()), ("g", "gmail:me@x")] {
        store
            .conn
            .execute(
                "INSERT INTO origin (label, root, created_at) VALUES (?1, ?2, 0)",
                (label, root),
            )
            .await
            .unwrap();
    }
    Importer::new(&store, "m", munchymail::mbox::Variant::Dovecot)
        .await
        .unwrap()
        .run()
        .await
        .unwrap();
    store
}

fn listing(entries: &[(&str, &str, i64)]) -> Listing {
    let mut l = Listing::new();
    for (path, entry, artifact) in entries {
        l.entry(path.to_string())
            .or_default()
            .push((entry.to_string(), *artifact));
    }
    l
}

async fn ints(store: &Store, sql: &str) -> Vec<Vec<Option<i64>>> {
    let mut rows = store.conn.query(sql, ()).await.unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().await.unwrap() {
        out.push(
            (0..r.column_count())
                .map(|i| r.get::<Option<i64>>(i).unwrap_or(None))
                .collect(),
        );
    }
    out
}

/// Three runs of origin `g`: e2 moves from INBOX to Archive, then e2 and e3 go.
const RUNS: [&[(&str, &str, i64)]; 3] = [
    &[("INBOX", "e1", 1), ("INBOX", "e2", 2), ("Archive", "e3", 3)],
    &[
        ("INBOX", "e1", 1),
        ("Archive", "e3", 3),
        ("Archive", "e2", 2),
    ],
    &[("INBOX", "e1", 1)],
];

async fn folder(store: &Store, path: &str, current: bool) -> usize {
    let f = Filter {
        folder: Some(path.into()),
        current,
        ..Filter::default()
    };
    api::search(store, &f, false, 100, None)
        .await
        .unwrap()
        .0
        .len()
}

#[test]
fn live_listings_write_only_what_changed() {
    let dir = std::env::temp_dir().join(format!("munchymail-presence-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(async {
        let store = store_with_copies(&dir).await;
        let mut runs = Vec::new();
        for r in RUNS {
            runs.push(presence::run(&store, 2, &listing(r)).await.unwrap());
        }
        // 3 rows opened, then 1 opened and 1 closed, then 2 closed: 4 rows in all.
        assert_eq!(
            ints(&store, "SELECT count(*), count(until_run) FROM presence").await,
            [[Some(4), Some(3)]]
        );
        // Removals come from the closed rows, with the move.
        let removals = ints(
            &store,
            "SELECT artifact_id, last_seen_run, gone_run, moved_to IS NOT NULL FROM removal ORDER BY gone_run, artifact_id",
        )
        .await;
        assert_eq!(
            removals,
            [
                [Some(2), Some(runs[0]), Some(runs[1]), Some(1)],
                [Some(2), Some(runs[1]), Some(runs[2]), Some(0)],
                [Some(3), Some(runs[1]), Some(runs[2]), Some(0)],
            ]
        );

        // Places: e1 is current; e2 was in both folders and is in neither now.
        let mut s = Sightings::new(&store).await.unwrap();
        let one = s.of(1).await.unwrap();
        assert!(
            one.iter()
                .any(|l| l.origin == "g" && l.path == "INBOX" && l.current)
        );
        let two: Vec<_> = s
            .of(2)
            .await
            .unwrap()
            .into_iter()
            .filter(|l| l.origin == "g")
            .collect();
        assert_eq!(two.len(), 2);
        assert!(two.iter().all(|l| !l.current));
        assert_eq!(folder(&store, "Archive", false).await, 2);
        assert_eq!(folder(&store, "Archive", true).await, 0);

        // The run differ sees the same history.
        let c = changes::between(&store, &[runs[0]], &[runs[2]], &[])
            .await
            .unwrap();
        let mut gone: Vec<i64> = c
            .removed
            .iter()
            .map(|&i| c.before.copies[i].artifact_id)
            .collect();
        gone.sort();
        assert_eq!(gone, [2, 3]);
        assert_eq!(c.unchanged_files, 0);
        let c = changes::between(&store, &[runs[0]], &[runs[1]], &[])
            .await
            .unwrap();
        assert_eq!((c.moved.len(), c.removed.len()), (1, 0));

        // Listings recorded whole by an older version convert to the same presence.
        let want = ints(
            &store,
            "SELECT origin_id, artifact_id, since_run, until_run FROM presence ORDER BY 1, 2, 3",
        )
        .await;
        let c = &store.conn;
        c.execute("DELETE FROM presence", ()).await.unwrap();
        for (r, entries) in runs.iter().zip(RUNS) {
            for (path, entries) in listing(entries) {
                c.execute(
                    "INSERT INTO file_content (hash, length, head_hash, format, message_count, preamble_length)
                     VALUES (randomblob(32), 0, x'', 'gmail-label', ?1, 0)",
                    (entries.len() as i64,),
                )
                .await
                .unwrap();
                let content = c.last_insert_rowid();
                for (i, (entry, a)) in entries.iter().enumerate() {
                    c.execute(
                        "INSERT INTO observation (content_id, ordinal, offset, artifact_id, from_line, separator, used_content_length)
                         VALUES (?1, ?2, ?2, ?3, ?4, x'', 0)",
                        (content, i as i64, *a, entry.as_bytes().to_vec()),
                    )
                    .await
                    .unwrap();
                }
                c.execute(
                    "INSERT INTO file_sighting (run_id, origin_id, path, content_id, dev, ino, mtime_ns, ctime_ns, changed_during_read)
                     VALUES (?1, 2, ?2, ?3, 0, 0, 0, 0, 0)",
                    (*r, path.as_str(), content),
                )
                .await
                .unwrap();
            }
        }
        assert!(presence::convert_listings(&store).await.unwrap().is_some());
        let got = ints(
            &store,
            "SELECT origin_id, artifact_id, since_run, until_run FROM presence ORDER BY 1, 2, 3",
        )
        .await;
        assert_eq!(got, want);
        assert_eq!(
            ints(
                &store,
                "SELECT count(*) FROM file_content WHERE format = 'gmail-label'"
            )
            .await,
            [[Some(0)]]
        );
        assert!(presence::convert_listings(&store).await.unwrap().is_none());
    });
    fs::remove_dir_all(&dir).unwrap();
}
