//! Google sign-in for an account (OAuth for installed apps: loopback
//! redirect with PKCE) and JSON calls to Google's APIs. The OAuth client and
//! refresh tokens live in `~/.config/munchymail/creds/`, readable only by you.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use data_encoding::BASE64URL_NOPAD;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use tracing::warn;
use ureq::http::Response;

use crate::store::now;
use crate::util::hex;

pub const GMAIL_READ: &str = "https://www.googleapis.com/auth/gmail.readonly";
pub const CONTACTS_READ: &str = "https://www.googleapis.com/auth/contacts.readonly";
/// Read-only: nothing here can change or delete mail. Sending will add gmail.send.
pub const SCOPES: [&str; 2] = [GMAIL_READ, CONTACTS_READ];
const CLIENT_FILE: &str = "google-client.json";
const GMAIL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
/// How long to wait for the browser to come back.
const SIGN_IN_WAIT: Duration = Duration::from_secs(600);

struct OauthClient {
    id: String,
    secret: String,
    auth_uri: String,
    token_uri: String,
}

/// `$XDG_CONFIG_HOME/munchymail/creds` (default `~/.config`), created mode 0700.
pub fn creds_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    let dir = base.join("munchymail").join("creds");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn token_path(address: &str) -> Result<PathBuf> {
    Ok(creds_dir()?.join(format!("{address}.google-token.json")))
}

fn read_client(path: &Path) -> Result<(OauthClient, String)> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v: Json =
        serde_json::from_str(&raw).with_context(|| format!("{} is not JSON", path.display()))?;
    let Some(c) = v.get("installed") else {
        bail!(
            "{} is not a Desktop app OAuth client (no \"installed\" key); create one of type Desktop app",
            path.display()
        );
    };
    let field = |k: &str| {
        c[k].as_str()
            .map(str::to_string)
            .with_context(|| format!("{} has no installed.{k}", path.display()))
    };
    let client = OauthClient {
        id: field("client_id")?,
        secret: field("client_secret")?,
        auth_uri: field("auth_uri")?,
        token_uri: field("token_uri")?,
    };
    Ok((client, raw))
}

/// Write `bytes` to `path` readable only by its owner, replacing it whole.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn random(n: usize) -> Result<Vec<u8>> {
    let mut b = vec![0; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of `key` in a URL or query string.
fn param(query: &str, key: &str) -> Option<String> {
    let q = query.split_once('?').map_or(query, |(_, q)| q);
    let q = q.split('#').next().unwrap_or("");
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| decode(v))
    })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// The response body as JSON, or an error with Google's own message.
fn json_of(mut r: Response<ureq::Body>, what: &str) -> Result<Json> {
    let status = r.status();
    let body = r
        .body_mut()
        .with_config()
        .limit(64 << 20)
        .read_to_string()
        .with_context(|| format!("{what}: reading the response"))?;
    if !status.is_success() {
        let v: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
        let msg = v["error"]["message"]
            .as_str()
            .or(v["error_description"].as_str())
            .or(v["error"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| body.chars().take(300).collect());
        bail!("{what}: HTTP {}: {msg}", status.as_u16());
    }
    serde_json::from_str(&body).with_context(|| format!("{what}: response is not JSON"))
}

/// Wait for the browser's redirect, on the listener or pasted on stdin.
fn redirected(listener: TcpListener) -> Result<String> {
    let (tx, rx) = mpsc::channel();
    let web = tx.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut conn = conn;
            let mut line = String::new();
            if BufReader::new(&conn).read_line(&mut line).is_err() {
                continue;
            }
            let target = line.split(' ').nth(1).unwrap_or("").to_string();
            let ours = param(&target, "state").is_some();
            let page = if ours {
                "munchymail: done, you can close this tab and go back to the terminal."
            } else {
                "munchymail is waiting for Google's sign-in redirect."
            };
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            if ours {
                let _ = web.send(target);
                return;
            }
        }
    });
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if line.contains("state=") {
                let _ = tx.send(line.trim().to_string());
                return;
            }
        }
    });
    rx.recv_timeout(SIGN_IN_WAIT)
        .context("no sign-in within 10 minutes; run `munchymail gmail auth` again")
}

