// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! A streaming `multipart/form-data` reader.
//!
//! The form's file is a whole update bundle, gigabytes of it, so nothing here
//! holds more than a buffer's worth: each part's body is read straight through
//! up to the next boundary, and only the small text fields are collected.

use std::io::{self, Read};

/// Longest part header line accepted.
const MAX_HEADER_LINE: usize = 8 * 1024;
/// Most header lines one part may carry.
const MAX_HEADERS: usize = 32;
/// The read buffer. Larger than any boundary by far.
const BUF: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Part {
    /// The form field's name.
    pub name: String,
    /// The file name, for a file field.
    pub filename: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Inside a part's body (or the preamble before the first boundary).
    Body,
    /// Right after a boundary, before the next part's headers.
    Boundary,
    /// Past the closing boundary.
    Done,
}

pub struct Multipart<R> {
    r: R,
    /// `CRLF--boundary`: how a boundary appears after a part's body.
    delim: Vec<u8>,
    buf: Vec<u8>,
    start: usize,
    end: usize,
    eof: bool,
    state: State,
}

impl<R: Read> Multipart<R> {
    pub fn new(r: R, boundary: &str) -> Self {
        let mut delim = b"\r\n--".to_vec();
        delim.extend_from_slice(boundary.as_bytes());
        let mut buf = vec![0u8; BUF.max(delim.len() * 2)];
        // The first boundary opens the body with no CRLF before it; supplying
        // one lets every boundary be found the same way.
        buf[..2].copy_from_slice(b"\r\n");
        Self {
            r,
            delim,
            buf,
            start: 0,
            end: 2,
            eof: false,
            state: State::Body,
        }
    }

    /// Move to the next part, skipping whatever is left of the current one, and
    /// return its headers; `None` past the last part.
    pub fn next_part(&mut self) -> io::Result<Option<Part>> {
        let mut scratch = [0u8; 8192];
        while self.read_body(&mut scratch)? > 0 {}
        if self.state == State::Done {
            return Ok(None);
        }
        self.fill(2)?;
        if &self.buf[self.start..self.start + 2] == b"--" {
            self.state = State::Done;
            return Ok(None);
        }
        // The rest of the boundary line (transport padding, then CRLF).
        self.line()?;

        let mut part = Part::default();
        let mut found = false;
        for _ in 0..=MAX_HEADERS {
            let line = self.line()?;
            if line.is_empty() {
                if !found {
                    return Err(invalid("a part without Content-Disposition"));
                }
                self.state = State::Body;
                return Ok(Some(part));
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(invalid("malformed part header"));
            };
            if name.trim().eq_ignore_ascii_case("content-disposition") {
                found = true;
                for param in value.split(';').skip(1) {
                    let Some((k, v)) = param.trim().split_once('=') else {
                        continue;
                    };
                    let v = unquote(v.trim());
                    match k.trim().to_ascii_lowercase().as_str() {
                        "name" => part.name = v,
                        "filename" => part.filename = Some(v),
                        _ => {}
                    }
                }
            }
        }
        Err(invalid("too many part headers"))
    }

