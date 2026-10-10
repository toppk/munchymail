use std::fs;
use std::path::Path;

use munchymail::changes;
use munchymail::import::Importer;
use munchymail::store::Store;

fn mbox(msgs: &[(&str, &str)]) -> String {
    msgs.iter()
        .map(|(id, extra)| {
            format!(
                "From {id}@x  Mon Oct  6 09:00:00 2026\nMessage-ID: <{id}@x>\n{extra}\nbody {id}\n"
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn import(store: &Store) -> i64 {
    let mut imp = Importer::new(store, "t").await.unwrap();
    imp.run().await.unwrap();
    changes::record_since_previous(store, imp.origin_id, imp.run_id)
        .await
        .unwrap();
    imp.run_id
}

async fn scenario(dir: &Path) {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("INBOX"), mbox(&[("a", ""), ("b", ""), ("c", "")])).unwrap();
    fs::write(tree.join("Archive"), mbox(&[("z", "")])).unwrap();
    let store = Store::open(&dir.join("data")).await.unwrap();
    store
        .conn
        .execute(
            "INSERT INTO origin (label, root, created_at) VALUES ('t', ?1, 0)",
            (tree.to_str().unwrap(),),
        )
        .await
        .unwrap();
    let r1 = import(&store).await;

    // a deleted, b moved to Archive with a new header, c's flags rewritten, d delivered.
    fs::write(
        tree.join("INBOX"),
        mbox(&[("c", "Status: RO\n"), ("d", "")]),
    )
    .unwrap();
    fs::write(
        tree.join("Archive"),
        mbox(&[("z", ""), ("b", "Status: O\n")]),
    )
    .unwrap();
    let r2 = import(&store).await;

    let c = changes::between(&store, &[r1], &[r2], &[]).await.unwrap();
    assert_eq!(c.unchanged, 1, "z");
    assert_eq!(c.changed.len(), 1, "c");
    assert_eq!(c.moved.len(), 1, "b");
    assert_eq!(c.after.copies[c.moved[0].1].path, "Archive");
    assert_eq!(c.removed.len(), 1, "a");
    assert_eq!(c.added.len(), 1, "d");

    let mut rows = store
        .conn
        .query(
            "SELECT path, last_seen_run, gone_run, coalesce(moved_to, '-') FROM removal ORDER BY moved_to",
            (),
        )
        .await
        .unwrap();
    let mut got = Vec::new();
    while let Some(r) = rows.next().await.unwrap() {
        got.push((
            r.get::<String>(0).unwrap(),
            r.get::<i64>(1).unwrap(),
            r.get::<i64>(2).unwrap(),
            r.get::<String>(3).unwrap(),
        ));
    }
    drop(rows);
    assert_eq!(
        got,
        [
            ("INBOX".to_string(), r1, r2, "-".to_string()),
            ("INBOX".to_string(), r1, r2, "Archive".to_string()),
        ]
    );

    // Nothing changed: nothing new is recorded, and recording again is idempotent.
    let r3 = import(&store).await;
    let c = changes::between(&store, &[r2], &[r3], &[]).await.unwrap();
    assert_eq!((c.unchanged_files, c.removed.len()), (2, 0));
    let again = changes::between(&store, &[r1], &[r2], &[]).await.unwrap();
    store.begin().await.unwrap();
    assert_eq!(changes::record(&store, &again).await.unwrap(), 0);
    store.commit().await.unwrap();
}

#[test]
fn removals_are_recorded_per_import() {
    let dir = std::env::temp_dir().join(format!("munchymail-changes-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
