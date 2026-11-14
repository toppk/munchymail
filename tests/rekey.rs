use std::fs;
use std::sync::Arc;

use munchymail::api::{self, Ref};
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::rekey;
use munchymail::store::Store;
use munchymail::text::{self, TextQuery};

const FROM: &str = "From x  Mon Oct  6 09:00:00 2026\n";

async fn store_with(dir: &std::path::Path, messages: &[&str]) -> Store {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let mbox: Vec<String> = messages.iter().map(|m| format!("{FROM}{m}")).collect();
    fs::write(tree.join("INBOX"), mbox.join("\n")).unwrap();
    let store = Store::open(&dir.join("data")).await.unwrap();
    store
        .conn
        .execute(
            "INSERT INTO origin (label, root, created_at) VALUES ('t', ?1, 0)",
            (tree.to_str().unwrap(),),
        )
        .await
        .unwrap();
    rekey::rekey(&store).await.unwrap();
    import(&store).await;
    store
}

async fn import(store: &Store) {
    Importer::new(store, "t", munchymail::mbox::Variant::Dovecot)
        .await
        .unwrap()
        .run()
        .await
        .unwrap();
}

async fn ints(store: &Store, sql: &str) -> Vec<Vec<Option<i64>>> {
    let mut rows = store.conn.query(sql, ()).await.unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().await.unwrap() {
        out.push(
            (0..r.column_count())
                .map(|i| r.get::<Option<i64>>(i).unwrap())
                .collect(),
        );
    }
    out
}

/// Which message each copy is in, by copy id.
async fn groups(store: &Store) -> Vec<i64> {
    ints(store, "SELECT msg_id FROM artifact ORDER BY artifact_id")
        .await
        .into_iter()
        .map(|r| r[0].unwrap())
        .collect()
}

/// Everything regrouping may change.
async fn state(store: &Store) -> Vec<Vec<Option<i64>>> {
    let mut s = ints(store, "SELECT artifact_id, msg_id FROM artifact ORDER BY 1").await;
    s.extend(
        ints(
            store,
            "SELECT msg_id, merged_into, split_from, key IS NULL, parts_version FROM message ORDER BY 1",
        )
        .await,
    );
    s
}

/// Put copy `a` in message `msg`, as an older rule might have.
async fn put_in(store: &Store, a: i64, msg: i64) {
    store
        .conn
        .execute(
            "UPDATE artifact SET msg_id = ?2 WHERE artifact_id = ?1",
            (a, msg),
        )
        .await
        .unwrap();
}

async fn active(store: &Store) -> i64 {
    ints(
        store,
        "SELECT count(*) FROM message WHERE merged_into IS NULL",
    )
    .await[0][0]
        .unwrap()
}

async fn text_hits(store: &Store, q: &str) -> usize {
    let f = Filter {
        text: Some(Arc::new(TextQuery::new(&store.data_dir, q).unwrap())),
        ..Filter::default()
    };
    api::search(store, &f, true, 100, None)
        .await
        .unwrap()
        .0
        .len()
}

