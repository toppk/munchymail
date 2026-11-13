//! `munchymail serve`: one process owns the account databases and runs CLI
//! commands sent over `$XDG_RUNTIME_DIR/munchymail/serve.sock`, so they no
//! longer queue for the database lock. A request is one JSON line (`argv`,
//! `cwd`, `account`, `data`); the reply is frames of a tag byte, a big-endian
//! u32 length and the bytes: stdout, stderr (log lines), then the exit code,
//! or only a refusal when the service holds another data root.

use std::cell::RefCell;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tracing::{debug, info, warn};

const STDOUT: u8 = b'1';
const STDERR: u8 = b'2';
const EXIT: u8 = b'x';
const OTHER_DATA: u8 = b'd';

/// `$MUNCHYMAIL_SOCKET`, else `$XDG_RUNTIME_DIR/munchymail/serve.sock`; none
/// without either (cron, ssh without a session), so commands run on their own.
pub fn socket() -> Option<PathBuf> {
    let var = |v| {
        std::env::var_os(v)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    var("MUNCHYMAIL_SOCKET").or_else(|| Some(var("XDG_RUNTIME_DIR")?.join("munchymail/serve.sock")))
}

/// One command line to run, as the client parsed it.
pub struct Request {
    /// Arguments after the program name.
    pub argv: Vec<String>,
    /// Where relative paths in `argv` start.
    pub cwd: PathBuf,
    /// The client's `--account` or `MUNCHYMAIL_ACCOUNT`.
    pub account: Option<String>,
    /// The client's data root, canonical; the service runs only its own.
    pub data: PathBuf,
    /// The client's terminal width, if its output is one.
    pub width: Option<usize>,
}

impl Request {
    fn to_line(&self) -> String {
        let v = json!({"argv": self.argv, "cwd": self.cwd, "account": self.account, "data": self.data, "width": self.width});
        format!("{v}\n")
    }

    fn parse(line: &str) -> Result<Request> {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let argv = v["argv"]
            .as_array()
            .context("no argv")?
            .iter()
            .map(|a| a.as_str().map(String::from).context("argv must be strings"))
            .collect::<Result<_>>()?;
        Ok(Request {
            argv,
            cwd: PathBuf::from(v["cwd"].as_str().context("no cwd")?),
            account: v["account"].as_str().map(String::from),
            data: PathBuf::from(v["data"].as_str().context("no data")?),
            width: v["width"].as_u64().map(|w| w as usize),
        })
    }
}

fn frame(w: &mut impl Write, tag: u8, bytes: &[u8]) -> io::Result<()> {
    let mut f = Vec::with_capacity(bytes.len() + 5);
    f.push(tag);
    f.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    f.extend_from_slice(bytes);
    w.write_all(&f)
}

/// A request's stdout.
pub struct Reply(UnixStream);

impl Write for Reply {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        frame(&mut self.0, STDOUT, b)?;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

thread_local! {
    static CLIENT: RefCell<Option<UnixStream>> = const { RefCell::new(None) };
}

/// A tracing writer sending log lines to the client whose request this thread runs.
pub struct ClientLog;

impl Write for ClientLog {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        CLIENT.with_borrow_mut(|c| {
            // A client that went away only loses its log.
            if let Some(s) = c {
                let _ = frame(s, STDERR, b);
            }
        });
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ClientLog {
    type Writer = ClientLog;
    fn make_writer(&'a self) -> ClientLog {
        ClientLog
    }
}

fn peer_uid(s: &UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: cred and len are valid for the call and len is cred's size.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Listen on `path`, replacing a stale socket.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("already serving on {}", path.display());
        }
        fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    info!(socket = %path.display(), "listening");
    Ok(listener)
}

/// Serve requests for data root `data` until killed, each on its own thread;
/// `handle` runs one and returns its exit code.
pub fn serve(
    listener: UnixListener,
    data: PathBuf,
    handle: impl Fn(Request, &mut Reply) -> u8 + Send + Sync + 'static,
) -> Result<()> {
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    let handle = Arc::new(handle);
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                warn!("accept: {e}");
                continue;
            }
        };
        match peer_uid(&conn) {
            Ok(uid) if uid == me => {}
            other => {
                warn!(?other, "refused a client of another user");
                continue;
            }
        }
        let (handle, data) = (handle.clone(), data.clone());
        std::thread::spawn(move || match answer(conn, &data, &*handle) {
            Err(e)
                if e.downcast_ref::<io::Error>().map(io::Error::kind)
                    == Some(io::ErrorKind::BrokenPipe) => {}
            Err(e) => warn!("request: {e:#}"),
            Ok(()) => {}
        });
    }
    Ok(())
}

fn answer(
    mut conn: UnixStream,
    data: &Path,
    handle: &impl Fn(Request, &mut Reply) -> u8,
) -> Result<()> {
    let mut line = String::new();
    BufReader::new(conn.try_clone()?).read_line(&mut line)?;
    let req = Request::parse(&line).context("bad request")?;
    if req.data != data {
        frame(&mut conn, OTHER_DATA, data.as_os_str().as_encoded_bytes())?;
        return Ok(());
    }
    CLIENT.set(Some(conn.try_clone()?));
    let mut reply = Reply(conn.try_clone()?);
    let code = handle(req, &mut reply);
    CLIENT.set(None);
    frame(&mut reply.0, EXIT, &[code])?;
    Ok(())
}

/// Run `req` in the service listening on `socket`, copying its output to
/// `stdout` and `stderr`; `None` when none is, or it holds another data root.
pub fn forward(
    socket: &Path,
    req: &Request,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<Option<u8>> {
    let mut conn = match UnixStream::connect(socket) {
        Ok(c) => c,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e).context("connecting to munchymail serve"),
    };
    conn.write_all(req.to_line().as_bytes())?;
    let mut conn = BufReader::new(conn);
    loop {
        let mut head = [0u8; 5];
        conn.read_exact(&mut head)
            .context("munchymail serve ended the request early; see its log")?;
        let mut bytes = vec![0; u32::from_be_bytes(head[1..].try_into()?) as usize];
        conn.read_exact(&mut bytes)?;
        match head[0] {
            STDOUT => match stdout.write_all(&bytes) {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(Some(0)),
                r => r?,
            },
            STDERR => stderr.write_all(&bytes)?,
            EXIT => {
                stdout.flush()?;
                return Ok(Some(bytes.first().copied().unwrap_or(1)));
            }
            OTHER_DATA => {
                let held = String::from_utf8_lossy(&bytes);
                debug!(%held, "munchymail serve holds another data root; running here");
                return Ok(None);
            }
            t => bail!("unknown frame {t} from munchymail serve"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_output_and_exit_code() {
        let dir = std::env::temp_dir().join(format!("munchymail-serve-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sock = dir.join("run/serve.sock");
        let mut req = Request {
            argv: vec!["a".into(), "b c".into()],
            cwd: "/x".into(),
            account: None,
            data: "/data".into(),
            width: None,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(forward(&sock, &req, &mut out, &mut err).unwrap(), None);
        let listener = bind(&sock).unwrap();
        std::thread::spawn(move || {
            serve(listener, "/data".into(), |req, reply| {
                writeln!(reply, "{}", req.argv.join("|")).unwrap();
                3
            })
        });
        assert_eq!(forward(&sock, &req, &mut out, &mut err).unwrap(), Some(3));
        assert_eq!(out, b"a|b c\n");
        req.data = "/other".into();
        assert_eq!(forward(&sock, &req, &mut out, &mut err).unwrap(), None);
        assert!(bind(&sock).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
