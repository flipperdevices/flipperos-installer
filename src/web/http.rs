// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! Just enough HTTP/1.1 for one browser or one client on a USB cable.
//!
//! One request per connection (`Connection: close`), bodies by length or
//! chunked, responses either whole or chunked for the streams. Nothing here is
//! exposed beyond the link, so there is no keep-alive, no TLS and no
//! compression to get wrong.

use std::io::{self, BufRead, Read, Write};

/// Longest request line plus headers accepted.
const MAX_HEAD: u64 = 16 * 1024;

/// A request line and its headers; the body is read separately, see [`body`].
#[derive(Debug, Default)]
pub struct Request {
    pub method: String,
    /// The path, without the query string, percent-decoded.
    pub path: String,
    /// Query parameters, decoded, in order.
    pub query: Vec<(String, String)>,
    /// Headers with lowercase names, in order.
    pub headers: Vec<(String, String)>,
}

/// Why a request could not be read.
#[derive(Debug)]
pub enum HeadError {
    /// The client went away, or sent nothing: no response is owed.
    Closed,
    /// Malformed or oversized; answer with this status.
    Bad(u16, String),
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether the client asked for a web page rather than plain text or JSON.
    ///
    /// Browsers say `text/html` when they navigate or submit a form; `curl` says
    /// `*/*`, and the page's own script asks for `text/plain`. That one header is
    /// what lets the browser and a client app share every endpoint.
    pub fn wants_html(&self) -> bool {
        self.header("accept")
            .is_some_and(|a| a.to_ascii_lowercase().contains("text/html"))
    }

    pub fn expects_continue(&self) -> bool {
        self.header("expect")
            .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.trim().parse().ok()
    }

    fn is_chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"))
    }

    /// Whether a body follows the head at all.
    pub fn has_body(&self) -> bool {
        self.is_chunked() || self.content_length().is_some_and(|n| n > 0)
    }

    /// The `boundary` of a `multipart/form-data` body, if this is one.
    pub fn multipart_boundary(&self) -> Option<String> {
        let ct = self.header("content-type")?;
        let mut parts = ct.split(';');
        if !parts
            .next()?
            .trim()
            .eq_ignore_ascii_case("multipart/form-data")
        {
            return None;
        }
        parts.find_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            k.trim()
                .eq_ignore_ascii_case("boundary")
                .then(|| v.trim().trim_matches('"').to_string())
        })
    }
}

/// Read a request's head.
pub fn read_head<R: BufRead>(r: &mut R) -> Result<Request, HeadError> {
    let mut budget = MAX_HEAD;
    let mut line = String::new();
    let mut next_line = |line: &mut String| -> Result<(), HeadError> {
        line.clear();
        let n = r
            .by_ref()
            .take(budget)
            .read_line(line)
            .map_err(|_| HeadError::Closed)?;
        if n == 0 {
            return Err(HeadError::Closed);
        }
        if !line.ends_with('\n') {
            return Err(HeadError::Bad(431, "request head too large".to_string()));
        }
        budget -= n as u64;
        let trimmed = line.trim_end_matches(['\r', '\n']).len();
        line.truncate(trimmed);
        Ok(())
    };

    next_line(&mut line)?;
    let mut words = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (words.next(), words.next(), words.next())
    else {
        return Err(HeadError::Bad(400, "malformed request line".to_string()));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(HeadError::Bad(
            505,
            "only HTTP/1.x is spoken here".to_string(),
        ));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut req = Request {
        method: method.to_string(),
        path: percent_decode(path, false),
        query: parse_query(query),
        headers: Vec::new(),
    };
    loop {
        next_line(&mut line)?;
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HeadError::Bad(400, "malformed header".to_string()));
        };
        req.headers
            .push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    if req.is_chunked() && req.content_length().is_some() {
        // RFC 9112 6.3: ambiguous framing must not be guessed at.
        return Err(HeadError::Bad(
            400,
            "both Content-Length and chunked".to_string(),
        ));
    }
    Ok(req)
}

/// The request's body, framed by its `Content-Length` or chunked encoding.
pub fn body<'a, R: BufRead + 'a>(req: &Request, r: &'a mut R) -> Box<dyn Read + 'a> {
    if req.is_chunked() {
        Box::new(Chunked::new(r))
    } else {
        Box::new(r.take(req.content_length().unwrap_or(0)))
    }
}

/// `application/x-www-form-urlencoded` or a query string, decoded.
pub fn parse_query(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect()
}

fn percent_decode(s: &str, plus_is_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' if plus_is_space => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A chunked request body, decoded.
struct Chunked<R> {
    r: R,
    /// Bytes left in the current chunk.
    left: u64,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    fn new(r: R) -> Self {
        Self {
            r,
            left: 0,
            done: false,
        }
    }

    fn line(&mut self) -> io::Result<String> {
        let mut line = String::new();
        self.r.by_ref().take(4096).read_line(&mut line)?;
        if !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed chunked body",
            ));
        }
        Ok(line.trim_end().to_string())
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or("").trim();
            self.left = u64::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "malformed chunk size"))?;
            if self.left == 0 {
                // Trailers, if any, up to the empty line.
                while !self.line()?.is_empty() {}
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left as usize);
        let n = self.r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        if self.left == 0 && !self.line()?.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk not followed by CRLF",
            ));
        }
        Ok(n)
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        507 => "Insufficient Storage",
        _ => "",
    }
}

fn head<W: Write>(w: &mut W, status: u16, headers: &[(&str, &str)]) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Connection: close\r\nCache-Control: no-store\r\n\r\n");
    w.write_all(out.as_bytes())
}

