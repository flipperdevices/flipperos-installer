// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! The web frontend: install over a USB cable from a browser or a script.
//!
//! It brings up the USB link (see [`crate::net`]) and serves, on the board's
//! address there:
//!
//! | Path | What |
//! |---|---|
//! | `GET /` | the installation form |
//! | `POST`/`PUT /install` | upload a bundle and install it, see [`upload`] |
//! | `GET /log` | the activity log, following a running install |
//! | `GET /status` | the installer's state, as JSON |
//! | `POST /reboot` | reboot into the installed system |
//!
//! A browser (which asks for `text/html`) gets pages; anything else gets plain
//! text or JSON from the very same paths, so a client app needs to know no more
//! than the form's field names. Pages work without scripts; `app.js` adds an
//! upload progress bar and nothing else of substance.
//!
//! Like the other two frontends, this one only drives the shared
//! [`Controller`]: an install started here shows on the device's screen and
//! the serial console, and one started there shows here.

pub mod http;
mod hub;
mod multipart;
mod pages;
mod upload;

use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;

use crate::core::model::{AppState, Phase};
use crate::core::Controller;
use crate::net::{self, Subnet};

use hub::Hub;

const STYLE: &str = include_str!("assets/style.css");
const SCRIPT: &str = include_str!("assets/app.js");

/// Connections served at once. A browser opens a few; more than this on a
/// point-to-point cable means something is wrong.
const MAX_CONNECTIONS: usize = 8;

/// How long a connection may sit idle in either direction. Bounds how long a
/// stalled upload holds the installer in its Receiving phase.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// How often an idle log stream checks that its client is still there.
const HANGUP_CHECK: Duration = Duration::from_secs(5);

/// Where and how to serve.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Serve here instead of on the USB link, and leave the USB port alone. For
    /// installing over Ethernet, or running the installer on a development host.
    pub listen: Option<SocketAddr>,
    /// The USB device controller to bind the gadget to; the only one if `None`.
    pub udc: Option<String>,
    /// The USB link's addressing.
    pub subnet: Subnet,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: None,
            udc: None,
            subnet: Subnet::DEFAULT,
        }
    }
}

/// What every connection shares.
struct Ctx {
    ctrl: Arc<Controller>,
    hub: Arc<Hub>,
    active: AtomicUsize,
    stop: AtomicBool,
}

/// The running server.
pub struct Running {
    pub addr: SocketAddr,
    accept: JoinHandle<()>,
}

impl Running {
    /// Block until the server stops, which it does when the installer closes
    /// its frontends (see [`Controller::request_reboot`]).
    pub fn wait(self) {
        let _ = self.accept.join();
    }
}

/// Bring up the USB link (unless told to listen elsewhere) and start serving
/// on a thread of its own.
pub fn start(ctrl: &Arc<Controller>, settings: Settings) -> Result<Running, String> {
    let addr = match settings.listen {
        Some(addr) => addr,
        None => {
            let log_ctrl = Arc::clone(ctrl);
            let ifname = net::start(
                settings.udc.as_deref(),
                settings.subnet,
                Arc::new(move |line| log_ctrl.log(line)),
            )
            .map_err(|e| format!("usb: {e}"))?;
            ctrl.log(format!(
                "usb: {ifname} is up as {}; the host gets {} over DHCP",
                settings.subnet,
                settings.subnet.host()
            ));
            SocketAddr::from((settings.subnet.addr, 80))
        }
    };
    let listener = TcpListener::bind(addr).map_err(|e| format!("web: listen on {addr}: {e}"))?;
    let addr = listener.local_addr().unwrap_or(addr);

    let ctx = Arc::new(Ctx {
        ctrl: Arc::clone(ctrl),
        hub: Hub::new(ctrl),
        active: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
    });
    {
        let ctx = Arc::clone(&ctx);
        ctrl.on_exit(move || {
            ctx.stop.store(true, Ordering::SeqCst);
            // Wake the accept loop so it sees the flag.
            let _ = TcpStream::connect_timeout(&addr, Duration::from_secs(1));
        });
    }
    let accept = std::thread::Builder::new()
        .name("web".to_string())
        .spawn(move || accept_loop(ctx, listener))
        .map_err(|e| format!("web: start: {e}"))?;
    ctrl.log(format!("web: serving http://{addr}/"));
    Ok(Running { addr, accept })
}

