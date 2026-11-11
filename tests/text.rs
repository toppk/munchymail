use std::fs;
use std::sync::Arc;

use munchymail::api;
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::store::Store;
use munchymail::text::{self, TextQuery};

fn msg(n: u32, subject: &str, rest: &str) -> String {
    format!(
        "From x  Mon Oct  6 09:00:00 2026\nFrom: a@x\nSubject: {subject}\nMessage-ID: <{n}@x>\n\
Date: 0{n} Oct 2026 09:00:00 +0000\n{rest}\n"
    )
}

async fn hits(store: &Store, q: &str) -> Vec<api::Hit> {
    let f = Filter {
        text: Some(Arc::new(TextQuery::new(&store.data_dir, q).unwrap())),
        ..Filter::default()
    };
    api::search(store, &f, true, 100, None).await.unwrap().0
}

async fn scenario(dir: &std::path::Path) {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let inbox = [
        msg(1, "Monthly statement", "\nYour invoices are attached.\r\n"),
        msg(
            2,
            "Alternatives",
            "Content-Type: multipart/alternative; boundary=b\n\n\
--b\nContent-Type: text/plain\n\nhello plain\n\
--b\nContent-Type: text/html\n\n<p>hello <b>htmlonly</b></p>\n--b--",
        ),
        msg(
            3,
            "Files",
            "Content-Type: multipart/mixed; boundary=b\n\n\
--b\nContent-Type: text/plain\n\nsee attached\n\
--b\nContent-Type: text/plain; name=notes.txt\nContent-Disposition: attachment\n\nbuy kumquats\n\
--b\nContent-Type: message/rfc822\nContent-Disposition: attachment\n\n\
Subject: Forwarded zebra\n\nnested quokka\n--b--",
        ),
        msg(
            4,
            "Html only",
            "Content-Type: text/html\n\n<p>a <b>platypus</b></p>",
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
    assert!(TextQuery::new(&store.data_dir, "x").is_err());
    assert_eq!(text::pending(&store).await.unwrap(), 4);
    assert_eq!(text::index(&store).await.unwrap().messages, 4);
    assert_eq!(text::pending(&store).await.unwrap(), 0);
    assert_eq!(text::index(&store).await.unwrap().messages, 0);

    let subjects = |h: Vec<api::Hit>| h.into_iter().map(|h| h.subject).collect::<Vec<_>>();
    // Stemmed: invoice finds invoices; phrases and negation work.
    assert_eq!(
        subjects(hits(&store, "invoice").await),
        ["Monthly statement"]
    );
    assert_eq!(hits(&store, "\"invoices are\"").await.len(), 1);
    assert_eq!(hits(&store, "invoice -statement").await.len(), 0);
    // An HTML alternative to a plain body isn't indexed twice; an HTML-only body is.
    assert_eq!(hits(&store, "htmlonly").await.len(), 0);
    assert_eq!(subjects(hits(&store, "platypus").await), ["Html only"]);

    let h = hits(&store, "kumquat").await;
    let m = h[0].matched.as_ref().unwrap();
    assert_eq!(
        (m.source, m.filename.as_deref()),
        ("attachment", Some("notes.txt"))
    );
    assert_eq!(h[0].snippet, "buy kumquats");
    assert_eq!((m.highlighted.len(), m.highlighted[0].clone()), (1, 4..12));
    assert_eq!(hits(&store, "quokka zebra").await.len(), 1);
    let h = hits(&store, "subject:monthly").await;
    assert_eq!(h[0].matched.as_ref().unwrap().source, "subject");
    assert!(!h[0].snippet.contains('\r'));

    // A missing index directory is rebuilt from scratch.
    fs::remove_dir_all(text::dir(&store.data_dir)).unwrap();
    assert_eq!(text::index(&store).await.unwrap().messages, 4);
    assert_eq!(hits(&store, "invoice").await.len(), 1);
}

#[test]
fn text_index_and_search() {
    let dir = std::env::temp_dir().join(format!("munchymail-text-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
