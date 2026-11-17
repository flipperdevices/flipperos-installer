// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! `POST`/`PUT /install`: receive a bundle and install it.
//!
//! The request carries the settings — `target`, `profiles`, `ufs` — and the
//! bundle, either as the fields of the page's form (`multipart/form-data`, the
//! bundle last) or as a raw `tar.zst` body with the settings in the query
//! string. Either way the bundle is never stored: it is unpacked as it arrives,
//! keeping only what this board and these profiles need and checking every file
//! against the manifest, so a 2 GB upload costs no more RAM than what it
//! installs (see [`archive::unpack_stream`]).
//!
//! Then the install starts, and the response is its log: a redirect to the log
//! page for a browser, or the log itself as plain text for anything else,
//! ending with `status: done` or `status: failed: …`. A request refused before
//! that gets a 4xx and `error: …`.

use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use crate::core::archive;
use crate::core::bundle::{self, Manifest};
use crate::core::controller::{InstallRequest, Receiving};
use crate::core::model::{human_bytes, AppState, BundleLocation, BundleRef, Source};
use crate::core::stage;

use super::multipart::Multipart;
use super::pages::{self, FormValues};
use super::{follow_log, http, status_line, Ctx, Format, Until, HTML, TEXT};

/// Log the upload's progress every this many bytes.
const PROGRESS_STEP: u64 = 100 * 1024 * 1024;

/// Longest text field accepted.
const MAX_FIELD: usize = 4096;

/// Why a request was not taken.
#[derive(Debug)]
struct Refusal {
    status: u16,
    msg: String,
}

impl Refusal {
    fn new(status: u16, msg: impl Into<String>) -> Self {
        Self {
            status,
            msg: msg.into(),
        }
    }
}

/// A bundle received and unpacked, with the installer still held for it.
struct Received<'a> {
    _guard: Receiving<'a>,
    dest: PathBuf,
    profiles: Vec<String>,
}

pub(super) fn handle<R: BufRead>(
    ctx: &Ctx,
    req: &http::Request,
    reader: &mut R,
    w: &mut TcpStream,
) -> io::Result<()> {
    let html = req.wants_html();
    // Everything logged from here on belongs to this request's story.
    let first_line = ctx.hub.snapshot().log_seq;

    let mut values = FormValues::default();
    for (k, v) in &req.query {
        if let Err(r) = set_field(&mut values, k, v) {
            return refuse(ctx, req, w, &values, r, None);
        }
    }
    if !req.has_body() {
        return refuse(
            ctx,
            req,
            w,
            &values,
            Refusal::new(400, "send the bundle as the request body"),
            None,
        );
    }
    let mut body = http::body(req, reader);
    let boundary = req.multipart_boundary();
    // The settings of a form are in its body, and checked once they have been
    // read; those in a query string can be checked now. A client that waits
    // for the go-ahead has sent nothing yet; one that does not is already
    // sending, and must be read to the end before closing, or the connection
    // resets under the response.
    if boundary.is_none() {
        if let Err(r) = precheck(&ctx.hub.snapshot(), &values) {
            let drain = !req.expects_continue();
            return refuse(ctx, req, w, &values, r, drain.then_some(&mut body));
        }
    }

    let received = match boundary {
        Some(boundary) => {
            let mut form = Multipart::new(&mut body, &boundary);
            receive_form(ctx, req, w, &mut form, &mut values)
        }
        None => {
            if req.expects_continue() {
                http::send_continue(w)?;
            }
            let r = receive(ctx, &values, &mut body, req.content_length());
            // The tar stream ends before the zstd frame and the body do.
            let _ = io::copy(&mut body, &mut io::sink());
            r
        }
    };
    let received = match received {
        Ok(r) => r,
        Err(r) => return refuse(ctx, req, w, &values, r, Some(&mut body)),
    };

    let request = InstallRequest {
        bundle_root: received.dest.to_string_lossy().into_owned(),
        target: values.target.clone(),
        profiles: received.profiles.clone(),
        reprovision_ufs: values.reprovision,
    };
    if let Err(e) = ctx.ctrl.install_request(&request) {
        // The bundle stays unpacked and listed: the operator can still pick
        // another target on the device and install it from there.
        drop(received);
        let status = if e.contains("busy") { 409 } else { 422 };
        return refuse(ctx, req, w, &values, Refusal::new(status, e), None);
    }
    drop(received);

    if html {
        let location = format!("/log?since={first_line}");
        return http::respond(
            w,
            303,
            TEXT,
            b"installing\n",
            &[("Location", location.as_str())],
        );
    }
    let conn = w.try_clone()?;
    let mut out = http::stream(&mut *w, 200, TEXT)?;
    let end = follow_log(
        ctx,
        &mut out,
        first_line,
        Format::Plain,
        Until::RunEnds,
        &conn,
    )?;
    out.write_all(status_line(&end).as_bytes())?;
    out.finish()
}

