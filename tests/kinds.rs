use std::fs;

use munchymail::api::{self, Ref};
use munchymail::import::Importer;
use munchymail::kinds;
use munchymail::mbox::Variant;
use munchymail::query::Filter;
use munchymail::store::Store;

const FROM: &str = "From x  Mon Oct  6 09:00:00 2026\n";
const D: &str = "Date: 01 Oct 2026 09:00:00 +0000";

async fn int(store: &Store, sql: &str) -> i64 {
    let mut rows = store.conn.query(sql, ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

async fn found(store: &Store, subject: &str) -> Vec<i64> {
    let f = Filter {
        headers: vec![("subject".into(), subject.into())],
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
fn damaged_copies_leave_every_listing_and_come_back() {
    let dir = std::env::temp_dir().join(format!("munchymail-kinds-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    pollster::block_on(async {
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        // 1 and 2: one message (same Message-ID and body), the first damaged
        // later; 3: its repair; 4: Dovecot's placeholder.
        let one = format!("{FROM}Message-ID: <a@x>\n{D}\nSubject: alpha\nStatus: RO\n\nbody a\n");
        let two = format!("{FROM}Message-ID: <a@x>\n{D}\nSubject: alpha\n\nbody a\n");
        let three = format!("{FROM}Message-ID: <b@x>\n{D}\nSubject: beta\n\nfixed\n");
        let four = format!(
            "{FROM}{D}\nSubject: {}\nX-IMAP: 1 2\n\nplaceholder\n",
            munchymail::headers::FOLDER_DATA_SUBJECT
        );
        fs::write(src.join("INBOX"), [one, two, three, four].join("\n")).unwrap();
        let store = Store::open(&dir.join("data")).await.unwrap();
        store
            .conn
            .execute(
                "INSERT INTO origin (label, root, created_at) VALUES ('a', ?1, 0)",
                (src.to_str().unwrap(),),
            )
            .await
            .unwrap();
        Importer::new(&store, "a", Variant::Dovecot)
            .await
            .unwrap()
            .run()
            .await
            .unwrap();
        munchymail::parts::index(&store).await.unwrap();
        let msg = api::read(&store, Ref::Copy(1)).await.unwrap().msg;
        assert_eq!(api::read(&store, Ref::Message(msg)).await.unwrap().id, 1);
        let headers = int(&store, "SELECT count(*) FROM header WHERE artifact_id = 1").await;
        assert!(headers > 0);

        // Copy 1 damaged: the message is shown and found by its other copy.
        let changed = kinds::set(
            &store,
            &[1],
            Some(kinds::DAMAGED),
            "cut short",
            kinds::OWNER,
        )
        .await
        .unwrap();
        assert_eq!(changed, [1]);
        let m = api::read(&store, Ref::Message(msg)).await.unwrap();
        assert_eq!((m.id, m.kind.as_deref()), (2, None));
        assert_eq!(m.kinds.len(), 1);
        assert_eq!(
            (m.kinds[0].copy, m.kinds[0].reason.as_str()),
            (1, "cut short")
        );
        assert_eq!(found(&store, "alpha").await, [2]);
        assert_eq!(
            int(&store, "SELECT count(*) FROM header WHERE artifact_id = 1").await,
            0
        );
        assert_eq!(
            int(
                &store,
                &format!(
                    "SELECT count(*) FROM message WHERE parts_version IS NULL AND msg_id = {msg}"
                )
            )
            .await,
            1
        );
        // Setting it again changes nothing.
        let again = kinds::set(&store, &[1], Some(kinds::DAMAGED), "again", kinds::OWNER).await;
        assert!(again.unwrap().is_empty());

        // Both copies damaged: the message leaves search, readable by id.
        kinds::set(
            &store,
            &[2],
            Some(kinds::DAMAGED),
            "cut short",
            kinds::OWNER,
        )
        .await
        .unwrap();
        assert!(found(&store, "alpha").await.is_empty());
        let m = api::read(&store, Ref::Message(msg)).await.unwrap();
        assert_eq!(m.kind.as_deref(), Some(kinds::DAMAGED));
        assert_eq!(
            int(
                &store,
                &format!("SELECT count(*) FROM text_forget WHERE msg_id = {msg}")
            )
            .await,
            1
        );

        // The repair links both ways.
        assert_eq!(
            kinds::link(
                &store,
                3,
                kinds::REPAIRED_FROM,
                &[1, 2],
                Some("rebuilt"),
                false
            )
            .await
            .unwrap(),
            2
        );
        let fixed = api::read(&store, Ref::Copy(3)).await.unwrap();
        assert_eq!(fixed.links.len(), 2);
        assert!(fixed.links.iter().all(|l| !l.reverse && l.other_msg == msg));
        let old = api::read(&store, Ref::Copy(1)).await.unwrap();
        assert!(
            old.links
                .iter()
                .any(|l| l.reverse && l.other == 3 && l.copy == 1)
        );

        // munchymail's own classification isn't changed by hand.
        assert_eq!(
            int(
                &store,
                "SELECT count(*) FROM artifact WHERE kind = 'folder-data'"
            )
            .await,
            1
        );
        let placeholder = int(
            &store,
            "SELECT artifact_id FROM artifact WHERE kind = 'folder-data'",
        )
        .await;
        assert!(
            kinds::set(&store, &[placeholder], None, "no", kinds::OWNER)
                .await
                .is_err()
        );

        // Undamaged: found again, headers back, history kept.
        kinds::set(&store, &[1, 2], None, "was fine", kinds::OWNER)
            .await
            .unwrap();
        assert_eq!(found(&store, "alpha").await, [1, 2]);
        assert_eq!(
            int(&store, "SELECT count(*) FROM header WHERE artifact_id = 1").await,
            headers
        );
        assert_eq!(
            int(&store, "SELECT count(*) FROM copy_kind_change").await,
            4
        );
        assert!(
            api::read(&store, Ref::Copy(1))
                .await
                .unwrap()
                .kinds
                .is_empty()
        );
    });
    fs::remove_dir_all(&dir).unwrap();
}
