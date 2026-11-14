use std::fs;

use munchymail::api::{self, Ref};
use munchymail::import::Importer;
use munchymail::mbox::Variant;
use munchymail::query::Filter;
use munchymail::store::Store;

const FROM: &str = "From x  Mon Oct  6 09:00:00 2026\n";
const D: &str = "Date: 01 Oct 2026 09:00:00 +0000";

async fn int(store: &Store, sql: &str) -> i64 {
    let mut rows = store.conn.query(sql, ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

async fn origin(store: &Store, label: &str, root: &std::path::Path, variant: Variant) {
    store
        .conn
        .execute(
            "INSERT INTO origin (label, root, created_at) VALUES (?1, ?2, 0)",
            (label, root.to_str().unwrap()),
        )
        .await
        .unwrap();
    Importer::new(store, label, variant)
        .await
        .unwrap()
        .run()
        .await
        .unwrap();
}

async fn marked(store: &Store, mark: &str) -> Vec<i64> {
    let f = Filter {
        marks: vec![mark.into()],
        ..Filter::default()
    };
    let mut ids: Vec<i64> = api::search(store, &f, false, 100, None)
        .await
        .unwrap()
        .0
        .iter()
        .map(|h| h.id)
        .collect();
    ids.sort();
    ids
}

#[test]
fn copies_are_marked_and_files_rebuild() {
    let dir = std::env::temp_dir().join(format!("munchymail-marks-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(async {
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        // Origin a (Dovecot): 1 a body longer than its Content-Length, 2 a
        // multipart never closed, 3 a complete message.
        let one = format!("{FROM}Message-ID: <cl@x>\n{D}\nContent-Length: 3\n\nhello\n");
        let two = format!(
            "{FROM}Message-ID: <mp@x>\n{D}\nContent-Type: multipart/mixed; boundary=\"Zz\"\n\n--Zz\n\npart\n"
        );
        let three = format!("{FROM}Message-ID: <p@x>\n{D}\n\nline one\nline two\n");
        fs::write(a.join("INBOX"), [one, two, three].join("\n")).unwrap();
        // Origin b (mboxrd): 4 the same message cut short, and 5 a body with a
        // quoted From line, so the file is rebuilt with quoting.
        let four = format!("{FROM}Message-ID: <p@x>\n{D}\n\nline one\nli");
        let five = format!("{FROM}Message-ID: <q@x>\n{D}\n\n>From the start\n");
        fs::write(b.join("INBOX"), [four, five].join("\n")).unwrap();
        let store = Store::open(&dir.join("data")).await.unwrap();
        origin(&store, "a", &a, Variant::Dovecot).await;
        origin(&store, "b", &b, Variant::Mboxrd).await;

        assert_eq!(marked(&store, "content-length").await, [1]);
        assert_eq!(marked(&store, "unclosed-boundary").await, [2]);
        assert_eq!(marked(&store, "prefix-of").await, [4]);
        assert_eq!(marked(&store, "*").await, [1, 2, 4]);
        let m = api::read(&store, Ref::Copy(4)).await.unwrap();
        assert_eq!(m.marks.len(), 1);
        assert!(
            m.marks[0]
                .detail
                .starts_with("copy 3: this body is its first 11 of 18 bytes")
        );
        assert!(m.marks[0].detail.ends_with("this body stops mid-line"));
        assert!(
            api::read(&store, Ref::Copy(1)).await.unwrap().marks[0]
                .detail
                .contains("the body is longer")
        );

        // The backfill finds the same.
        let before = int(&store, "SELECT count(*) FROM copy_mark").await;
        store
            .conn
            .execute("DELETE FROM copy_mark", ())
            .await
            .unwrap();
        munchymail::marks::backfill(&store).await.unwrap().unwrap();
        assert_eq!(int(&store, "SELECT count(*) FROM copy_mark").await, before);
        assert!(munchymail::marks::backfill(&store).await.unwrap().is_none());

        // Both files rebuild byte for byte, mboxrd quoting included.
        for label in ["a", "b"] {
            let r = munchymail::verify::origin(&store, label).await.unwrap();
            assert_eq!(
                (r.files, r.failures.len()),
                (1, 0),
                "{label}: {:?}",
                r.failures
            );
        }
        // A framing change is caught.
        store
            .conn
            .execute(
                "UPDATE observation SET separator = x'' WHERE artifact_id = 1",
                (),
            )
            .await
            .unwrap();
        let r = munchymail::verify::origin(&store, "a").await.unwrap();
        assert_eq!(r.failures.len(), 1);
    });
    fs::remove_dir_all(&dir).unwrap();
}
