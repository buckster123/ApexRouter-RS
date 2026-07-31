//! A very small HTTP/1.1 server and client, on `std::net` and nothing else.
//!
//! Deliberately not axum: the fake `llama-server` is built on demand into its own target
//! directory, and a test double whose first use costs a tokio/hyper/axum compile is a test
//! double nobody reaches for. What is here is the subset the ApexRouter code paths
//! actually exercise — keep-alive, `Content-Length` and chunked request bodies, chunked
//! responses for SSE — plus the two things a well-behaved framework will not do for you:
//! never answer, and abort a response half-written.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Longest request line plus headers we will read before giving up.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Longest body we will read. A 4 MiB prompt is a real ApexRouter test; 64 MiB is not.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// One parsed request.
#[derive(Clone, Debug, Default)]
pub struct Req {
    /// `GET`, `POST`, …
    pub method: String,
    /// Path with the query string removed, never empty.
    pub path: String,
    /// Query string without the `?`.
    pub query: String,
    /// Header names lowercased; a repeated name keeps the last value.
    pub headers: BTreeMap<String, String>,
    /// The body, decoded from `Content-Length` or `Transfer-Encoding: chunked`.
    pub body: Vec<u8>,
    /// `HTTP/1.0` or `HTTP/1.1`.
    pub version: String,
}

impl Req {
    /// One header, lowercased name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The body as JSON, when it is JSON.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// The body as text, lossily.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Whether the connection should stay open after this exchange.
    pub fn keep_alive(&self) -> bool {
        match self.header("connection") {
            Some(v) if v.eq_ignore_ascii_case("close") => false,
            Some(v) if v.to_ascii_lowercase().contains("keep-alive") => true,
            _ => self.version != "HTTP/1.0",
        }
    }

    /// A query parameter.
    pub fn param(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.to_owned())
        })
    }
}

/// Read one request. `Ok(None)` is a clean end of connection.
pub fn read_request(r: &mut BufReader<TcpStream>) -> io::Result<Option<Req>> {
    let mut head = String::new();
    let mut line = String::new();

    // Request line. An immediate EOF here is the client hanging up between requests.
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or("/").to_owned();
    let version = parts.next().unwrap_or("HTTP/1.1").to_owned();
    if method.is_empty() {
        return Ok(None);
    }

    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        head.push_str(&line);
        if head.len() > MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header too large",
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }

    let body = read_body(r, &headers)?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (target, String::new()),
    };

    Ok(Some(Req {
        method,
        path,
        query,
        headers,
        body,
        version,
    }))
}

/// `Content-Length`, then `Transfer-Encoding: chunked`, then no body.
fn read_body(
    r: &mut BufReader<TcpStream>,
    headers: &BTreeMap<String, String>,
) -> io::Result<Vec<u8>> {
    if headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        return read_chunked(r);
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if len == 0 {
        return Ok(Vec::new());
    }
    if len > MAX_BODY_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// RFC 9112 chunked transfer decoding, without trailers.
fn read_chunked(r: &mut BufReader<TcpStream>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunk header"));
        }
        let size_text = line.trim().split(';').next().unwrap_or("0").to_owned();
        let size = usize::from_str_radix(size_text.trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk size"))?;
        if size == 0 {
            // Consume the trailer section up to the final empty line.
            loop {
                line.clear();
                if r.read_line(&mut line)? == 0 {
                    break;
                }
                if line.trim().is_empty() {
                    break;
                }
            }
            return Ok(out);
        }
        if out.len() + size > MAX_BODY_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let mut buf = vec![0u8; size];
        r.read_exact(&mut buf)?;
        out.extend_from_slice(&buf);
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf)?;
    }
}

/// The reason phrase for the statuses this fake emits.
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Write a complete response with a `Content-Length`.
pub fn respond(
    w: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    keep_alive: bool,
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        reason(status),
        body.len(),
        if keep_alive { "keep-alive" } else { "close" },
    )
    .into_bytes();
    head.extend_from_slice(body);
    w.write_all(&head)?;
    w.flush()
}

/// Write a JSON response.
pub fn respond_json(
    w: &mut TcpStream,
    status: u16,
    body: &serde_json::Value,
    keep_alive: bool,
) -> io::Result<()> {
    let raw = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    respond(w, status, "application/json", &raw, keep_alive)
}

/// An OpenAI-shaped error body, which is what every ApexRouter error path parses.
pub fn error_body(status: u16, message: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({"error": {"code": status, "message": message, "type": kind}})
}

/// Start a chunked `text/event-stream` response. Headers are flushed immediately, which is
/// what makes "the response headers arrived before the first token" observable.
pub fn begin_sse(w: &mut TcpStream) -> io::Result<()> {
    w.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-cache\r\n\
          Transfer-Encoding: chunked\r\n\
          Connection: close\r\n\r\n",
    )?;
    w.flush()
}

/// One chunk of a chunked body, flushed.
pub fn write_chunk(w: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    w.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")?;
    w.flush()
}

/// The terminating zero-length chunk. Omitting it is what `die_mid_stream` does.
pub fn end_chunked(w: &mut TcpStream) -> io::Result<()> {
    w.write_all(b"0\r\n\r\n")?;
    w.flush()
}

// -------------------------------------------------------------------------------------
// A client, for the control helper only
// -------------------------------------------------------------------------------------

/// One blocking request against a loopback base URL. Used by [`crate::Control`] so a test
/// can read a fake's state without pulling reqwest into a non-async context.
///
/// # Errors
/// Any transport failure, or a non-2xx status, as a rendered string.
pub fn request(
    base_url: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let hostport = base_url
        .trim_end_matches('/')
        .rsplit("//")
        .next()
        .unwrap_or(base_url)
        .to_owned();
    let mut stream =
        TcpStream::connect(&hostport).map_err(|e| format!("connect {hostport}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {hostport}\r\nAccept: */*\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(payload.as_bytes()))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("write {path}: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read {path}: {e}"))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{path}: no header terminator in {} bytes", text.len()))?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(format!("{method} {path} -> {status}: {body}"));
    }
    if body.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(body).map_err(|e| format!("{path}: {e} in {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_alive_follows_the_version_and_the_header() {
        let mut r = Req {
            version: "HTTP/1.1".to_owned(),
            ..Req::default()
        };
        assert!(r.keep_alive());
        r.headers
            .insert("connection".to_owned(), "close".to_owned());
        assert!(!r.keep_alive());
        r.headers.clear();
        r.version = "HTTP/1.0".to_owned();
        assert!(!r.keep_alive());
        r.headers
            .insert("connection".to_owned(), "keep-alive".to_owned());
        assert!(r.keep_alive());
    }

    #[test]
    fn a_query_parameter_is_readable() {
        let r = Req {
            query: "action=save&id_slot=2".to_owned(),
            ..Req::default()
        };
        assert_eq!(r.param("action").as_deref(), Some("save"));
        assert_eq!(r.param("id_slot").as_deref(), Some("2"));
        assert_eq!(r.param("nope"), None);
    }
}