/// Read the form's fields in order until the bundle, receive the bundle, and
/// make sure nothing that would have changed the install follows it.
fn receive_form<'a, B: Read>(
    ctx: &'a Ctx,
    req: &http::Request,
    w: &mut TcpStream,
    form: &mut Multipart<B>,
    values: &mut FormValues,
) -> Result<Received<'a>, Refusal> {
    let malformed = |e: io::Error| Refusal::new(400, format!("malformed form: {e}"));
    let mut received: Option<Received<'a>> = None;
    while let Some(part) = form.next_part().map_err(malformed)? {
        let setting = matches!(part.name.as_str(), "target" | "profiles" | "ufs");
        if setting && received.is_some() {
            if let Some(r) = received.take() {
                let _ = fs::remove_dir_all(&r.dest);
            }
            return Err(Refusal::new(
                400,
                format!(
                    "the '{}' field came after the bundle; send every setting first",
                    part.name
                ),
            ));
        }
        if setting {
            let text = form.read_text(MAX_FIELD).map_err(malformed)?;
            set_field(values, &part.name, &text)?;
            continue;
        }
        if part.name != "bundle" {
            continue;
        }
        if received.is_some() {
            return Err(Refusal::new(400, "send one bundle per request"));
        }
        precheck(&ctx.hub.snapshot(), values)?;
        if req.expects_continue() {
            // Only now, with the settings checked, is the rest worth sending.
            let _ = http::send_continue(w);
        }
        received = Some(receive(
            ctx,
            values,
            &mut form.body(),
            req.content_length(),
        )?);
    }
    received.ok_or_else(|| Refusal::new(400, "the form has no 'bundle' file"))
}

/// Apply one setting from the query string or the form.
fn set_field(values: &mut FormValues, name: &str, value: &str) -> Result<(), Refusal> {
    let value = value.trim();
    match name {
        "target" => values.target = (!value.is_empty()).then(|| value.to_string()),
        "profiles" => values.profiles = value.to_string(),
        "ufs" => {
            values.reprovision = match value.to_ascii_lowercase().as_str() {
                "" | "keep" | "0" | "no" | "off" | "false" => false,
                "reprovision" | "1" | "yes" | "on" | "true" => true,
                other => {
                    return Err(Refusal::new(
                        400,
                        format!("ufs must be 'keep' or 'reprovision', not '{other}'"),
                    ))
                }
            };
        }
        _ => {}
    }
    Ok(())
}

/// What can be checked before a byte of the bundle has been read.
fn precheck(state: &AppState, values: &FormValues) -> Result<(), Refusal> {
    if !state.phase.accepts_upload() {
        return Err(Refusal::new(
            409,
            format!("the installer is busy ({})", state.phase.label()),
        ));
    }
    let device = values
        .target
        .as_ref()
        .and_then(|p| state.devices.iter().find(|d| &d.path == p));
    match (&values.target, device) {
        (Some(path), None) => Err(Refusal::new(
            422,
            format!(
                "{path} is not an installable device (there is: {})",
                device_list(state)
            ),
        )),
        (Some(_), Some(d)) if !d.boot_rom_capable() => Err(Refusal::new(
            422,
            format!("{} is not a device the boot ROM can boot from", d.summary()),
        )),
        (None, _) if state.selection.target_device.is_none() && !values.reprovision => {
            Err(Refusal::new(
                422,
                "no target device; name one, or set ufs=reprovision to provision a blank \
                 UFS device",
            ))
        }
        _ => Ok(()),
    }
}

