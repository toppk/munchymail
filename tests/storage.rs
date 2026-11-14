use std::fs;

use munchymail::api::{self, Ref};
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::store::Store;

async fn int(store: &Store, sql: &str) -> i64 {
    let mut rows = store.conn.query(sql, ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

#[test]
fn folder_data_is_kept_but_not_mail() {
    let dir = std::env::temp_dir().join(format!("munchymail-storage-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(async {
        let tree = dir.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(
            tree.join("INBOX"),
            "From MAILER_DAEMON  Mon Oct  6 09:00:00 2026\n\
             From: Mail System Internal Data <MAILER-DAEMON@x>\n\
             Subject: DON'T DELETE THIS MESSAGE -- FOLDER INTERNAL DATA\n\
             X-IMAP: 1216699624 0000000002\n\
             Status: RO\n\n\
             This text is part of the internal format of your mail folder.\n\n\
             From a@x  Mon Oct  6 09:00:00 2026\n\
             From: a@x\nMessage-ID: <real@x>\nSubject: real\nStatus: RO\nX-UID: 1\n\n\
             hello\n",
        )
        .unwrap();
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
        assert_eq!(int(&store, "SELECT count(*) FROM artifact").await, 2);
        assert_eq!(
            int(
                &store,
                "SELECT count(*) FROM artifact WHERE kind = 'folder-data'"
            )
            .await,
            1
        );

        // In no listing, filter, count or index.
        let (hits, _) = api::search(&store, &Filter::default(), false, 100, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        for (name, value) in [
            ("subject", "*FOLDER INTERNAL*"),
            ("from", "*Internal Data*"),
        ] {
            let f = Filter {
                headers: vec![(name.into(), value.into())],
                ..Filter::default()
            };
            assert!(
                api::search(&store, &f, false, 100, None)
                    .await
                    .unwrap()
                    .0
                    .is_empty()
            );
        }
        assert_eq!(
            int(&store, "SELECT count(*) FROM header WHERE artifact_id = 1").await,
            0
        );
        munchymail::parts::index(&store).await.unwrap();
        assert_eq!(
            int(&store, "SELECT count(DISTINCT msg_id) FROM part").await,
            1
        );
        assert_eq!(munchymail::parts::pending(&store).await.unwrap(), 0);

        // Addressed by id, it is readable and says what it is.
        let data = api::read(&store, Ref::Copy(1)).await.unwrap();
        assert_eq!(data.kind.as_deref(), Some("folder-data"));
        assert!(data.headers.iter().any(|f| f.name == "x-imap"));

        // `read` of mail leaves out the server's bookkeeping; the stored bytes keep it.
        let m = api::read(&store, Ref::Copy(hits[0].id)).await.unwrap();
        assert_eq!(m.kind, None);
        let names: Vec<String> = m.to_json()["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h[0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["from", "message-id", "subject"]);
        assert_eq!(
            int(&store, "SELECT count(*) FROM header WHERE name = 'x-uid'").await,
            1
        );
    });
    fs::remove_dir_all(&dir).unwrap();
}
