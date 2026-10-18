//! Upload (copy link): HTTPS POST of a file to the keyless hosts in `upload-hosts.conf`,
//! top to bottom, until one returns a link (FUNCTIONAL_SPEC.md 6.4, 10.3).
//!
//! Network access is opt-in per use (REQ-NET-2): the menu verb asks for confirmation with
//! Cancel as the default (REQ-SEC-3) before calling this, and it lives in `twctl` only (the
//! shell DLL never performs network I/O, REQ-NET-1). Transport is WinHTTP (the OS TLS stack,
//! Schannel): no native-code crate is needed and certificates are validated by Windows.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::ops::{self, Result};

/// Largest file sent in one request.
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;
/// Largest response body read back.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Per-phase network timeout, milliseconds.
const TIMEOUT_MS: i32 = 60_000;

pub const HOSTS_FILE: &str = "upload-hosts.conf";

/// Built-in hosts used while `upload-hosts.conf` has no active line. Public keyless
/// services; their endpoints can change without notice.
const DEFAULT_HOSTS: &[&str] = &[
    "https://litterbox.catbox.moe/resources/internals/api.php | fileToUpload | text | reqtype=fileupload | time=72h | @expires=72h",
    "https://catbox.moe/user/api.php | fileToUpload | text | reqtype=fileupload",
    "https://0x0.st | file | text",
    "https://uguu.se/upload | files[] | json | @expires=3h",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    Text,
    Json,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub url: String,
    pub field: String,
    pub response: Response,
    /// Extra form fields sent with the file.
    pub extra: Vec<(String, String)>,
    /// Link lifetime in seconds, from `@expires=`.
    pub expires_secs: Option<u64>,
}

fn parse_expiry(v: &str) -> Option<u64> {
    let (n, unit) = v.split_at(v.len().checked_sub(1)?);
    let n: u64 = n.parse().ok()?;
    let mul = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    n.checked_mul(mul)
}

/// `<https-url> | <form-field> | text|json | name=value | ...`
pub fn parse_host_line(line: &str) -> Result<Host> {
    let parts: Vec<&str> = line.split('|').map(str::trim).collect();
    let [url, field, mode, rest @ ..] = parts.as_slice() else {
        return Err("expected `<https-url> | <form-field> | text|json | ...`".into());
    };
    if url.len() < 9 || !url[..8].eq_ignore_ascii_case("https://") {
        return Err(format!("only https:// hosts are allowed ({url})"));
    }
    if field.is_empty() || field.contains(['"', '\r', '\n']) {
        return Err("bad form field name".into());
    }
    let response = match *mode {
        "text" => Response::Text,
        "json" => Response::Json,
        other => return Err(format!("response type must be text or json, not {other:?}")),
    };
    let (mut extra, mut expires_secs) = (Vec::new(), None);
    for item in rest {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| format!("expected name=value, got {item:?}"))?;
        if k.contains(['"', '\r', '\n']) || v.contains(['\r', '\n']) {
            return Err(format!("bad extra field {k:?}"));
        }
        if k == "@expires" {
            expires_secs = Some(parse_expiry(v).ok_or_else(|| format!("bad @expires {v:?}"))?);
        } else {
            extra.push((k.to_owned(), v.to_owned()));
        }
    }
    Ok(Host {
        url: (*url).to_owned(),
        field: (*field).to_owned(),
        response,
        extra,
        expires_secs,
    })
}

/// Active hosts of a conf file (`#` starts a comment). Empty when every line is commented
/// out, in which case the defaults apply.
pub fn parse_hosts(text: &str) -> Result<Vec<Host>> {
    let mut hosts = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        hosts.push(parse_host_line(line).map_err(|e| format!("{HOSTS_FILE} line {}: {e}", i + 1))?);
    }
    Ok(hosts)
}

fn default_hosts() -> Vec<Host> {
    DEFAULT_HOSTS
        .iter()
        .map(|l| parse_host_line(l).expect("built-in host lines are valid"))
        .collect()
}

/// Per-user product folder for upload state when the app is not portable.
fn state_dir() -> Result<PathBuf> {
    Ok(std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA not set")?
        .join(tw_core::ids::PRODUCT_NAME))
}

