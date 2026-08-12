//! One place for reading bytes: HTTP(S) from the image/update servers, or a
//! local file. Also the SHA-256 primitives used to check artifacts against the
//! digests published in a build manifest.
//!
//! Everything that pulls an artifact — the catalog, the Btrfs layout probe, the
//! staging pass and the install pipeline — goes through here, so timeouts,
//! connection reuse and mid-transfer resume are configured once.

use std::io::{self, Read};
use std::sync::OnceLock;
use std::time::Duration;

use crate::core::model::Source;

pub type Result<T> = std::result::Result<T, String>;

/// Manifests and layout files are small; refuse to buffer more than this so a
/// misconfigured URL can't exhaust memory on a device with no swap.
const MAX_SMALL_READ: u64 = 32 * 1024 * 1024;

/// How long to wait for a connection and for each individual read. There is
/// deliberately *no* whole-request deadline: a profile pack is ~1 GiB and a slow
/// but healthy link must not be killed partway through, which would leave an
/// already-wiped target unusable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times a dropped body is resumed with a ranged request before the
/// transfer is failed for good.
const MAX_RESUMES: u8 = 4;

/// Shared HTTP agent, so repeated manifest fetches reuse the connection and the
/// TLS session instead of paying a fresh handshake each time.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .build()
    })
}

/// Whether a location is an HTTP(S) URL rather than a filesystem path.
pub fn is_url(location: &str) -> bool {
    location.starts_with("http://") || location.starts_with("https://")
}

/// Open a byte stream for `location`, interpreted according to `source`: an HTTP
/// GET for [`Source::Server`], or the local file for media and unpacked bundles.
pub fn open(location: &str, source: &Source) -> Result<Box<dyn Read + Send>> {
    match source {
        Source::Server => Ok(Box::new(ResumingReader::start(location)?)),
        Source::Removable { .. } | Source::Local { .. } => open_file(location),
    }
}

/// Open a byte stream for `location`, deciding from the location itself. For
/// callers that have a URL-or-path string but no [`Source`] to go with it.
pub fn open_at(location: &str) -> Result<Box<dyn Read + Send>> {
    if is_url(location) {
        Ok(Box::new(ResumingReader::start(location)?))
    } else {
        open_file(location)
    }
}

fn open_file(path: &str) -> Result<Box<dyn Read + Send>> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    Ok(Box::new(file))
}

/// Read a small object (a manifest, a layout file) fully into memory.
pub fn read_all(location: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    open_at(location)?
        .take(MAX_SMALL_READ)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {location}: {e}"))?;
    Ok(buf)
}

/// Read and deserialize a small JSON object.
pub fn json<T: serde::de::DeserializeOwned>(location: &str) -> Result<T> {
    let bytes = read_all(location)?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {location}: {e}"))
}

/// Read a small object as UTF-8 text.
pub fn read_text(location: &str) -> Result<String> {
    let bytes = read_all(location)?;
    String::from_utf8(bytes).map_err(|e| format!("decode {location}: {e}"))
}

/// An HTTP response body that resumes itself.
///
/// A dropped connection partway through a multi-hundred-megabyte artifact would
/// otherwise fail an install that has already wiped the target, so on a read
/// error we re-issue the request with `Range: bytes=<offset>-` and carry on. The
/// server must answer `206`; a `200` would restart the body from zero and splice
/// duplicate bytes into the stream, so that is treated as a failure.
struct ResumingReader {
    url: String,
    inner: Box<dyn Read + Send>,
    read: u64,
    resumes: u8,
}

impl ResumingReader {
    fn start(url: &str) -> Result<Self> {
        let resp = agent()
            .get(url)
            .call()
            .map_err(|e| format!("GET {url}: {e}"))?;
        Ok(Self {
            url: url.to_string(),
            inner: Box::new(resp.into_reader()),
            read: 0,
            resumes: 0,
        })
    }

    /// Re-open the body from `self.read` bytes in. Returns an error if the
    /// server won't serve the range.
    fn resume(&mut self) -> Result<()> {
        let resp = agent()
            .get(&self.url)
            .set("Range", &format!("bytes={}-", self.read))
            .call()
            .map_err(|e| format!("resume {} at {}: {e}", self.url, self.read))?;
        if resp.status() != 206 {
            return Err(format!(
                "resume {} at {}: server ignored the range request (HTTP {})",
                self.url,
                self.read,
                resp.status()
            ));
        }
        self.inner = Box::new(resp.into_reader());
        Ok(())
    }
}