fn run(name: &str, f: impl AsyncFnOnce(&std::path::Path)) {
    let dir = std::env::temp_dir().join(format!("munchymail-rekey-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(f(&dir));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn import_keeps_copies_without_evidence_apart() {
    run("import", async |dir| {
        let store = store_with(
            dir,
            &[
                // Four deliveries of one template: no From, Message-ID or Date.
                "Received: from one\nSubject: [SPAM]\n\n{{BODY_TEXT}}\n",
                "Received: from two\nSubject: [SPAM]\n\n{{BODY_TEXT}}\n",
                // No separator, no evidence fields: the whole header decides.
                "X-Note: one\n",
                "X-Note: two\n",
                // Different invalid Message-IDs, nothing else.
                "Message-ID: unknown-1\n\nsame\n",
                "Message-ID: unknown-2\n\nsame\n",
                // Same bytes, read differently.
                "Message-ID: <b@x>\n\naGVsbG8=\n",
                "Message-ID: <b@x>\nContent-Transfer-Encoding: base64\n\naGVsbG8=\n",
                // The same delivery seen twice, once with Dovecot's fields.
                "Received: from three\n\nhello\n",
                "Received: from three\nStatus: RO\nX-UID: 7\n\nhello\n",
            ],
        )
        .await;
        let g = groups(&store).await;
        assert_eq!(g.len(), 10);
        let distinct: std::collections::BTreeSet<_> = g[..8].iter().collect();
        assert_eq!(distinct.len(), 8, "{g:?}");
        assert_eq!(g[8], g[9]);
    });
}

#[test]
fn split_merge_and_fold_in_one_regrouping() {
    run("split", async |dir| {
        let store = store_with(
            dir,
            &[
                // 1, 2: one email, from two places.
                "Message-ID: <a@x>\nSubject: alpha\n\nbody a\n",
                "Message-ID: <a@x>\nSubject: alpha\nStatus: RO\n\nbody a\n",
                // 3, 4: two deliveries with no Message-ID.
                "Received: one\nSubject: apples\n\nsame\n",
                "Received: two\nSubject: oranges\n\nsame\n",
                // 5: a lesser copy of 4, folded to it.
                "Received: two\nSubject: oranges\nX-Folded: yes\n\nsame, damaged\n",
            ],
        )
        .await;
        let g = groups(&store).await;
        assert_eq!(g[0], g[1]);
        let (m_a, m_3, m_4, m_5) = (g[0], g[2], g[3], g[4]);
        assert!(m_3 != m_4 && m_4 != m_5);

        // As an older, looser rule left it: 3, 4 and 5 in m_3 (5 folded to 4),
        // and 2 split off alone into m_4, whose id is now the higher one.
        store
            .conn
            .execute(
                "INSERT INTO fold (artifact_id, copy_of, reason) VALUES (5, 4, 'test')",
                (),
            )
            .await
            .unwrap();
        for a in [3, 4, 5] {
            put_in(&store, a, m_3).await;
        }
        put_in(&store, 2, m_4).await;
        for m in [m_5] {
            store
                .conn
                .execute("DELETE FROM message WHERE msg_id = ?1", (m,))
                .await
                .unwrap();
        }
        munchymail::parts::index(&store).await.unwrap();
        text::index(&store).await.unwrap();
        assert_eq!(
            text_hits(&store, "oranges").await,
            0,
            "only m_3's preferred copy is indexed"
        );

        // Interrupted: nothing changes.
        let before = state(&store).await;
        store.begin().await.unwrap();
        rekey::regroup_in(&store).await.unwrap();
        store.rollback().await.unwrap();
        assert_eq!(state(&store).await, before);

        let s = rekey::regroup(&store).await.unwrap();
        let g = groups(&store).await;
        // 1 and 2 are one message again: m_a survives (lower), m_4 is merged into it.
        assert_eq!((g[0], g[1]), (m_a, m_a));
        assert_eq!(s.merged, vec![(m_4, m_a)]);
        // 3 keeps m_3 (its preferred copy); 4 splits off, and 5 follows its fold.
        assert_eq!(g[2], m_3);
        assert_eq!(g[3], g[4]);
        let new = g[3];
        assert!(new > m_4, "a new id, not a reused one");
        assert_eq!(s.split, vec![(new, m_3)]);
        assert_eq!(
            ints(
                &store,
                &format!("SELECT split_from FROM message WHERE msg_id = {new}")
            )
            .await,
            [[Some(m_3)]]
        );

        // The merged id resolves, and says so.
        let r = api::read(&store, Ref::Message(m_4)).await.unwrap();
        assert_eq!((r.msg, r.merged_from), (m_a, Some(m_4)));
        assert_eq!(api::copy(&store, Ref::Message(m_4)).await.unwrap(), 1);

        // Both deliveries are searchable, by what only one of them says.
        munchymail::parts::index(&store).await.unwrap();
        text::index(&store).await.unwrap();
        assert_eq!(text_hits(&store, "oranges").await, 1);
        assert_eq!(text_hits(&store, "apples").await, 1);

        // Again: nothing to do.
        let after = state(&store).await;
        assert!(!rekey::regroup(&store).await.unwrap().changed());
        assert_eq!(state(&store).await, after);

        // A merged id is never handed out again.
        let tree = dir.join("tree");
        let mut mbox = fs::read_to_string(tree.join("INBOX")).unwrap();
        mbox.push_str(&format!("\n{FROM}Message-ID: <z@x>\n\nnew\n"));
        fs::write(tree.join("INBOX"), mbox).unwrap();
        import(&store).await;
        let newest = *groups(&store).await.last().unwrap();
        assert!(newest > new && newest != m_4);
        assert_eq!(active(&store).await, 4);
    });
}
