use std::fs;

use munchymail::api::{self, Ref};
use munchymail::import::Importer;
use munchymail::query::Filter;
use munchymail::store::Store;

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
             X-IMAPbase: 1216699624 0000000002\n\
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
        let mut rows = store
            .conn
            .query(
                "SELECT (SELECT count(*) FROM artifact), (SELECT count(*) FROM storage_record)",
                (),
            )
            .await
            .unwrap();
        let r = rows.next().await.unwrap().unwrap();
        assert_eq!((r.get::<i64>(0).unwrap(), r.get::<i64>(1).unwrap()), (2, 1));
        drop(rows);
        let (hits, _) = api::search(&store, &Filter::default(), false, 100, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        let f = Filter {
            headers: vec![("subject".into(), "*FOLDER INTERNAL*".into())],
            ..Filter::default()
        };
        assert!(
            api::search(&store, &f, false, 100, None)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        // `read` leaves out the server's bookkeeping; the stored bytes keep it.
        let m = api::read(&store, Ref::Copy(hits[0].id)).await.unwrap();
        let names: Vec<String> = m.to_json()["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h[0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["from", "message-id", "subject"]);
        let mut rows = store
            .conn
            .query("SELECT count(*) FROM header WHERE name = 'x-uid'", ())
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            1
        );
    });
    fs::remove_dir_all(&dir).unwrap();
}
