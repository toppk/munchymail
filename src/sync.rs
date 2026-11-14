//! Keeping live origins current: when each was last checked, failures and
//! backoff, and which failures stop an origin or the whole service.

use anyhow::Result;

use crate::store::{Store, now};

/// How often the service checks a live Gmail origin.
pub const GMAIL_EVERY: i64 = 180;
/// The longest wait between retries of a failing origin.
const MAX_BACKOFF: i64 = 3600;

/// What a failed check means for what happens next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Retry later, backing off: the network, Google's servers, a lock.
    Transient,
    /// This origin needs someone: a revoked sign-in, a missing mailbox.
    Origin,
    /// Nothing will work until someone looks: a corrupt database, a full disk.
    Systemic,
}

pub fn classify(e: &anyhow::Error) -> Failure {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            match io.kind() {
                StorageFull | ReadOnlyFilesystem | QuotaExceeded => return Failure::Systemic,
                NotFound | PermissionDenied => return Failure::Origin,
                _ => {}
            }
        }
    }
    let text = format!("{e:#}").to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if any(&[
        "malformed",
        "corrupt",
        "disk is full",
        "no space left",
        "read-only file system",
    ]) {
        Failure::Systemic
    } else if any(&["no google sign-in", "http 401"])
        || (text.contains("sign-in may be revoked") && any(&["http 400", "http 401"]))
        || (text.contains("http 403") && !any(&["ratelimitexceeded", "quota"]))
    {
        Failure::Origin
    } else {
        Failure::Transient
    }
}

/// Seconds to wait after the `failures`th failure in a row.
pub fn backoff(failures: i64) -> i64 {
    let doublings = failures.clamp(1, 16) - 1;
    (GMAIL_EVERY << doublings).min(MAX_BACKOFF)
}

/// A live, unretired origin and how keeping it current is going.
#[derive(Debug, Clone)]
pub struct Live {
    pub origin_id: i64,
    pub label: String,
    pub gmail: bool,
    pub checked_at: Option<i64>,
    pub retry_at: Option<i64>,
    pub stopped: Option<String>,
}

impl Live {
    /// Whether the timer should check it at `t`.
    pub fn due(&self, t: i64) -> bool {
        self.stopped.is_none()
            && match self.retry_at {
                Some(r) => t >= r,
                None => self.checked_at.is_none_or(|c| t >= c + GMAIL_EVERY),
            }
    }
}

pub async fn live(store: &Store) -> Result<Vec<Live>> {
    let mut rows = store
        .conn
        .query(
            "SELECT g.origin_id, g.label, g.root LIKE 'gmail:%', s.checked_at, s.retry_at,
                    CASE WHEN s.stopped_at IS NOT NULL THEN coalesce(s.problem, 'stopped') END
             FROM origin g LEFT JOIN sync_state s ON s.origin_id = g.origin_id
             WHERE g.role = 'live' AND g.retired_at IS NULL ORDER BY g.origin_id",
            (),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push(Live {
            origin_id: r.get(0)?,
            label: r.get(1)?,
            gmail: r.get::<i64>(2)? != 0,
            checked_at: r.get(3)?,
            retry_at: r.get(4)?,
            stopped: r.get(5)?,
        });
    }
    Ok(out)
}

pub async fn checked(store: &Store, origin_id: i64) -> Result<()> {
    let t = now();
    store
        .conn
        .execute(
            "INSERT INTO sync_state (origin_id, checked_at, tried_at, failures)
             VALUES (?1, ?2, ?2, 0)
             ON CONFLICT (origin_id) DO UPDATE SET checked_at = ?2, tried_at = ?2,
                 failures = 0, retry_at = NULL, problem = NULL",
            (origin_id, t),
        )
        .await?;
    Ok(())
}