/// `upload-hosts.conf` lives next to the executable for portable copies, else under
/// `%LOCALAPPDATA%\Thumbwright` (an installed folder is not writable).
pub fn hosts_path(dir: &Path) -> Result<PathBuf> {
    if tw_core::config::is_portable(dir) {
        Ok(dir.join(HOSTS_FILE))
    } else {
        Ok(state_dir()?.join(HOSTS_FILE))
    }
}

fn hosts_file_template() -> String {
    let mut s = String::from(
        "# Thumbwright upload hosts. One host per line, tried top to bottom:\n\
         #   <https-url> | <form-field> | text|json | name=value | ...\n\
         # text: the response body is the link. json: the first https link in the body.\n\
         # `@expires=72h` (m, h, d) records the link lifetime in the upload history.\n\
         # Only https:// URLs are allowed. While every line is commented out the built-in\n\
         # defaults below apply; remove the leading # from lines to use your own list.\n",
    );
    for l in DEFAULT_HOSTS {
        s.push_str("# ");
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// Creates the hosts file with the commented defaults when missing; returns its path.
pub fn ensure_hosts_file(dir: &Path) -> Result<PathBuf> {
    let path = hosts_path(dir)?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        ops::write_atomic(&path, hosts_file_template().as_bytes())?;
    }
    Ok(path)
}

/// Hosts to try: the file's active lines, else the built-in defaults.
pub fn load_hosts(dir: &Path) -> Result<Vec<Host>> {
    let path = hosts_path(dir)?;
    let hosts = match std::fs::read_to_string(&path) {
        Ok(text) => parse_hosts(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    Ok(if hosts.is_empty() {
        default_hosts()
    } else {
        hosts
    })
}

fn boundary() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("----twctl{:x}{:x}", std::process::id(), nanos)
}

/// `multipart/form-data` body: extra fields first, then the file part.
fn multipart(boundary: &str, host: &Host, file_name: &str, mime: &str, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(data.len() + 512);
    for (k, v) in &host.extra {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    let safe: String = file_name
        .chars()
        .map(|c| {
            if matches!(c, '"' | '\\' | '\r' | '\n') || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{safe}\"\r\n\
             Content-Type: {mime}\r\n\r\n",
            host.field
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Closes a WinHTTP handle on drop.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a handle returned by WinHTTP and owned by this guard.
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn win(what: &str, e: windows::core::Error) -> String {
    format!("{what}: {e}")
}

/// `scheme://host[:port]/path?query` into its parts.
fn split_url(url: &str) -> Result<(bool, String, u16, String)> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("unsupported URL {url}"));
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_owned()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?,
        ),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() || host.contains(['@', ' ']) {
        return Err(format!("bad host in {url}"));
    }
    Ok((secure, host.to_owned(), port, path))
}

/// POSTs `body` and returns `(status, response body)`.
fn post(url: &str, content_type: &str, body: &[u8]) -> Result<(u32, Vec<u8>)> {
    let (secure, host, port, path) = split_url(url)?;
    let len = u32::try_from(body.len()).map_err(|_| "request too large".to_string())?;
    let agent = HSTRING::from(format!("Thumbwright/{}", tw_core::ids::VERSION));
    // SAFETY: standard WinHTTP request sequence; every handle is closed by its guard and
    // every buffer outlives the call that uses it.
    unsafe {
        let session = Handle(WinHttpOpen(
            &agent,
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(win("WinHttpOpen", windows::core::Error::from_thread()));
        }
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS)
            .map_err(|e| win("WinHttpSetTimeouts", e))?;
        let connection = Handle(WinHttpConnect(session.0, &HSTRING::from(host), port, 0));
        if connection.0.is_null() {
            return Err(win("WinHttpConnect", windows::core::Error::from_thread()));
        }
        let request = Handle(WinHttpOpenRequest(
            connection.0,
            w!("POST"),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure {
                WINHTTP_FLAG_SECURE
            } else {
                Default::default()
            },
        ));
        if request.0.is_null() {
            return Err(win(
                "WinHttpOpenRequest",
                windows::core::Error::from_thread(),
            ));
        }
        let header: Vec<u16> = format!("Content-Type: {content_type}\r\n")
            .encode_utf16()
            .collect();
        WinHttpAddRequestHeaders(request.0, &header, WINHTTP_ADDREQ_FLAG_ADD)
            .map_err(|e| win("WinHttpAddRequestHeaders", e))?;
        WinHttpSendRequest(request.0, None, Some(body.as_ptr().cast()), len, len, 0)
            .map_err(|e| win("send", e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| win("receive", e))?;
        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| win("status", e))?;
        let mut response = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            )
            .map_err(|e| win("read", e))?;
            if read == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..read as usize]);
            if response.len() > MAX_RESPONSE_BYTES {
                return Err("the host sent an oversized response".into());
            }
        }
        Ok((status, response))
    }
}

