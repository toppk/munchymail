//! Compare two snapshots of an mbox message by message, without the database.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::headers::{self, Field};
use crate::{mbox, source};

const LOCK_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Msg {
    pub header_hash: [u8; 32],
    pub body_hash: [u8; 32],
    pub message_id: Option<String>,
    pub fields: Vec<Field>,
}

impl Msg {
    /// Every value of `name`, in order, for report keys.
    pub fn values(&self, name: &str) -> Option<Vec<String>> {
        let v: Vec<String> = self
            .fields
            .iter()
            .filter(|f| f.name == name)
            .map(|f| f.value.clone())
            .collect();
        (!v.is_empty()).then_some(v)
    }

    pub fn same_message_key(&self) -> ([u8; 32], Option<String>) {
        (self.body_hash, self.message_id.clone())
    }
}

/// Read under the shared lock, split, and keep only hashes and decoded headers.
pub fn load(path: &Path, variant: mbox::Variant) -> Result<Vec<Msg>> {
    let snap = source::read_locked(path, LOCK_TIMEOUT)?;
    let buf = &snap.bytes;
    let parsed = mbox::split(buf, variant).with_context(|| path.display().to_string())?;
    parsed
        .messages
        .par_iter()
        .map(|m| {
            let (header, body) = parsed
                .content(buf, m)
                .with_context(|| path.display().to_string())?;
            let fields = headers::fields(&header);
            Ok(Msg {
                header_hash: Sha256::digest(&header).into(),
                body_hash: Sha256::digest(&body).into(),
                message_id: headers::first(&fields, "message-id").map(str::to_string),
                fields,
            })
        })
        .collect()
}

pub struct Diff {
    pub unchanged: usize,
    /// (before index, after index) with identical body and Message-ID but different header bytes.
    pub changed: Vec<(usize, usize)>,
    /// Before indexes not in after, found in a moved-to file.
    pub moved: Vec<usize>,
    /// Before indexes not in after nor in any moved-to file.
    pub lost: Vec<usize>,
    /// After indexes not in before.
    pub added: Vec<usize>,
    /// Messages in moved-to files that did not come from before.
    pub moved_to_extra: usize,
}

pub fn compare(before: &[Msg], after: &[Msg], moved_to: &[Msg]) -> Diff {
    // Exact matches first, as a multiset so duplicate copies pair off one-to-one.
    let mut exact: HashMap<([u8; 32], [u8; 32]), Vec<usize>> = HashMap::new();
    for (i, m) in after.iter().enumerate() {
        exact
            .entry((m.header_hash, m.body_hash))
            .or_default()
            .push(i);
    }
    let mut unchanged = 0;
    let mut rest_before = Vec::new();
    for (i, m) in before.iter().enumerate() {
        match exact
            .get_mut(&(m.header_hash, m.body_hash))
            .and_then(Vec::pop)
        {
            Some(_) => unchanged += 1,
            None => rest_before.push(i),
        }
    }
    let mut same: HashMap<([u8; 32], Option<String>), Vec<usize>> = HashMap::new();
    for idxs in exact.into_values() {
        for i in idxs {
            same.entry(after[i].same_message_key()).or_default().push(i);
        }
    }
    let mut elsewhere: HashMap<([u8; 32], Option<String>), usize> = HashMap::new();
    for m in moved_to {
        *elsewhere.entry(m.same_message_key()).or_default() += 1;
    }

    let (mut changed, mut moved, mut lost) = (Vec::new(), Vec::new(), Vec::new());
    for i in rest_before {
        let key = before[i].same_message_key();
        if let Some(j) = same.get_mut(&key).and_then(Vec::pop) {
            changed.push((i, j));
        } else if let Some(n) = elsewhere.get_mut(&key).filter(|n| **n > 0) {
            *n -= 1;
            moved.push(i);
        } else {
            lost.push(i);
        }
    }
    let mut added: Vec<usize> = same.into_values().flatten().collect();
    added.sort_unstable();
    let moved_to_extra = elsewhere.values().sum();
    Diff {
        unchanged,
        changed,
        moved,
        lost,
        added,
        moved_to_extra,
    }
}

fn by_name(m: &Msg) -> BTreeMap<&str, Vec<&str>> {
    let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in &m.fields {
        map.entry(f.name.as_str())
            .or_default()
            .push(f.value.as_str());
    }
    map
}

/// Header names whose values differ between two versions of a message.
pub fn changed_headers(a: &Msg, b: &Msg) -> Vec<String> {
    let (ma, mb) = (by_name(a), by_name(b));
    let mut names: Vec<String> = ma
        .keys()
        .chain(mb.keys())
        .filter(|n| ma.get(*n) != mb.get(*n))
        .map(|n| n.to_string())
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        // Same decoded values: only folding, whitespace or line endings differ.
        names.push("(raw bytes only)".into());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(h: u8, b: u8, id: &str, status: &str) -> Msg {
        Msg {
            header_hash: [h; 32],
            body_hash: [b; 32],
            message_id: Some(id.into()),
            fields: vec![
                Field {
                    name: "message-id".into(),
                    value: id.into(),
                },
                Field {
                    name: "status".into(),
                    value: status.into(),
                },
            ],
        }
    }

    #[test]
    fn classifies_every_message() {
        let before = vec![
            msg(1, 1, "a", "O"),
            msg(2, 2, "b", "O"),
            msg(3, 3, "c", "O"),
            msg(4, 4, "d", "O"),
            msg(5, 5, "e", "O"),
        ];
        // b: flags rewritten; c: moved elsewhere with new headers; d: gone; f: new mail.
        let after = vec![
            msg(1, 1, "a", "O"),
            msg(9, 2, "b", "RO"),
            msg(5, 5, "e", "O"),
            msg(6, 6, "f", ""),
        ];
        let dest = vec![msg(8, 3, "c", ""), msg(7, 7, "x", "")];
        let d = compare(&before, &after, &dest);
        assert_eq!(d.unchanged, 2);
        assert_eq!(d.changed, vec![(1, 1)]);
        assert_eq!(changed_headers(&before[1], &after[1]), vec!["status"]);
        assert_eq!(d.moved, vec![2]);
        assert_eq!(d.lost, vec![3]);
        assert_eq!(d.added, vec![3]);
        assert_eq!(d.moved_to_extra, 1);
    }
}
