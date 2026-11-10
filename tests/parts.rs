use std::fs;
use std::io::{Cursor, Write};

use munchymail::import::Importer;
use munchymail::parts::{self, Key};
use munchymail::query::Filter;
use munchymail::store::Store;

fn zip() -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("Invoice.js", opts).unwrap();
    z.write_all(b"x").unwrap();
    z.finish().unwrap().into_inner()
}

fn msg(n: u32, part: &str) -> String {
    format!(
        "From x  Mon Oct  6 09:00:00 2026\nFrom: a@x\nMessage-ID: <{n}@x>\n\
Date: 0{n} Oct 2026 09:00:00 +0000\nContent-Type: multipart/mixed; boundary=b\n\n\
--b\nContent-Type: text/plain\n\nhi\n--b\n{part}\n--b--\n"
    )
}

async fn count(store: &Store, f: Filter) -> i64 {
    let set = f.materialize(&store.conn).await.unwrap();
    let mut rows = store
        .conn
        .query(&format!("SELECT count(*) FROM {set}"), ())
        .await
        .unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

async fn scenario(dir: &std::path::Path) {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let b64 = |b: &[u8]| data_encoding::BASE64.encode(b);
    let inbox = [
        msg(
            1,
            &format!(
                "Content-Type: application/octet-stream; name=scan.bin\nContent-Transfer-Encoding: base64\n\n{}",
                b64(b"%PDF-1.4 x")
            ),
        ),
        msg(
            2,
            &format!(
                "Content-Type: application/zip; name=a.zip\nContent-Transfer-Encoding: base64\n\n{}",
                b64(&zip())
            ),
        ),
        msg(3, "Content-Type: text/html\n\n<p>alternative</p>"),
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
    assert_eq!(parts::pending(&store).await.unwrap(), 3);
    let s = parts::index(&store).await.unwrap();
    assert_eq!((s.messages, s.parts, s.problems), (3, 6, 0));
    assert_eq!(parts::pending(&store).await.unwrap(), 0);
    assert_eq!(parts::index(&store).await.unwrap().messages, 0);

    let by = |t: &[&str]| Filter {
        attachments: t.iter().map(|s| s.to_string()).collect(),
        ..Filter::default()
    };
    // Found by content although declared octet-stream; body parts never match.
    assert_eq!(count(&store, by(&["pdf"])).await, 1);
    assert_eq!(count(&store, by(&["application/pdf"])).await, 1);
    assert_eq!(count(&store, by(&["zip"])).await, 1);
    assert_eq!(count(&store, by(&["html"])).await, 0);
    let named = |p: &str| Filter {
        attachment_names: vec![p.to_string()],
        ..Filter::default()
    };
    assert_eq!(count(&store, named("*.js")).await, 1);
    assert_eq!(count(&store, named("scan.*")).await, 1);

    let g = parts::report(&store, "artifact", Key::Mismatch, false)
        .await
        .unwrap();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].key, "application/octet-stream -> application/pdf");
    let all = parts::report(&store, "artifact", Key::Type, true)
        .await
        .unwrap();
    assert_eq!(all[0].key, "text/plain");
    assert_eq!(all[0].messages, 3);
}

#[test]
fn parts_index_types_and_zip_names() {
    let dir = std::env::temp_dir().join(format!("munchymail-parts-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