/// Send a whole response.
pub fn respond<W: Write>(
    w: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> io::Result<()> {
    let len = body.len().to_string();
    let mut headers = vec![("Content-Type", content_type), ("Content-Length", &len)];
    headers.extend_from_slice(extra);
    head(w, status, &headers)?;
    w.write_all(body)?;
    w.flush()
}

/// Tell a client waiting on `Expect: 100-continue` to send its body.
pub fn send_continue<W: Write>(w: &mut W) -> io::Result<()> {
    w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    w.flush()
}

/// Start a chunked response, for a body that is produced as it is sent.
pub fn stream<W: Write>(w: W, status: u16, content_type: &str) -> io::Result<ChunkedWriter<W>> {
    let mut w = w;
    head(
        &mut w,
        status,
        &[
            ("Content-Type", content_type),
            ("Transfer-Encoding", "chunked"),
            // Ask any proxy not to sit on the stream; there should be none.
            ("X-Accel-Buffering", "no"),
        ],
    )?;
    Ok(ChunkedWriter { w })
}

/// The body of a chunked response. Every `write` is sent as one chunk at once,
/// so a log line reaches the client when it is logged.
pub struct ChunkedWriter<W: Write> {
    w: W,
}

impl<W: Write> ChunkedWriter<W> {
    /// Send the terminating chunk.
    pub fn finish(mut self) -> io::Result<()> {
        self.w.write_all(b"0\r\n\r\n")?;
        self.w.flush()
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut chunk = format!("{:x}\r\n", buf.len()).into_bytes();
        chunk.extend_from_slice(buf);
        chunk.extend_from_slice(b"\r\n");
        self.w.write_all(&chunk)?;
        self.w.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Request, HeadError> {
        read_head(&mut io::BufReader::new(raw.as_bytes()))
    }

    #[test]
    fn reads_a_request_head() {
        let req = parse(
            "PUT /install?target=%2Fdev%2Fsda&profiles=Desktop+Router HTTP/1.1\r\n\
             Host: 192.168.241.1\r\nAccept: */*\r\nExpect: 100-continue\r\n\
             Content-Length: 12\r\n\r\nbody",
        )
        .unwrap();
        assert_eq!(req.method, "PUT");
        assert_eq!(req.path, "/install");
        assert_eq!(req.param("target"), Some("/dev/sda"));
        assert_eq!(req.param("profiles"), Some("Desktop Router"));
        assert_eq!(req.content_length(), Some(12));
        assert!(req.expects_continue());
        assert!(!req.wants_html());
        assert!(req.has_body());
    }

    #[test]
    fn tells_a_browser_from_a_client() {
        let browser =
            parse("GET / HTTP/1.1\r\nAccept: text/html,application/xhtml+xml,*/*;q=0.8\r\n\r\n")
                .unwrap();
        assert!(browser.wants_html());
        let script = parse("POST /install HTTP/1.1\r\nAccept: text/plain\r\n\r\n").unwrap();
        assert!(!script.wants_html());
    }

    #[test]
    fn refuses_bad_heads() {
        assert!(matches!(parse(""), Err(HeadError::Closed)));
        assert!(matches!(
            parse("GET /\r\n\r\n"),
            Err(HeadError::Bad(400, _))
        ));
        assert!(matches!(
            parse("GET / HTTP/2\r\n\r\n"),
            Err(HeadError::Bad(505, _))
        ));
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(20000));
        assert!(matches!(parse(&huge), Err(HeadError::Bad(431, _))));
        assert!(matches!(
            parse("POST / HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(HeadError::Bad(400, _))
        ));
    }

    #[test]
    fn finds_the_multipart_boundary() {
        let req = parse(
            "POST /install HTTP/1.1\r\nContent-Type: multipart/form-data; \
             boundary=\"----x123\"\r\n\r\n",
        )
        .unwrap();
        assert_eq!(req.multipart_boundary().as_deref(), Some("----x123"));
        let raw =
            parse("POST /install HTTP/1.1\r\nContent-Type: application/zstd\r\n\r\n").unwrap();
        assert_eq!(raw.multipart_boundary(), None);
    }

    #[test]
    fn decodes_percent_and_plus() {
        assert_eq!(
            parse_query("a=1%202&b=x+y&c&d=%zz&e=%4"),
            [
                ("a".to_string(), "1 2".to_string()),
                ("b".to_string(), "x y".to_string()),
                ("c".to_string(), String::new()),
                ("d".to_string(), "%zz".to_string()),
                ("e".to_string(), "%4".to_string()),
            ]
        );
    }

    #[test]
    fn reads_a_chunked_body() {
        let raw = "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
                   5\r\nhello\r\n7;ext=1\r\n, world\r\n0\r\nTrailer: x\r\n\r\nNEXT";
        let mut r = io::BufReader::new(raw.as_bytes());
        let req = read_head(&mut r).unwrap();
        let mut out = String::new();
        body(&req, &mut r).read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello, world");
        // The reader stops at the end of the body, not the stream.
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "NEXT");
    }

    #[test]
    fn writes_a_chunked_response() {
        let mut out = Vec::new();
        {
            let mut w = stream(&mut out, 200, "text/plain").unwrap();
            w.write_all(b"line one\n").unwrap();
            w.write_all(b"").unwrap();
            w.finish().unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Transfer-Encoding: chunked\r\n"));
        assert!(text.ends_with("\r\n\r\n9\r\nline one\n\r\n0\r\n\r\n"));
    }
}
