use std::fs;

use munchymail::api::{self, Ref};
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::store::Store;

fn msg(id: &str, day: u32, extra: &str, body: &str) -> String {
    format!(
        "From x  Mon Oct  6 09:00:00 2026\nFrom: a@x\nMessage-ID: <{id}@x>\nDate: {day:02} Oct 2026 09:00:00 +0000\n{extra}\n{body}\n"
    )
}

async fn scenario(dir: &std::path::Path) {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let inbox = [
        msg("a", 1, "Subject: one\n", "first"),
        msg("b", 2, "Subject: two \x1b[31mred\n", "second"),
        msg(
            "c",
            2,
            "Content-Type: text/html\n",
            "<p>third &amp; last</p>",
        ),
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
    Importer::new(&store, "t")
        .await
        .unwrap()
        .run()
        .await
        .unwrap();

    // Two pages cover everything once, newest first, same-date ties by id.
    let f = Filter::default();
    let (p1, next) = api::search(&store, &f, false, 2, None).await.unwrap();
    let (p2, end) = api::search(&store, &f, false, 2, next.as_deref())
        .await
        .unwrap();
    assert!(end.is_none());
    let order: Vec<String> = p1.iter().chain(&p2).map(|h| h.snippet.clone()).collect();
    assert_eq!(order, ["third & last", "second", "first"]);
    let j = p1[1].to_json().to_string();
    assert!(
        !j.contains('\u{1b}') && j.contains("two \u{FFFD}[31mred"),
        "{j}"
    );
    assert!(api::search(&store, &f, false, 0, None).await.is_err());

    // "a" leaves INBOX: still readable, its location no longer current.
    let a = p2[0].id;
    fs::write(tree.join("INBOX"), inbox[1..].join("\n")).unwrap();
    Importer::new(&store, "t")
        .await
        .unwrap()
        .run()
        .await
        .unwrap();
    let m = api::read(&store, Ref::Copy(a)).await.unwrap();
    assert_eq!(m.decoded.text.trim(), "first");
    assert_eq!(m.locations.len(), 1);
    assert!(!m.locations[0].current);
    assert!(m.locations[0].origin_last_import >= Some(m.locations[0].last_seen));
    let b = api::read(&store, Ref::Copy(p1[1].id)).await.unwrap();
    assert!(b.locations[0].current);
    assert!(api::read(&store, Ref::Copy(999_999)).await.is_err());
}

#[test]
fn search_pages_and_read_tracks_locations() {
    let dir = std::env::temp_dir().join(format!("munchymail-read-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