fn device_list(state: &AppState) -> String {
    if state.devices.is_empty() {
        return "nothing".to_string();
    }
    state
        .devices
        .iter()
        .map(|d| d.path.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Hold the installer, then unpack the bundle as it arrives into the scratch
/// directory's `upload/`, replacing whatever an earlier upload left there.
fn receive<'a>(
    ctx: &'a Ctx,
    values: &FormValues,
    body: &mut dyn Read,
    total: Option<u64>,
) -> Result<Received<'a>, Refusal> {
    let guard = ctx
        .ctrl
        .begin_receiving()
        .map_err(|e| Refusal::new(409, e))?;
    let cache_dir = ctx.ctrl.config().cache_dir.clone();
    let dest = Path::new(&cache_dir).join("upload");
    let _ = fs::remove_dir_all(&dest);
    ctx.ctrl.log(match total {
        Some(n) => format!("upload: receiving a bundle, {}", human_bytes(n)),
        None => "upload: receiving a bundle".to_string(),
    });

    let state = ctx.ctrl.snapshot();
    let board_id = state.board.board_id.clone();
    let mcu_target = state.mcu.as_ref().and_then(|m| m.target.clone());
    let mut picked: Option<Vec<String>> = None;
    let mut refusal: Option<Refusal> = None;
    let mut on_manifest = |bytes: &[u8]| -> Result<archive::Keep, String> {
        let r = plan(
            ctx,
            &dest,
            &cache_dir,
            bytes,
            &board_id,
            mcu_target.as_deref(),
            &values.profiles,
        );
        match r {
            Ok((keep, profiles)) => {
                picked = Some(profiles);
                Ok(keep)
            }
            Err(r) => {
                let msg = r.msg.clone();
                refusal = Some(r);
                Err(msg)
            }
        }
    };

    let mut counted = Counted {
        inner: body,
        n: 0,
        total,
        next_log: PROGRESS_STEP,
        shown: 0,
        ctx,
        guard: &guard,
    };
    let result = archive::unpack_stream(&mut counted, &dest, &mut on_manifest, &mut |_| {});
    let received = counted.n;
    match result {
        Ok(written) => {
            ctx.ctrl.log(format!(
                "upload: received {} ({} unpacked)",
                human_bytes(received),
                human_bytes(written)
            ));
            Ok(Received {
                _guard: guard,
                dest,
                profiles: picked.unwrap_or_default(),
            })
        }
        Err(e) => {
            let r = refusal.unwrap_or_else(|| {
                if e.contains("os error 28") {
                    Refusal::new(507, format!("out of space for the bundle: {e}"))
                } else if e.contains("timed out") || e.contains("os error 11") {
                    Refusal::new(400, "the upload stalled".to_string())
                } else {
                    Refusal::new(422, e)
                }
            });
            ctx.ctrl.log(format!("upload: refused: {}", r.msg));
            Err(r)
        }
    }
}

/// Read the manifest the moment it arrives, decide what of the bundle to keep,
/// and make sure it fits.
fn plan(
    ctx: &Ctx,
    dest: &Path,
    cache_dir: &str,
    bytes: &[u8],
    board_id: &str,
    mcu_target: Option<&str>,
    profiles: &str,
) -> Result<(archive::Keep, Vec<String>), Refusal> {
    let manifest: Manifest = serde_json::from_slice(bytes)
        .map_err(|e| Refusal::new(422, format!("the bundle's manifest is unreadable: {e}")))?;
    if manifest.schema != 1 {
        return Err(Refusal::new(
            422,
            format!(
                "the bundle declares manifest schema {}, which this installer does not \
                 understand",
                manifest.schema
            ),
        ));
    }
    if manifest.bundle.name.is_empty() {
        return Err(Refusal::new(422, "the bundle's manifest names no bundle"));
    }
    let root = dest.to_string_lossy().into_owned();
    let location = BundleLocation::Dir { root: root.clone() };
    let reference = BundleRef {
        id: root.clone(),
        channel: "upload".to_string(),
        dir: manifest.bundle.name.clone(),
        location: location.clone(),
        source: Source::Local { root },
        archive: None,
    };
    let resolved = bundle::resolve(&reference, &location, &manifest, board_id, mcu_target)
        .map_err(|e| Refusal::new(422, e))?;
    let extras =
        bundle::parse_profiles(profiles, &resolved.build).map_err(|e| Refusal::new(422, e))?;
    let keep = bundle::upload_filter(&manifest, &resolved.board_dir, mcu_target, &extras);
    let needed: u64 = manifest
        .files
        .iter()
        .filter(|f| (keep.filter)(Path::new(&f.path)))
        .map(|f| f.size)
        .sum();
    // Streaming is no way out here: an upload has nowhere to come from twice,
    // so all the bundle's kept files must fit at once.
    stage::ensure_room(cache_dir, needed, "receive the bundle").map_err(|e| {
        Refusal::new(
            507,
            format!(
                "{e}. The bundle is unpacked there as it arrives; fewer profiles need \
                 less, or start the installer with --cache-dir on a larger filesystem."
            ),
        )
    })?;
    ctx.ctrl.log(format!(
        "upload: bundle {} {}, keeping {} for {} with {}",
        manifest.bundle.name,
        manifest.bundle.version,
        human_bytes(needed),
        resolved.board_dir,
        if extras.is_empty() {
            "Minimal only".to_string()
        } else {
            format!("Minimal, {}", extras.join(", "))
        }
    ));
    Ok((keep, extras))
}

/// The upload's body, counted on the way through for the progress bar and log.
struct Counted<'a, 'g, R: ?Sized> {
    inner: &'a mut R,
    n: u64,
    total: Option<u64>,
    next_log: u64,
    /// The progress last published, in tenths of a percent. Every change of
    /// state redraws both other frontends, so not on every read.
    shown: u64,
    ctx: &'a Ctx,
    guard: &'a Receiving<'g>,
}