/// Sign `address` in to Google and keep its refresh token in the creds
/// directory. `client` is an OAuth client JSON to copy there first.
pub fn authorize(address: &str, client: Option<&Path>, out: &mut impl Write) -> Result<Json> {
    let kept = creds_dir()?.join(CLIENT_FILE);
    let (c, raw) = match client {
        Some(p) => read_client(p)?,
        None if kept.exists() => read_client(&kept)?,
        None => bail!(
            "no Google OAuth client in {}; put the client JSON of a Desktop app there, or pass --client FILE",
            kept.display()
        ),
    };
    if client.is_some() {
        write_private(&kept, raw.as_bytes())?;
    }
    let verifier = BASE64URL_NOPAD.encode(&random(32)?);
    let challenge = BASE64URL_NOPAD.encode(&Sha256::digest(verifier.as_bytes()));
    let state = hex(&random(16)?);
    let listener = TcpListener::bind("127.0.0.1:0").context("listening on 127.0.0.1")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}&access_type=offline&prompt=consent&login_hint={}",
        c.auth_uri,
        encode(&c.id),
        encode(&redirect),
        encode(&SCOPES.join(" ")),
        encode(address)
    );
    writeln!(
        out,
        "Open this in a browser and sign in as {address} (read-only: Gmail and contacts):\n\n{url}\n"
    )?;
    writeln!(
        out,
        "Waiting for the redirect to {redirect}. If the browser is on another machine the page \
         will not load: copy the address it ended on and paste it here."
    )?;
    out.flush()?;
    let back = redirected(listener)?;
    if param(&back, "state").as_deref() != Some(state.as_str()) {
        bail!(
            "the redirect's state does not match this sign-in; run `munchymail gmail auth` again"
        );
    }
    if let Some(e) = param(&back, "error") {
        bail!("Google did not grant access: {e}");
    }
    let code = param(&back, "code").context("the redirect has no code")?;
    let t = json_of(
        agent().post(&c.token_uri).send_form([
            ("code", code.as_str()),
            ("client_id", &c.id),
            ("client_secret", &c.secret),
            ("redirect_uri", &redirect),
            ("grant_type", "authorization_code"),
            ("code_verifier", &verifier),
        ])?,
        "exchanging the sign-in code",
    )?;
    let granted: Vec<&str> = t["scope"].as_str().unwrap_or("").split(' ').collect();
    if !granted.contains(&GMAIL_READ) {
        bail!(
            "Gmail read access was not granted (its box was unticked?); run `munchymail gmail auth` again and allow it"
        );
    }
    if !granted.contains(&CONTACTS_READ) {
        warn!("contacts access was not granted; Gmail sync works without it");
    }
    let refresh = t["refresh_token"]
        .as_str()
        .context("Google sent no refresh token; run `munchymail gmail auth` again")?;
    let access = t["access_token"]
        .as_str()
        .context("Google sent no access token")?;
    let profile = json_of(
        agent()
            .get(&format!("{GMAIL}/profile"))
            .header("Authorization", &format!("Bearer {access}"))
            .call()?,
        "reading the Gmail profile",
    )?;
    let signed_in = profile["emailAddress"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if signed_in != address {
        bail!(
            "signed in as {signed_in:?}, but this account is {address}; run again and choose {address}"
        );
    }
    let token = json!({
        "refresh_token": refresh,
        "scope": t["scope"],
        "client_id": c.id,
        "address": address,
        "created_at": now(),
    });
    write_private(&token_path(address)?, token.to_string().as_bytes())?;
    Ok(profile)
}

/// Calls to Google as an account, refreshing the access token as needed.
pub struct Session {
    agent: ureq::Agent,
    client: OauthClient,
    refresh: String,
    access: Option<(String, Instant)>,
    pub scopes: Vec<String>,
}

impl Session {
    pub fn open(address: &str) -> Result<Session> {
        let path = token_path(address)?;
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("{address} has no Google sign-in; run `munchymail gmail auth`")
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let t: Json = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not JSON", path.display()))?;
        let (client, _) = read_client(&creds_dir()?.join(CLIENT_FILE))?;
        Ok(Session {
            agent: agent(),
            client,
            refresh: t["refresh_token"]
                .as_str()
                .context("token file has no refresh_token")?
                .into(),
            access: None,
            scopes: t["scope"]
                .as_str()
                .unwrap_or("")
                .split(' ')
                .map(str::to_string)
                .collect(),
        })
    }

    fn access_token(&mut self) -> Result<String> {
        if let Some((t, until)) = &self.access
            && Instant::now() < *until
        {
            return Ok(t.clone());
        }
        let r = self.agent.post(&self.client.token_uri).send_form([
            ("client_id", self.client.id.as_str()),
            ("client_secret", &self.client.secret),
            ("refresh_token", &self.refresh),
            ("grant_type", "refresh_token"),
        ])?;
        let t = json_of(r, "refreshing the Google token").map_err(|e| {
            e.context(
                "the sign-in may be revoked, or expired (apps left in Testing lose it after 7 days); run `munchymail gmail auth` again",
            )
        })?;
        let token = t["access_token"]
            .as_str()
            .context("no access_token")?
            .to_string();
        let secs = t["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120);
        self.access = Some((token.clone(), Instant::now() + Duration::from_secs(secs)));
        Ok(token)
    }

    /// GET a Gmail API path under users/me, e.g. "profile" or "labels".
    pub fn gmail(&mut self, path: &str) -> Result<Json> {
        let token = self.access_token()?;
        let r = self
            .agent
            .get(&format!("{GMAIL}/{path}"))
            .header("Authorization", &format!("Bearer {token}"))
            .call()?;
        json_of(
            r,
            &format!("Gmail {}", path.split('?').next().unwrap_or(path)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_params() {
        let u = "/?state=ab12&code=4%2F0Ab_x+y&scope=a%20b";
        assert_eq!(param(u, "state").as_deref(), Some("ab12"));
        assert_eq!(param(u, "code").as_deref(), Some("4/0Ab_x y"));
        assert_eq!(
            param("http://127.0.0.1:5/?error=access_denied", "error").as_deref(),
            Some("access_denied")
        );
        assert_eq!(param(u, "nope"), None);
        assert_eq!(decode("100%"), "100%");
        assert_eq!(encode("a b/c@d"), "a%20b%2Fc%40d");
    }
}