/// Record a failed check and say what it means; an origin failure stops it.
pub async fn failed(store: &Store, origin_id: i64, e: &anyhow::Error) -> Result<Failure> {
    let kind = classify(e);
    let (t, problem) = (now(), format!("{e:#}"));
    let failures: i64 = {
        let mut rows = store
            .conn
            .query(
                "SELECT failures FROM sync_state WHERE origin_id = ?1",
                (origin_id,),
            )
            .await?;
        match rows.next().await? {
            Some(r) => r.get::<i64>(0)? + 1,
            None => 1,
        }
    };
    let stopped = (kind == Failure::Origin).then_some(t);
    store
        .conn
        .execute(
            "INSERT INTO sync_state (origin_id, tried_at, failures, retry_at, stopped_at, problem)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (origin_id) DO UPDATE SET tried_at = ?2, failures = ?3,
                 retry_at = ?4, stopped_at = ?5, problem = ?6",
            (
                origin_id,
                t,
                failures,
                t + backoff(failures),
                stopped,
                problem,
            ),
        )
        .await?;
    Ok(kind)
}

/// Let a stopped origin be checked again; false if none has `label`.
pub async fn resume(store: &Store, label: &str) -> Result<bool> {
    let n = store
        .conn
        .execute(
            "UPDATE sync_state SET stopped_at = NULL, retry_at = NULL, failures = 0, problem = NULL
             WHERE origin_id = (SELECT origin_id FROM origin WHERE label = ?1)",
            (label,),
        )
        .await?;
    let exists = store
        .conn
        .query("SELECT 1 FROM origin WHERE label = ?1", (label,))
        .await?
        .next()
        .await?
        .is_some();
    Ok(n > 0 || exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn classifies_failures() {
        let io = |k| anyhow::Error::from(std::io::Error::from(k));
        assert_eq!(
            classify(&io(std::io::ErrorKind::StorageFull)),
            Failure::Systemic
        );
        assert_eq!(classify(&io(std::io::ErrorKind::NotFound)), Failure::Origin);
        assert_eq!(
            classify(&io(std::io::ErrorKind::ConnectionReset)),
            Failure::Transient
        );
        let revoked =
            anyhow!("refreshing the Google token: HTTP 400: Token has been expired or revoked.")
                .context("the sign-in may be revoked, or expired");
        assert_eq!(classify(&revoked), Failure::Origin);
        let refresh_down = anyhow!("refreshing the Google token: HTTP 503: unavailable")
            .context("the sign-in may be revoked, or expired");
        assert_eq!(classify(&refresh_down), Failure::Transient);
        assert_eq!(
            classify(&anyhow!(
                "Gmail history: HTTP 403: Rate Limit Exceeded quota"
            )),
            Failure::Transient
        );
        assert_eq!(
            classify(&anyhow!("Gmail history: HTTP 403: Insufficient Permission")),
            Failure::Origin
        );
        assert_eq!(
            classify(&anyhow!("Gmail history: dns error")),
            Failure::Transient
        );
        let corrupt =
            Err::<(), _>(anyhow!("database disk image is malformed")).context("importing");
        assert_eq!(classify(&corrupt.unwrap_err()), Failure::Systemic);
    }

    #[test]
    fn backs_off_to_an_hour() {
        assert_eq!(backoff(1), 180);
        assert_eq!(backoff(2), 360);
        assert_eq!(backoff(5), 2880);
        assert_eq!(backoff(6), 3600);
        assert_eq!(backoff(100), 3600);
    }

    #[test]
    fn due_when_never_checked_or_after_the_interval_or_the_retry() {
        let l = Live {
            origin_id: 1,
            label: "gmail".into(),
            gmail: true,
            checked_at: None,
            retry_at: None,
            stopped: None,
        };
        assert!(l.due(0));
        let checked = Live {
            checked_at: Some(1000),
            ..l.clone()
        };
        assert!(!checked.due(1000 + GMAIL_EVERY - 1) && checked.due(1000 + GMAIL_EVERY));
        let failing = Live {
            retry_at: Some(5000),
            ..checked.clone()
        };
        assert!(!failing.due(4999) && failing.due(5000));
        let stopped = Live {
            stopped: Some("revoked".into()),
            ..l
        };
        assert!(!stopped.due(i64::MAX));
    }
}