impl<R: Read + ?Sized> Read for Counted<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.n += n as u64;
        if let Some(total) = self.total.filter(|t| *t > 0) {
            let permille = (self.n.min(total) * 1000) / total;
            if permille != self.shown {
                self.shown = permille;
                self.guard.set_progress(permille as f32 / 1000.0);
            }
        }
        if self.n >= self.next_log {
            self.next_log += PROGRESS_STEP;
            self.ctx.ctrl.log(match self.total {
                Some(t) => format!(
                    "upload: {} of {} received",
                    human_bytes(self.n),
                    human_bytes(t)
                ),
                None => format!("upload: {} received", human_bytes(self.n)),
            });
        }
        Ok(n)
    }
}

/// Turn a request down: the form again with the reason for a browser, one
/// `error:` line for anything else. `drain` is the rest of a body the client is
/// still sending, read off after the response so the close does not reset it.
fn refuse(
    ctx: &Ctx,
    req: &http::Request,
    w: &mut TcpStream,
    values: &FormValues,
    refusal: Refusal,
    drain: Option<&mut Box<dyn Read + '_>>,
) -> io::Result<()> {
    let result = if req.wants_html() {
        let host = super::host(req, w);
        let page = pages::index(&ctx.hub.snapshot(), values, Some(&refusal.msg), &host);
        http::respond(w, refusal.status, HTML, page.as_bytes(), &[])
    } else {
        http::respond(
            w,
            refusal.status,
            TEXT,
            format!("error: {}\n", refusal.msg).as_bytes(),
            &[],
        )
    };
    if let Some(body) = drain {
        let _ = io::copy(body, &mut io::sink());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{StorageDevice, StorageKind};

    fn state() -> AppState {
        AppState {
            phase: crate::core::model::Phase::Ready,
            devices: vec![StorageDevice {
                path: "/dev/mmcblk0".into(),
                kind: StorageKind::Emmc,
                model: "x".into(),
                size_bytes: 1 << 34,
                removable: false,
                logical_block_size: 512,
            }],
            ..AppState::default()
        }
    }

    #[test]
    fn reads_the_settings() {
        let mut v = FormValues::default();
        set_field(&mut v, "target", " /dev/mmcblk0 ").unwrap();
        set_field(&mut v, "profiles", "Desktop").unwrap();
        set_field(&mut v, "ufs", "reprovision").unwrap();
        set_field(&mut v, "unrelated", "x").unwrap();
        assert_eq!(v.target.as_deref(), Some("/dev/mmcblk0"));
        assert_eq!(v.profiles, "Desktop");
        assert!(v.reprovision);
        set_field(&mut v, "target", "").unwrap();
        assert_eq!(v.target, None);
        let r = set_field(&mut v, "ufs", "maybe").unwrap_err();
        assert_eq!(r.status, 400);
    }

    #[test]
    fn checks_what_it_can_before_the_upload() {
        let mut s = state();
        let mut v = FormValues {
            target: Some("/dev/sdz".into()),
            ..FormValues::default()
        };
        let r = precheck(&s, &v).unwrap_err();
        assert_eq!(r.status, 422);
        assert!(r.msg.contains("there is: /dev/mmcblk0"), "{}", r.msg);

        // No target named and none selected: only provisioning could make one.
        v.target = None;
        assert_eq!(precheck(&s, &v).unwrap_err().status, 422);
        v.reprovision = true;
        assert!(precheck(&s, &v).is_ok());

        v.target = Some("/dev/mmcblk0".into());
        assert!(precheck(&s, &v).is_ok());

        // Listed, but nothing the boot ROM could start from.
        s.devices.push(StorageDevice {
            path: "/dev/loop0".into(),
            kind: StorageKind::Other,
            model: "loop".into(),
            size_bytes: 1 << 34,
            removable: false,
            logical_block_size: 512,
        });
        v.target = Some("/dev/loop0".into());
        let r = precheck(&s, &v).unwrap_err();
        assert_eq!(r.status, 422);
        assert!(r.msg.contains("boot ROM"), "{}", r.msg);

        v.target = Some("/dev/mmcblk0".into());
        s.phase = crate::core::model::Phase::Installing;
        assert_eq!(precheck(&s, &v).unwrap_err().status, 409);
        s.phase = crate::core::model::Phase::Discovering;
        assert_eq!(precheck(&s, &v).unwrap_err().status, 409);
        // Still looking for bundles, which an upload does not need.
        s.phase = crate::core::model::Phase::Searching;
        assert!(precheck(&s, &v).is_ok());
    }
}