fn accept_loop(ctx: Arc<Ctx>, listener: TcpListener) {
    for stream in listener.incoming() {
        if ctx.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut stream) = stream else { continue };
        if ctx.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            ctx.active.fetch_sub(1, Ordering::SeqCst);
            let _ = http::respond(
                &mut stream,
                503,
                "text/plain; charset=utf-8",
                b"error: too many connections\n",
                &[],
            );
            continue;
        }
        let conn_ctx = Arc::clone(&ctx);
        let spawned = std::thread::Builder::new()
            .name("web-conn".to_string())
            .spawn(move || {
                serve(&conn_ctx, stream);
                conn_ctx.active.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            ctx.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Answer the one request a connection carries.
fn serve(ctx: &Ctx, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::with_capacity(64 * 1024, read_half);
    let mut w = stream;
    let req = match http::read_head(&mut reader) {
        Ok(req) => req,
        Err(http::HeadError::Closed) => return,
        Err(http::HeadError::Bad(status, msg)) => {
            let _ = plain(&mut w, status, &format!("error: {msg}\n"));
            return;
        }
    };
    // Write errors mean the client left; there is nobody to tell.
    let _ = route(ctx, &req, &mut reader, &mut w);
}

fn route(
    ctx: &Ctx,
    req: &http::Request,
    reader: &mut BufReader<TcpStream>,
    w: &mut TcpStream,
) -> io::Result<()> {
    let html = req.wants_html();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => {
            let page = pages::index(
                &ctx.hub.snapshot(),
                &pages::FormValues::default(),
                None,
                &host(req, w),
            );
            http::respond(w, 200, HTML, page.as_bytes(), &[])
        }
        ("GET", "/style.css") => {
            http::respond(w, 200, "text/css; charset=utf-8", STYLE.as_bytes(), &[])
        }
        ("GET", "/app.js") => http::respond(
            w,
            200,
            "text/javascript; charset=utf-8",
            SCRIPT.as_bytes(),
            &[],
        ),
        ("POST" | "PUT", "/install") => upload::handle(ctx, req, reader, w),
        ("GET", "/log") => log(ctx, req, w),
        ("GET", "/status") => {
            let body = status(&ctx.hub.snapshot()).to_string() + "\n";
            http::respond(w, 200, "application/json", body.as_bytes(), &[])
        }
        ("POST", "/reboot") => reboot(ctx, req, reader, w),
        (_, path) => {
            let allow = match path {
                "/" | "/style.css" | "/app.js" | "/log" | "/status" => Some("GET"),
                "/install" => Some("POST, PUT"),
                "/reboot" => Some("POST"),
                _ => None,
            };
            let (status, msg) = match allow {
                Some(_) => (405, format!("{} is not allowed here", req.method)),
                None => (404, format!("there is nothing at {path}")),
            };
            let extra: Vec<(&str, &str)> = allow.map(|a| ("Allow", a)).into_iter().collect();
            if html {
                let page = pages::message(&ctx.hub.snapshot(), http::reason(status), &msg);
                http::respond(w, status, HTML, page.as_bytes(), &extra)
            } else {
                http::respond(
                    w,
                    status,
                    TEXT,
                    format!("error: {msg}\n").as_bytes(),
                    &extra,
                )
            }
        }
    }
}

const HTML: &str = "text/html; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";

fn plain<W: Write>(w: &mut W, status: u16, body: &str) -> io::Result<()> {
    http::respond(w, status, TEXT, body.as_bytes(), &[])
}

/// The address the client reached us at, for the examples on the form.
fn host(req: &http::Request, w: &TcpStream) -> String {
    req.header("host")
        .map(str::to_string)
        .or_else(|| w.local_addr().ok().map(|a| a.to_string()))
        .unwrap_or_default()
}

/// `GET /log[?since=N][&follow=1]`
fn log(ctx: &Ctx, req: &http::Request, w: &mut TcpStream) -> io::Result<()> {
    let since = req.param("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    let follow = matches!(req.param("follow"), Some("1" | "true" | "yes"));
    let until = if follow && !req.wants_html() {
        Until::Forever
    } else {
        Until::RunEnds
    };
    if req.wants_html() {
        let conn = w.try_clone()?;
        let mut out = http::stream(&mut *w, 200, HTML)?;
        out.write_all(pages::log_open(&ctx.hub.snapshot()).as_bytes())?;
        let end = follow_log(ctx, &mut out, since, Format::Html, until, &conn)?;
        out.write_all(pages::log_close(&end).as_bytes())?;
        out.finish()
    } else {
        let conn = w.try_clone()?;
        let mut out = http::stream(&mut *w, 200, TEXT)?;
        follow_log(ctx, &mut out, since, Format::Plain, until, &conn)?;
        out.finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Until {
    /// Until nothing is running: the end of an install, or at once when idle.
    RunEnds,
    /// Until the client hangs up.
    Forever,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Plain,
    Html,
}

/// Send the log lines after `seq`, then each new one as it is logged, until
/// `until`. Returns the state it stopped at.
fn follow_log<W: Write>(
    ctx: &Ctx,
    out: &mut W,
    mut seq: u64,
    format: Format,
    until: Until,
    conn: &TcpStream,
) -> io::Result<Arc<AppState>> {
    let mut state = ctx.hub.snapshot();
    loop {
        let (dropped, lines) = state.log_since(seq);
        let mut chunk = String::new();
        if dropped > 0 {
            let note = format!("[{dropped} earlier line(s) are no longer kept]");
            chunk.push_str(&match format {
                Format::Plain => note + "\n",
                Format::Html => pages::log_line(&note),
            });
        }
        for line in lines {
            match format {
                Format::Plain => {
                    chunk.push_str(line);
                    chunk.push('\n');
                }
                Format::Html => chunk.push_str(&pages::log_line(line)),
            }
        }
        if !chunk.is_empty() {
            out.write_all(chunk.as_bytes())?;
        }
        seq = state.log_seq;
        if until == Until::RunEnds && !state.phase.is_busy() {
            return Ok(state);
        }
        if hung_up(conn) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        state = ctx.hub.wait(seq, state.phase.is_busy(), HANGUP_CHECK);
    }
}

/// Whether the client closed its end. A stream that only writes would
/// otherwise not notice until the next line is logged, which, on an idle
/// installer, may be never.
fn hung_up(conn: &TcpStream) -> bool {
    let mut fd = libc::pollfd {
        fd: conn.as_raw_fd(),
        events: libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: one valid pollfd, no waiting.
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

/// The last line of a plain-text install log.
fn status_line(state: &AppState) -> String {
    match &state.phase {
        Phase::Done => "status: done\n".to_string(),
        Phase::Failed(e) => format!("status: failed: {e}\n"),
        other => format!("status: {}\n", other.label()),
    }
}

/// `GET /status`
fn status(s: &AppState) -> serde_json::Value {
    let phase = match s.phase {
        Phase::Discovering => "discovering",
        Phase::Searching => "searching",
        Phase::Ready => "ready",
        Phase::Provisioning => "provisioning",
        Phase::Receiving => "receiving",
        Phase::Installing => "installing",
        Phase::Done => "done",
        Phase::Failed(_) => "failed",
    };
    let error = match &s.phase {
        Phase::Failed(e) => Some(e.as_str()),
        _ => None,
    };
    json!({
        "phase": phase,
        "phase_label": s.phase.label(),
        "error": error,
        "progress": s.progress,
        "dry_run": s.dry_run,
        "board": { "id": s.board.board_id, "model": s.board.model },
        "mcu": s.mcu.as_ref().map(|m| json!({ "version": m.version, "target": m.target })),
        "devices": s.devices.iter().map(|d| json!({
            "path": d.path,
            "kind": d.kind.as_str(),
            "model": d.model.trim(),
            "size_bytes": d.size_bytes,
            "removable": d.removable,
        })).collect::<Vec<_>>(),
        "target": s.selection.target_device,
        "bundle": s.bundle.as_ref().map(|b| json!({
            "name": b.name,
            "version": b.version,
            "channel": b.channel,
        })),
        "profiles": s.selection.profiles,
        "log_seq": s.log_seq,
        "can_install": s.can_install(),
        "can_reboot": s.can_reboot(),
    })
}

/// `POST /reboot`
fn reboot(
    ctx: &Ctx,
    req: &http::Request,
    reader: &mut BufReader<TcpStream>,
    w: &mut TcpStream,
) -> io::Result<()> {
    // A form post carries an (empty) body; read it so closing does not reset
    // the connection under the response.
    let _ = io::copy(
        &mut http::body(req, reader).take(64 * 1024),
        &mut io::sink(),
    );
    let state = ctx.hub.snapshot();
    let html = req.wants_html();
    if !state.can_reboot() {
        let why = if state.dry_run {
            "This was a dry run: there is no installed system to boot into."
        } else {
            "Reboot is offered once an installation has finished."
        };
        return if html {
            let page = pages::message(&state, "Not now", why);
            http::respond(w, 409, HTML, page.as_bytes(), &[])
        } else {
            plain(w, 409, &format!("error: {why}\n"))
        };
    }
    if html {
        let page = pages::message(
            &state,
            "Rebooting",
            "The device is restarting into the installed system. This page will not \
             come back: the installer is gone once it has.",
        );
        http::respond(w, 200, HTML, page.as_bytes(), &[])?;
    } else {
        plain(w, 200, "rebooting\n")?;
    }
    ctx.ctrl.request_reboot();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{StorageDevice, StorageKind};
    use crate::core::Config;

    /// A small bundle for the generic board, as `tar.zst` bytes.
    fn bundle(manifest_last: bool) -> Vec<u8> {
        let files: [(&str, &[u8]); 5] = [
            ("u-boot/generic/u-boot-rockchip.bin", b"uboot"),
            ("u-boot/evb/u-boot-rockchip.bin", b"someone else's"),
            ("profile-packs/Minimal_9_stock_pack.zst", b"minimal"),
            ("profile-packs/Desktop_9_stock_inc_pack.zst", b"desktop"),
            ("profile-packs/home_9_pack.zst", b"home"),
        ];
        let sha = |b: &[u8]| {
            let mut s = crate::core::fetch::Sha256::new();
            s.update(b);
            s.finish_hex()
        };
        let manifest = json!({
            "schema": 1,
            "bundle": { "name": "test", "version": "1" },
            "files": files.iter().map(|(p, b)| json!({
                "path": p, "size": b.len(), "sha256": sha(b),
            })).collect::<Vec<_>>(),
        })
        .to_string();
        let mut members: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(p, b)| (format!("b/{p}"), b.to_vec()))
            .collect();
        let m = ("b/manifest.json".to_string(), manifest.into_bytes());
        if manifest_last {
            members.push(m);
        } else {
            members.insert(0, m);
        }
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (name, body) in &members {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, &body[..]).unwrap();
            }
            builder.finish().unwrap();
        }
        zstd::stream::encode_all(&tar_bytes[..], 1).unwrap()
    }

    /// A dry-run installer with one eMMC, served on a free local port.
    fn server(name: &str) -> (Arc<Controller>, SocketAddr) {
        let cache = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&cache);
        let ctrl = Controller::new(Config {
            automount: false,
            ufs_check: false,
            cache_dir: cache.to_string_lossy().into_owned(),
            ..Config::default()
        });
        ctrl.update_for_test(|s| {
            s.phase = Phase::Ready;
            s.devices = vec![StorageDevice {
                path: "/dev/fake-emmc".into(),
                kind: StorageKind::Emmc,
                model: "fake".into(),
                size_bytes: 32 << 30,
                removable: false,
                logical_block_size: 512,
            }];
        });
        let running = start(
            &ctrl,
            Settings {
                listen: Some("127.0.0.1:0".parse().unwrap()),
                ..Settings::default()
            },
        )
        .unwrap();
        (ctrl, running.addr)
    }

    /// Send one request and read the whole response.
    fn exchange(addr: SocketAddr, head: &str, body: &[u8]) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The body of a chunked response, decoded.
    fn dechunk(response: &str) -> String {
        let body = response.split_once("\r\n\r\n").unwrap().1;
        let mut r = io::BufReader::new(body.as_bytes());
        let req = http::Request {
            headers: vec![("transfer-encoding".into(), "chunked".into())],
            ..http::Request::default()
        };
        let mut out = String::new();
        http::body(&req, &mut r).read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn a_client_uploads_and_follows_the_install() {
        let (ctrl, addr) = server("flipperos-web-raw");
        let body = bundle(true);
        let response = exchange(
            addr,
            &format!(
                "PUT /install?target=/dev/fake-emmc&profiles=none HTTP/1.1\r\n\
                 Host: test\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            &body,
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let log = dechunk(&response);
        assert!(log.contains("upload: received"), "{log}");
        let last = log.trim_end().lines().last().unwrap();
        assert!(last.starts_with("status: "), "{log}");
        // Whatever the dry run made of the fake disk, the log said it before
        // the status line did.
        assert!(
            log.contains("installation complete") || log.contains("installation failed"),
            "{log}"
        );

        // Only Minimal was asked for, and the manifest came last: what arrived
        // before it and is not needed was pruned.
        let cache = std::path::PathBuf::from(&ctrl.config().cache_dir).join("upload");
        assert!(cache
            .join("profile-packs/Minimal_9_stock_pack.zst")
            .is_file());
        assert!(!cache
            .join("profile-packs/Desktop_9_stock_inc_pack.zst")
            .exists());
        assert!(!cache.join("u-boot/evb").exists());
        assert_eq!(ctrl.snapshot().selection.profiles, Vec::<String>::new());
    }

    #[test]
    fn a_browser_is_sent_to_the_log_page() {
        let (_ctrl, addr) = server("flipperos-web-form");
        let mut form = b"--B\r\nContent-Disposition: form-data; name=\"target\"\r\n\r\n\
            /dev/fake-emmc\r\n--B\r\nContent-Disposition: form-data; name=\"profiles\"\r\n\r\n\
            Desktop\r\n--B\r\nContent-Disposition: form-data; name=\"bundle\"; \
            filename=\"b.tar.zst\"\r\n\r\n"
            .to_vec();
        form.extend_from_slice(&bundle(false));
        form.extend_from_slice(b"\r\n--B--\r\n");
        let response = exchange(
            addr,
            &format!(
                "POST /install HTTP/1.1\r\nHost: test\r\nAccept: text/html\r\n\
                 Content-Type: multipart/form-data; boundary=B\r\nContent-Length: {}\r\n\r\n",
                form.len()
            ),
            &form,
        );
        assert!(response.starts_with("HTTP/1.1 303 "), "{response}");
        assert!(response.contains("\r\nLocation: /log?since="), "{response}");
    }

    #[test]
    fn a_refused_request_says_why() {
        let (_ctrl, addr) = server("flipperos-web-refused");
        // An unknown profile is only found once the manifest is in.
        let body = bundle(false);
        let response = exchange(
            addr,
            &format!(
                "PUT /install?target=/dev/fake-emmc&profiles=Gaming HTTP/1.1\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            ),
            &body,
        );
        assert!(response.starts_with("HTTP/1.1 422 "), "{response}");
        assert!(response.contains("error: this bundle has no profile 'Gaming'"));

        // A browser gets the form back, with the reason on it.
        let response = exchange(
            addr,
            "PUT /install?target=/dev/nope HTTP/1.1\r\nAccept: text/html\r\n\
             Expect: 100-continue\r\nContent-Length: 10\r\n\r\n",
            b"",
        );
        assert!(response.starts_with("HTTP/1.1 422 "), "{response}");
        assert!(response.contains("/dev/nope is not an installable device"));
        assert!(response.contains("<form"));

        let response = exchange(addr, "GET /nowhere HTTP/1.1\r\n\r\n", b"");
        assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
        let response = exchange(addr, "GET /install HTTP/1.1\r\n\r\n", b"");
        assert!(response.starts_with("HTTP/1.1 405 "), "{response}");
        assert!(response.contains("\r\nAllow: POST, PUT\r\n"));
    }

    #[test]
    fn reports_the_state_as_json() {
        let mut s = AppState {
            phase: Phase::Failed("disk on fire".into()),
            log_seq: 42,
            ..AppState::default()
        };
        let v = status(&s);
        assert_eq!(v["phase"], "failed");
        assert_eq!(v["error"], "disk on fire");
        assert_eq!(v["log_seq"], 42);
        assert_eq!(v["can_reboot"], false);
        assert_eq!(status_line(&s), "status: failed: disk on fire\n");
        s.phase = Phase::Done;
        assert_eq!(status_line(&s), "status: done\n");
    }
}
