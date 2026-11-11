use std::fs;
use std::sync::Arc;

use munchymail::api;
use munchymail::extract;
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::store::Store;
use munchymail::text::{self, TextQuery};

const PDF: &[u8] = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 100]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n\
4 0 obj<</Length 41>>stream\nBT /F1 12 Tf 10 50 Td (wombat pdf) Tj ET\nendstream endobj\n\
5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n";

fn docx() -> Vec<u8> {
    use std::io::Write;
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    z.start_file(
        "word/document.xml",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    z.write_all(b"<w:document><w:p><w:r><w:t>numbat docx</w:t></w:r></w:p></w:document>")
        .unwrap();
    z.finish().unwrap().into_inner()
}

/// Whether the sandboxed tools run here (bwrap needs user namespaces).
fn sandbox_works() -> bool {
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/lib64",
            "/lib64",
            "--unshare-all",
            "/usr/bin/pdftotext",
            "-v",
        ])
        .output()
        .is_ok_and(|o| o.status.success())
}

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
        msg(
            5,
            "Documents",
            &format!(
                "Content-Type: multipart/mixed; boundary=b\n\n--b\nContent-Type: text/plain\n\nfiles\n\
--b\nContent-Type: application/octet-stream; name=a.pdf\nContent-Transfer-Encoding: base64\n\n{}\n\
--b\nContent-Type: application/octet-stream; name=b.docx\nContent-Transfer-Encoding: base64\n\n{}\n--b--",
                data_encoding::BASE64.encode(PDF),
                data_encoding::BASE64.encode(&docx())
            ),
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
    assert_eq!(text::pending(&store).await.unwrap(), 5);
    munchymail::parts::index(&store).await.unwrap();
    let e = extract::run(&store).await.unwrap();
    assert_eq!(e.documents, 2);
    assert_eq!(text::index(&store).await.unwrap().messages, 5);
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

    // Documents: docx in-process, PDF only where the sandbox runs.
    let h = hits(&store, "numbat").await;
    let m = h[0].matched.as_ref().unwrap();
    assert_eq!(
        (m.source, m.filename.as_deref()),
        ("attachment", Some("b.docx"))
    );
    if sandbox_works() {
        assert_eq!(hits(&store, "wombat").await.len(), 1);
    } else {
        eprintln!("bwrap or pdftotext unavailable; PDF extraction not checked");
    }
    assert_eq!(extract::run(&store).await.unwrap().documents, 0);

    // A missing index directory is rebuilt from scratch.
    fs::remove_dir_all(text::dir(&store.data_dir)).unwrap();
    assert_eq!(text::index(&store).await.unwrap().messages, 5);
    assert_eq!(hits(&store, "invoice").await.len(), 1);
}

#[test]
fn text_index_and_search() {
    let dir = std::env::temp_dir().join(format!("munchymail-text-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(scenario(&dir));
    fs::remove_dir_all(&dir).unwrap();
}
