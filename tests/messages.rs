use std::fs;

use munchymail::api::{self, Ref};
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::store::Store;

fn msg(headers: &str, body: &str) -> String {
    format!("From x  Mon Oct  6 09:00:00 2026\nFrom: a@x\n{headers}\n\n{body}\n")
}

const D1: &str = "Date: 01 Oct 2026 09:00:00 +0000";
const D2: &str = "Date: 02 Oct 2026 09:00:00 +0000";

async fn import(store: &Store) {
    Importer::new(store, "t", munchymail::mbox::Variant::Dovecot)
        .await
        .unwrap()
        .run()
        .await
        .unwrap();
}

/// Message id of each stored copy, in file order.
async fn msgs(store: &Store) -> Vec<i64> {
    let mut rows = store
        .conn
        .query(
            "SELECT a.msg_id FROM observation o JOIN artifact a ON a.artifact_id = o.artifact_id
             WHERE o.content_id = (SELECT max(content_id) FROM file_content) ORDER BY o.ordinal",
            (),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().await.unwrap() {
        out.push(r.get(0).unwrap());
    }
    out
}

async fn scenario(dir: &std::path::Path) {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let inbox = [
        // 0, 1: Status rewrite and SpamAssassin's subject tag: one message.
        msg(
            &format!("Message-ID: <a@x>\n{D1}\nSubject: hi\nTo: b@x"),
            "same",
        ),
        msg(
            &format!("Message-ID: <a@x>\n{D1}\nSubject: [SPAM] hi\nTo: \"b\" <b@x>\nStatus: RO"),
            "same",
        ),
        // 2: a reused Message-ID with another body.
        msg(&format!("Message-ID: <a@x>\n{D1}"), "other"),
        // 3: the same body under another Message-ID.
        msg(&format!("Message-ID: <b@x>\n{D1}"), "same"),
        // 4: same body and Message-ID, another Date (a reused ID).
        msg(&format!("Message-ID: <a@x>\n{D2}"), "same"),
        // 5, 6: no Message-ID: same body and Date fold, another Date doesn't.
        msg(D1, "daily reminder"),
        msg(&format!("{D1}\nStatus: RO"), "daily reminder"),
        msg(D2, "daily reminder"),
    ];
    fs::write(tree.join("INBOX"), inbox.join("\n")).unwrap();
    let store = Store::open(&dir.join("data")).await.unwrap();
    store
        .conn
        .execute(
            "INSERT INTO origin (label, root, created_at) VALUES ('t', ?1, 0)",
            (tree.to_str().unwrap(),),
        )
        .await
        .unwrap();
    import(&store).await;

    let m = msgs(&store).await;
    assert_eq!(m[0], m[1]);
    assert_eq!(m[5], m[6]);
    let mut distinct = vec![m[0], m[2], m[3], m[4], m[5], m[7]];
    distinct.dedup();
    assert_eq!(distinct.len(), 6, "{m:?}");
    let (hits, _) = api::search(&store, &Filter::default(), true, 100, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 6);

    // Dovecot rewrites Status on message 0 and 1: a new copy, the same message id.
    let first = api::read(&store, Ref::Message(m[0])).await.unwrap();
    assert_eq!(first.copies.len(), 2);
    let mut rewritten = inbox.clone();
    rewritten[0] = rewritten[0].replace("To: b@x", "To: b@x\nStatus: O");
    fs::write(tree.join("INBOX"), rewritten[..2].join("\n")).unwrap();
    import(&store).await;
    let now = msgs(&store).await;
    assert_eq!(now, [m[0], m[0]]);
    let r = api::read(&store, Ref::Message(m[0])).await.unwrap();
    assert_eq!(r.copies.len(), 3);
    assert!(r.locations.iter().any(|l| l.current));
    assert_eq!(r.decoded.text.trim(), "same");
    // The copy shown is the preferred one: the earliest stored, not the newest seen.
    assert_eq!(r.id, *r.copies.iter().min().unwrap());
    assert!(api::read(&store, Ref::Message(999_999)).await.is_err());

    // A version 3 database gets the same grouping when upgraded.
    for sql in [
        "DROP INDEX artifact_msg",
        "ALTER TABLE artifact DROP COLUMN msg_id",
        "DELETE FROM message",
        "UPDATE meta SET value = '3' WHERE key = 'schema_version'",
    ] {
        store.conn.execute(sql, ()).await.unwrap();
    }
    drop(store);
    let store = Store::open(&dir.join("data")).await.unwrap();
    let (hits, _) = api::search(&store, &Filter::default(), true, 100, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 6);
    // Regrouped under the current key rule, still the same six.
    munchymail::rekey::regroup(&store).await.unwrap();
    let (hits, _) = api::search(&store, &Filter::default(), true, 100, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 6);

    assert_eq!("m12".parse(), Ok(Ref::Message(12)));
    assert_eq!("12".parse(), Ok(Ref::Copy(12)));
    assert!("x12".parse::<Ref>().is_err());
}

#[test]
fn message_ids_group_copies_and_survive_rewrites() {
    let dir = std::env::temp_dir().join(format!("munchymail-messages-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