/// The link in a response body, per the host's response type.
pub fn extract_link(host: &Host, body: &str) -> Result<String> {
    let body = body.trim();
    let link = match host.response {
        Response::Text => {
            let first = body.lines().next().unwrap_or("").trim();
            first.starts_with("https://").then(|| first.to_owned())
        }
        Response::Json => {
            let unescaped = body.replace("\\/", "/");
            unescaped.find("https://").map(|start| {
                unescaped[start..]
                    .chars()
                    .take_while(|c| {
                        !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\' | '<' | '>')
                    })
                    .collect()
            })
        }
    };
    link.filter(|l: &String| l.len() > "https://".len() && !l.contains(['\r', '\n']))
        .ok_or_else(|| "the response contained no https link".to_string())
}

/// Uploads `data` to one host and returns the link.
fn try_host(host: &Host, file_name: &str, mime: &str, data: &[u8]) -> Result<String> {
    let b = boundary();
    let (status, response) = post(
        &host.url,
        &format!("multipart/form-data; boundary={b}"),
        &multipart(&b, host, file_name, mime, data),
    )?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    extract_link(host, &String::from_utf8_lossy(&response))
}

#[derive(Debug)]
pub struct Uploaded {
    pub link: String,
    pub expires_at: Option<u64>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Uploads `path` to the first host that accepts it.
pub fn upload_file(hosts: &[Host], path: &Path) -> Result<Uploaded> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if len > MAX_UPLOAD_BYTES {
        return Err(format!(
            "{}: larger than the {} MiB upload limit",
            path.display(),
            MAX_UPLOAD_BYTES / (1024 * 1024)
        ));
    }
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let mime = crate::web_mime(&ops::ext_of(path)).unwrap_or("application/octet-stream");
    let mut failures = Vec::new();
    for host in hosts {
        match try_host(host, &name, mime, &data) {
            Ok(link) => {
                return Ok(Uploaded {
                    link,
                    expires_at: host.expires_secs.map(|s| now() + s),
                });
            }
            Err(e) => failures.push(format!("{}: {e}", host.url)),
        }
    }
    Err(format!(
        "{}: no upload host accepted the file\n{}",
        path.display(),
        failures.join("\n")
    ))
}

fn history_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("upload-history.tsv"))
}