    /// Read the current part's body; `0` at its end.
    pub fn read_body(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.state != State::Body || out.is_empty() {
            return Ok(0);
        }
        loop {
            let avail = &self.buf[self.start..self.end];
            if let Some(pos) = find(avail, &self.delim) {
                if pos == 0 {
                    self.start += self.delim.len();
                    self.state = State::Boundary;
                    return Ok(0);
                }
                let n = pos.min(out.len());
                out[..n].copy_from_slice(&avail[..n]);
                self.start += n;
                return Ok(n);
            }
            // Everything before a CR that might begin a boundary not yet fully
            // read is certainly body.
            let tail = avail.len().saturating_sub(self.delim.len() - 1);
            let safe = avail[tail..]
                .iter()
                .position(|&b| b == b'\r')
                .map_or(avail.len(), |p| tail + p);
            if safe > 0 {
                let n = safe.min(out.len());
                out[..n].copy_from_slice(&avail[..n]);
                self.start += n;
                return Ok(n);
            }
            if self.eof {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the form ended inside a part",
                ));
            }
            self.refill()?;
        }
    }

    /// The current part's body as a string, for a text field of at most `max`
    /// bytes.
    pub fn read_text(&mut self, max: usize) -> io::Result<String> {
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = self.read_body(&mut buf)?;
            if n == 0 {
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            if out.len() + n > max {
                return Err(invalid("form field too long"));
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// The current part's body as a reader.
    pub fn body(&mut self) -> PartBody<'_, R> {
        PartBody { m: self }
    }

    /// Make at least `n` bytes available, or fail at the end of the input.
    fn fill(&mut self, n: usize) -> io::Result<()> {
        while self.end - self.start < n {
            if self.eof {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the form ended early",
                ));
            }
            self.refill()?;
        }
        Ok(())
    }

    /// One CRLF-terminated line, without its terminator.
    fn line(&mut self) -> io::Result<String> {
        loop {
            let avail = &self.buf[self.start..self.end];
            if let Some(pos) = find(avail, b"\r\n") {
                let line = String::from_utf8_lossy(&avail[..pos]).into_owned();
                self.start += pos + 2;
                return Ok(line);
            }
            if avail.len() > MAX_HEADER_LINE {
                return Err(invalid("part header line too long"));
            }
            if self.eof {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the form ended inside a part header",
                ));
            }
            self.refill()?;
        }
    }

    fn refill(&mut self) -> io::Result<()> {
        if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        if self.end == self.buf.len() {
            return Err(invalid("form line longer than the buffer"));
        }
        loop {
            match self.r.read(&mut self.buf[self.end..]) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(n) => {
                    self.end += n;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// One part's body, read through [`Multipart::read_body`].
pub struct PartBody<'a, R> {
    m: &'a mut Multipart<R>,
}

impl<R: Read> Read for PartBody<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.m.read_body(buf)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let first = *needle.first()?;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        let p = haystack[i..=haystack.len() - needle.len()]
            .iter()
            .position(|&b| b == first)?;
        i += p;
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn unquote(v: &str) -> String {
    match v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => inner.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => v.to_string(),
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out `step` bytes at a time, so boundaries straddle
    /// reads at every possible offset.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn form(file: &[u8]) -> Vec<u8> {
        let mut body = b"preamble to ignore\r\n--XyZ\r\n\
            Content-Disposition: form-data; name=\"target\"\r\n\r\n/dev/sda\r\n\
            --XyZ\r\n\
            Content-Disposition: form-data; name=\"profiles\"\r\n\r\nDesktop, Router\r\n\
            --XyZ  \r\n\
            Content-Disposition: form-data; name=\"bundle\"; filename=\"b \\\"1\\\".tar.zst\"\r\n\
            Content-Type: application/zstd\r\n\r\n"
            .to_vec();
        body.extend_from_slice(file);
        body.extend_from_slice(b"\r\n--XyZ--\r\nepilogue");
        body
    }

    /// File content full of near-misses: CRs, CRLFs, dashes and boundaries
    /// missing their last character. (A form's boundary never occurs in its
    /// content; that is how the client chose it.)
    fn tricky_file() -> Vec<u8> {
        let mut f = Vec::new();
        for i in 0..5000u32 {
            f.extend_from_slice(&i.to_le_bytes());
            f.extend_from_slice(b"\r\n--XyY-\r\r\n--Xy\r\r\n-");
        }
        f
    }

    fn read_all(step: usize) -> (Vec<(Part, Vec<u8>)>, usize) {
        let file = tricky_file();
        let data = form(&file);
        let mut m = Multipart::new(Trickle { data: &data, step }, "XyZ");
        let mut parts = Vec::new();
        while let Some(part) = m.next_part().unwrap() {
            let mut body = Vec::new();
            m.body().read_to_end(&mut body).unwrap();
            parts.push((part, body));
        }
        (parts, file.len())
    }

    #[test]
    fn splits_a_form_at_every_read_size() {
        for step in [1, 2, 3, 7, 64, 4096, 1 << 20] {
            let (parts, file_len) = read_all(step);
            assert_eq!(parts.len(), 3, "step {step}");
            assert_eq!(parts[0].0.name, "target");
            assert_eq!(parts[0].1, b"/dev/sda");
            assert_eq!(parts[1].0.name, "profiles");
            assert_eq!(parts[1].1, b"Desktop, Router");
            assert_eq!(parts[2].0.name, "bundle");
            assert_eq!(parts[2].0.filename.as_deref(), Some("b \"1\".tar.zst"));
            assert_eq!(parts[2].1.len(), file_len, "step {step}");
            assert_eq!(parts[2].1, tricky_file(), "step {step}");
        }
    }

    #[test]
    fn reads_text_fields_and_skips_unread_bodies() {
        let data = form(b"payload");
        let mut m = Multipart::new(&data[..], "XyZ");
        m.next_part().unwrap().unwrap();
        assert_eq!(m.read_text(100).unwrap(), "/dev/sda");
        // Profiles is never read; moving on skips it.
        assert_eq!(m.next_part().unwrap().unwrap().name, "profiles");
        assert_eq!(m.next_part().unwrap().unwrap().name, "bundle");
        assert!(m.next_part().unwrap().is_none());
        assert!(m.next_part().unwrap().is_none());

        let mut m = Multipart::new(&data[..], "XyZ");
        m.next_part().unwrap();
        assert!(m.read_text(3).is_err());
    }

    #[test]
    fn a_truncated_form_is_an_error() {
        let data = form(b"payload");
        let cut = &data[..data.len() - 20];
        let mut m = Multipart::new(cut, "XyZ");
        for _ in 0..2 {
            m.next_part().unwrap();
        }
        m.next_part().unwrap();
        let mut sink = Vec::new();
        let err = m.body().read_to_end(&mut sink).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