impl Read for ResumingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Ok(n) => {
                    self.read += n as u64;
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if self.resumes >= MAX_RESUMES {
                        return Err(io::Error::other(format!(
                            "{} failed after {} byte(s) and {} resume(s): {e}",
                            self.url, self.read, self.resumes
                        )));
                    }
                    self.resumes += 1;
                    self.resume().map_err(io::Error::other)?;
                }
            }
        }
    }
}

// --- digests ---------------------------------------------------------------

/// Incremental SHA-256. Thin wrapper over `ring::digest`, which is already
/// linked in for TLS and uses the ARMv8 SHA-2 extensions on the RK3576. Keeping
/// every hashing site behind this type means switching implementations later is
/// a change in one file.
pub struct Sha256 {
    ctx: ring::digest::Context,
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            ctx: ring::digest::Context::new(&ring::digest::SHA256),
        }
    }

    pub fn update(&mut self, buf: &[u8]) {
        self.ctx.update(buf);
    }

    /// Lowercase hex of the digest, consuming the state.
    pub fn finish_hex(self) -> String {
        let digest = self.ctx.finish();
        let mut out = String::with_capacity(64);
        for b in digest.as_ref() {
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// Read adapter that feeds everything read into a caller-owned digest.
///
/// The digests published in a manifest cover the artifact *as stored*, i.e. the
/// compressed pack, so this wraps the raw source and sits underneath any zstd
/// decoder — the same position [`crate::core::install`]'s progress reader takes.
pub struct Digesting<'a, R> {
    pub inner: R,
    pub sha: &'a mut Sha256,
}

impl<R: Read> Read for Digesting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.sha.update(&buf[..n]);
        }
        Ok(n)
    }
}

/// Outcome of checking a computed digest against a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The digest matched the one published for the artifact.
    Match,
    /// No digest was published, so nothing could be checked.
    Unpublished,
    /// The digest did not match: `(computed, expected)`.
    Mismatch(String, String),
}

impl Verdict {
    /// A line describing the outcome, for the activity log. `None` when there is
    /// nothing worth saying (a plain match).
    pub fn message(&self, what: &str) -> Option<String> {
        match self {
            Verdict::Match => None,
            Verdict::Unpublished => Some(format!(
                "no sha256 published for {what}; verification skipped"
            )),
            Verdict::Mismatch(got, want) => {
                Some(format!("sha256 mismatch for {what}: got {got}, want {want}"))
            }
        }
    }
}

/// Compare a computed digest against the expected one from a manifest,
/// case-insensitively. `expected` of `None` means the manifest published none.
pub fn verify(computed: Sha256, expected: Option<&str>) -> Verdict {
    let got = computed.finish_hex();
    match expected {
        None => Verdict::Unpublished,
        Some(want) if want.eq_ignore_ascii_case(&got) => Verdict::Match,
        Some(want) => Verdict::Mismatch(got, want.to_ascii_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_known_answer() {
        let mut sha = Sha256::new();
        sha.update(b"abc");
        assert_eq!(
            sha.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn digesting_reader_matches_one_shot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();

        let mut one_shot = Sha256::new();
        one_shot.update(&data);
        let expected = one_shot.finish_hex();

        // Read through the adapter in awkward chunks.
        let mut streamed = Sha256::new();
        let mut reader = Digesting {
            inner: &data[..],
            sha: &mut streamed,
        };
        let mut buf = [0u8; 7];
        loop {
            match reader.read(&mut buf).unwrap() {
                0 => break,
                _ => continue,
            }
        }
        assert_eq!(streamed.finish_hex(), expected);
    }

    #[test]
    fn verify_is_case_insensitive_and_reports() {
        let mut sha = Sha256::new();
        sha.update(b"abc");
        let upper = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        assert_eq!(verify(sha, Some(upper)), Verdict::Match);

        let mut sha = Sha256::new();
        sha.update(b"abc");
        let v = verify(sha, Some("00"));
        assert!(matches!(v, Verdict::Mismatch(_, _)));
        let msg = v.message("Minimal (full)").expect("a mismatch has a message");
        assert!(msg.contains("Minimal (full)"), "{msg}");
        assert!(msg.contains("ba7816bf"), "{msg}");

        let sha = Sha256::new();
        assert_eq!(verify(sha, None), Verdict::Unpublished);
        assert!(Verdict::Match.message("x").is_none());
    }

    #[test]
    fn recognises_urls() {
        assert!(is_url("https://example.invalid/a"));
        assert!(is_url("http://example.invalid/a"));
        assert!(!is_url("/mnt/sd/manifest.json"));
    }
}
