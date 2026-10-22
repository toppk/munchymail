use std::fs;
use std::io::Read;

use flate2::read::GzDecoder;
use munchymail::export;
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::store::Store;

fn msg(id: &str, from: &str, to: &str, extra: &str) -> String {
    format!(
        "From x  Mon Oct  6 09:00:00 2026\nFrom: {from}\nTo: {to}\nMessage-ID: <{id}@x>\n{extra}\nbody {id}\n"
    )
}

async fn scenario(dir: &std::path::Path) {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let inbox = [
        msg("a", "Ann <ann@corp.example>", "me@home", ""),
        msg("b", "spam@other.example", "me@home", ""),
    ];
    // The same reply in two folders, one copy with a Status header added.
    let sent = [
        msg("c", "me@home", "Bob <bob@corp.example>", ""),
        msg("c", "me@home", "Bob <bob@corp.example>", "Status: RO\n"),
    ];
    fs::write(tree.join("INBOX"), inbox.join("\n")).unwrap();
    fs::write(tree.join("Sent"), sent.join("\n")).unwrap();
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

    let filter = Filter {
        participants: vec!["*@corp.example".into()],
        ..Filter::default()
    };
    let out = dir.join("corp.tgz");
    store.begin().await.unwrap();
    let s = export::write(
        &store,
        &filter,
        "corp",
        "Corp",
        "munchymail export corp",
        &out,
    )
    .await
    .unwrap();
    store.commit().await.unwrap();
    assert_eq!((s.messages, s.folded), (2, 1));

    let mut tar = tar::Archive::new(GzDecoder::new(fs::File::open(&out).unwrap()));
    let mut files = Vec::new();
    for e in tar.entries().unwrap() {
        let mut e = e.unwrap();
        let mut body = String::new();
        e.read_to_string(&mut body).unwrap();
        files.push((e.path().unwrap().display().to_string(), body));
    }
    let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        names,
        [
            "corp/",
            "corp/maildir/",
            "corp/maildir/001",
            "corp/maildir/002",
            "corp/index.md"
        ]
    );
    assert!(files[2].1.starts_with("From: Ann"), "no From_ line");
    assert!(files[4].1.contains("| [2](maildir/002) |"));
    assert!(files[4].1.contains("| Sent |"));
}

#[test]
fn export_folds_header_only_copies() {
    let dir = std::env::temp_dir().join(format!("munchymail-export-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
