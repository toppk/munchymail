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
    Importer::new(&store, "t", munchymail::mbox::Variant::Dovecot)
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
        export::Newlines::Keep,
        &mut std::fs::File::create(&out).unwrap(),
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
    assert!(files[4].1.contains("byte for byte as stored"));
}

fn entries(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut tar = tar::Archive::new(GzDecoder::new(fs::File::open(path).unwrap()));
    let mut out = Vec::new();
    for e in tar.entries().unwrap() {
        let mut e = e.unwrap();
        let mut body = Vec::new();
        e.read_to_end(&mut body).unwrap();
        out.push((e.path().unwrap().display().to_string(), body));
    }
    out
}

async fn crlf_scenario(dir: &std::path::Path) {
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let stored =
        b"From: Ann <ann@corp.example>\r\nMessage-ID: <crlf@x>\r\n\r\nline one\r\nline two\r\n";
    let mut mbox = b"From x  Mon Oct  6 09:00:00 2026\n".to_vec();
    mbox.extend_from_slice(stored);
    mbox.push(b'\n');
    fs::write(tree.join("INBOX"), &mbox).unwrap();
    let store = Store::open(&dir.join("data")).await.unwrap();
    store
        .conn
        .execute(
            "INSERT INTO origin (label, root, created_at) VALUES ('t', ?1, 0)",
            (tree.to_str().unwrap(),),
        )
        .await
        .unwrap();
    Importer::new(&store, "t", munchymail::mbox::Variant::Dovecot)
        .await
        .unwrap()
        .run()
        .await
        .unwrap();
    let mut rows = store
        .conn
        .query("SELECT header_hash, body_hash FROM artifact", ())
        .await
        .unwrap();
    let r = rows.next().await.unwrap().unwrap();
    let (h, b): (Vec<u8>, Vec<u8>) = (r.get(0).unwrap(), r.get(1).unwrap());
    drop(rows);
    let mut copy = store.object(&h).await.unwrap();
    copy.extend(store.object(&b).await.unwrap());
    assert_eq!(copy, stored, "the import keeps CRLF");

    let filter = Filter {
        participants: vec!["*@corp.example".into()],
        ..Filter::default()
    };
    for (name, nl) in [
        ("keep", export::Newlines::Keep),
        ("lf", export::Newlines::Lf),
    ] {
        let out = dir.join(format!("{name}.tgz"));
        store.begin().await.unwrap();
        export::write(
            &store,
            &filter,
            name,
            name,
            "t",
            nl,
            &mut fs::File::create(&out).unwrap(),
        )
        .await
        .unwrap();
        store.commit().await.unwrap();
        let files = entries(&out);
        let (msg, index) = (&files[2].1, String::from_utf8_lossy(&files[3].1));
        if nl == export::Newlines::Keep {
            assert_eq!(msg, &copy, "keep exports the stored bytes");
            assert!(index.contains("byte for byte as stored"));
        } else {
            let lf = String::from_utf8(copy.clone())
                .unwrap()
                .replace("\r\n", "\n");
            assert_eq!(msg, lf.as_bytes());
            assert!(!index.contains("byte for byte") && index.contains("line endings are LF"));
        }
    }
}

#[test]
fn export_folds_header_only_copies() {
    let dir = std::env::temp_dir().join(format!("munchymail-export-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn export_keeps_stored_bytes_by_default() {
    let dir = std::env::temp_dir().join(format!("munchymail-export-crlf-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(crlf_scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
