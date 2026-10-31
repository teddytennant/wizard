//! Just enough HTTP/1.1 for the two local listeners that need one: the
//! artifact viewer (`wizard artifact serve`) and the trigger endpoint
//! (`wizard trigger serve`). One request per connection, no keep-alive, no
//! chunked bodies, bounded reads. Anything fancier belongs in a real server.

use std::collections::HashMap;
use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Largest header block read before the request is refused.
const MAX_HEAD: usize = 16 * 1024;

/// Largest request body read before the request is refused.
const MAX_BODY: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The path without its query string, still percent-encoded.
    pub path: String,
    pub query: String,
    /// Header names lowercased.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Read one request. `Ok(None)` when the peer sent nothing.
pub async fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header too large",
            ));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short request",
                ))
            };
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad request line",
        ));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    }))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    }
}

/// Write a whole response and close the connection.
pub async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

/// Percent-decode a URL path. `None` when it does not decode to UTF-8.
pub fn decode_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = path.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A guess at a media type from a file extension.
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "txt" | "md" | "csv" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_round_trips_and_refuses_bad_input() {
        assert_eq!(decode_path("/a%20b/c").as_deref(), Some("/a b/c"));
        assert_eq!(decode_path("/%2e%2e/x").as_deref(), Some("/../x"));
        assert_eq!(decode_path("/%zz"), None);
        assert_eq!(decode_path("/%ff"), None);
    }

    #[test]
    fn media_types_follow_the_extension() {
        assert_eq!(content_type("a/index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("x.PNG"), "image/png");
        assert_eq!(content_type("x.unknown"), "application/octet-stream");
    }

    #[tokio::test]
    async fn a_request_with_a_body_is_read_whole() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"POST /x/y?z=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nX-Foo: Bar\r\n\r\nhello")
                .await
                .unwrap();
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.unwrap();
            String::from_utf8(out).unwrap()
        });
        let (mut conn, _) = listener.accept().await.unwrap();
        let req = read_request(&mut conn).await.unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/x/y");
        assert_eq!(req.query, "z=1");
        assert_eq!(req.header("x-foo"), Some("Bar"));
        assert_eq!(req.body, b"hello");
        respond(&mut conn, 200, "text/plain", b"ok").await.unwrap();
        let reply = client.await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK"));
        assert!(reply.ends_with("ok"));
    }
}