fn tsv_field(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

/// Appends one history record: `unix time, expiry (0 = none), link, file`.
pub fn record(file: &Path, up: &Uploaded) -> Result<()> {
    use std::io::Write;
    let path = history_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(
        f,
        "{}\t{}\t{}\t{}",
        now(),
        up.expires_at.unwrap_or(0),
        tsv_field(&up.link),
        tsv_field(&file.display().to_string())
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

/// `upload-history`: records newest last.
pub fn history(json: bool) -> Result<String> {
    let text = match std::fs::read_to_string(history_path()?) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let rows: Vec<[&str; 4]> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(4, '\t');
            Some([it.next()?, it.next()?, it.next()?, it.next()?])
        })
        .collect();
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|[t, e, link, file]| {
                serde_json::json!({
                    "uploaded": t.parse::<u64>().unwrap_or(0),
                    "expires": e.parse::<u64>().ok().filter(|v| *v != 0),
                    "link": link,
                    "file": file,
                })
            })
            .collect();
        return Ok(serde_json::Value::Array(items).to_string());
    }
    Ok(rows
        .iter()
        .map(|[t, e, link, file]| {
            let exp = match e.parse::<u64>() {
                Ok(0) | Err(_) => "no expiry recorded".to_owned(),
                Ok(v) => format!("expires (unix) {v}"),
            };
            format!("{t}\t{link}\t{exp}\t{file}")
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn host_lines_enforce_https_and_parse_options() {
        let h = parse_host_line("https://h.example/up | f | json | a=b | @expires=72h").unwrap();
        assert_eq!(h.field, "f");
        assert_eq!(h.response, Response::Json);
        assert_eq!(h.extra, vec![("a".to_owned(), "b".to_owned())]);
        assert_eq!(h.expires_secs, Some(72 * 3600));
        assert!(parse_host_line("http://h.example | f | text").is_err());
        assert!(parse_host_line("https://h.example | f | xml").is_err());
        assert!(parse_host_line("https://h.example | f").is_err());
        assert!(parse_host_line("https://h.example | f | text | noequals").is_err());
        assert!(parse_host_line("https://h.example | f\"x | text").is_err());
    }

    #[test]
    fn commented_out_file_means_defaults_and_defaults_are_valid() {
        assert!(parse_hosts(&hosts_file_template()).unwrap().is_empty());
        assert_eq!(default_hosts().len(), 4);
        assert!(
            default_hosts()
                .iter()
                .any(|h| h.expires_secs == Some(72 * 3600))
        );
        let err = parse_hosts("# ok\nhttp://bad | f | text\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn link_extraction() {
        let text = parse_host_line("https://h | f | text").unwrap();
        assert_eq!(
            extract_link(&text, "https://x.test/a.png\n").unwrap(),
            "https://x.test/a.png"
        );
        assert!(extract_link(&text, "<html>nope</html>").is_err());
        assert!(extract_link(&text, "http://x.test/a.png").is_err());
        let json = parse_host_line("https://h | f | json").unwrap();
        assert_eq!(
            extract_link(&json, r#"{"files":[{"url":"https:\/\/h.test\/a.png"}]}"#).unwrap(),
            "https://h.test/a.png"
        );
        assert!(extract_link(&json, r#"{"url":"http://insecure.test/x"}"#).is_err());
    }

    /// Serves one request on a loopback socket and returns what it received.
    fn serve_once(reply: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/upload", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            let (mut header_end, mut want) = (None, usize::MAX);
            while got.len() < want {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                if header_end.is_none()
                    && let Some(p) = got.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    header_end = Some(p + 4);
                    let head = String::from_utf8_lossy(&got[..p]).to_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    want = p + 4 + len;
                }
            }
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            got
        });
        (url, handle)
    }

    #[test]
    fn posts_multipart_to_a_local_listener_and_returns_the_link() {
        let (url, server) = serve_once("https://files.test/abc.png\n");
        let dir = std::env::temp_dir().join(format!("twctl-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pic.png");
        std::fs::write(&file, b"%PNG-fake-bytes").unwrap();
        let host = Host {
            url,
            field: "fileToUpload".into(),
            response: Response::Text,
            extra: vec![("reqtype".into(), "fileupload".into())],
            expires_secs: Some(60),
        };
        let up = upload_file(&[host], &file).unwrap();
        assert_eq!(up.link, "https://files.test/abc.png");
        assert!(up.expires_at.is_some());
        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.starts_with("POST /upload HTTP/1.1"), "{request}");
        assert!(
            request
                .to_lowercase()
                .contains("multipart/form-data; boundary=")
        );
        assert!(request.contains("name=\"reqtype\"\r\n\r\nfileupload"));
        assert!(request.contains("name=\"fileToUpload\"; filename=\"pic.png\""));
        assert!(request.contains("Content-Type: image/png"));
        assert!(request.contains("%PNG-fake-bytes"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn falls_through_to_the_next_host_on_failure() {
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_url = format!("http://{}/", dead.local_addr().unwrap());
        drop(dead); // nothing listens: connection refused
        let (url, server) = serve_once("https://ok.test/x");
        let dir = std::env::temp_dir().join(format!("twctl-up2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let mk = |url: String| Host {
            url,
            field: "f".into(),
            response: Response::Text,
            extra: Vec::new(),
            expires_secs: None,
        };
        let up = upload_file(&[mk(dead_url), mk(url)], &file).unwrap();
        assert_eq!(up.link, "https://ok.test/x");
        server.join().unwrap();
        assert!(
            upload_file(&[], &file)
                .unwrap_err()
                .contains("no upload host")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn url_splitting() {
        assert_eq!(
            split_url("https://a.b:8443/x?y=1").unwrap(),
            (true, "a.b".into(), 8443, "/x?y=1".into())
        );
        assert_eq!(
            split_url("https://a.b").unwrap(),
            (true, "a.b".into(), 443, "/".into())
        );
        assert_eq!(split_url("https://a.b?q=1").unwrap().3, "/?q=1");
        assert!(split_url("ftp://a").is_err());
        assert!(split_url("https://u@a.b/").is_err());
    }
}
