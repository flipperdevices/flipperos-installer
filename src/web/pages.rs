// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! The pages, rendered on the board from the current state.
//!
//! Everything works without a script: the form posts, the log page streams.
//! `app.js` only adds an upload progress bar and keeps the log scrolled.

use std::fmt::Write;

use crate::core::model::{human_bytes, AppState, Phase};

/// What the form was last submitted with, to put back on a page that refuses
/// it.
#[derive(Clone, Debug)]
pub struct FormValues {
    pub target: Option<String>,
    pub profiles: String,
    pub reprovision: bool,
}

impl Default for FormValues {
    fn default() -> Self {
        Self {
            target: None,
            profiles: "all".to_string(),
            reprovision: false,
        }
    }
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const FAVICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' \
    viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='8' fill='%23ff8200'/%3E\
    %3Ctext x='16' y='22' font-family='Arial' font-weight='800' font-size='15' \
    fill='white' text-anchor='middle'%3EF1%3C/text%3E%3C/svg%3E";

/// The page up to the start of `<main>`'s content.
fn open(title: &str, state: &AppState) -> String {
    let board = if state.board.model.is_empty() {
        "FlipperOS".to_string()
    } else {
        state.board.model.clone()
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <link rel=\"icon\" href=\"{FAVICON}\">\n\
         <link rel=\"stylesheet\" href=\"/style.css\">\n\
         <script src=\"/app.js\"></script>\n\
         </head>\n<body>\n\
         <header><div class=\"bar\"><span class=\"logo\">F1</span>\
         <span class=\"name\">FlipperOS Installer</span>\
         <span class=\"sub\">{board}</span></div></header>\n<main>\n",
        title = escape(title),
        board = escape(&board),
    )
}

const CLOSE: &str = "</main>\n</body>\n</html>\n";

fn dry_run_callout(state: &AppState) -> &'static str {
    if state.dry_run {
        "<div class=\"callout warn\"><p><strong>Dry run.</strong> The installer was \
         started without <code>--no-dry-run</code>: every step is logged, nothing is \
         written.</p></div>\n"
    } else {
        ""
    }
}

/// The form, with an error from the last attempt if there was one.
pub fn index(state: &AppState, values: &FormValues, error: Option<&str>, host: &str) -> String {
    let mut out = open("FlipperOS Installer", state);
    out.push_str(
        "<h1>Install FlipperOS</h1>\n<p class=\"lead\">Upload an update bundle over this \
         cable and install it onto the device's storage — no network needed.</p>\n",
    );
    out.push_str(dry_run_callout(state));
    if !state.phase.accepts_upload() {
        let _ = writeln!(
            out,
            "<div class=\"callout warn\"><p>The installer is busy ({}). \
             <a href=\"/log\">Follow its log</a>.</p></div>",
            escape(&state.phase.label())
        );
    }
    if let Some(e) = error {
        let _ = writeln!(
            out,
            "<div class=\"callout error\" role=\"alert\"><p><strong>Not installed.</strong> \
             {}</p></div>",
            escape(e)
        );
    }

    // What the board is.
    out.push_str("<section class=\"card\"><h2>This device</h2><dl class=\"facts\">");
    let _ = write!(
        out,
        "<dt>Board</dt><dd>{}</dd>",
        escape(if state.board.board_id.is_empty() {
            "unknown"
        } else {
            &state.board.board_id
        })
    );
    if let Some(mcu) = &state.mcu {
        let _ = write!(
            out,
            "<dt>MCU</dt><dd>firmware {}{}</dd>",
            escape(&mcu.version),
            mcu.target
                .as_deref()
                .map(|t| format!(", target {}", escape(t)))
                .unwrap_or_default()
        );
    }
    let _ = writeln!(
        out,
        "<dt>Installer</dt><dd>{}</dd></dl></section>",
        escape(&state.phase.label())
    );

    // The form. The bundle comes last: a form's fields arrive in order, and the
    // board needs to know what to do with the bundle before it arrives.
    out.push_str(
        "<form class=\"card\" id=\"install\" method=\"post\" action=\"/install\" \
         enctype=\"multipart/form-data\">\n<h2>Installation</h2>\n",
    );

    out.push_str(
        "<div class=\"field\"><label class=\"title\" for=\"target\">Target device</label>\
         <select id=\"target\" name=\"target\">",
    );
    let chosen = values
        .target
        .as_deref()
        .or(state.selection.target_device.as_deref());
    if state.devices.is_empty() {
        out.push_str("<option value=\"\">No storage found</option>");
    }
    for d in &state.devices {
        let bootable = d.boot_rom_capable();
        let _ = write!(
            out,
            "<option value=\"{path}\"{sel}{dis}>{label}{why}</option>",
            path = escape(&d.path),
            sel = if bootable && Some(d.path.as_str()) == chosen {
                " selected"
            } else {
                ""
            },
            dis = if bootable { "" } else { " disabled" },
            label = escape(&format!(
                "{} — {} {}, {}",
                d.path,
                d.kind.as_str(),
                d.model.trim(),
                human_bytes(d.size_bytes)
            )),
            why = if bootable { "" } else { " (not bootable)" },
        );
    }
    out.push_str("</select><p class=\"hint\">Everything on it is erased.</p></div>\n");

    let _ = writeln!(
        out,
        "<div class=\"field\"><label class=\"title\" for=\"profiles\">Profiles</label>\
         <input type=\"text\" id=\"profiles\" name=\"profiles\" value=\"{}\" \
         list=\"profile-hints\" autocomplete=\"off\" spellcheck=\"false\">\
         <datalist id=\"profile-hints\"><option value=\"all\"><option value=\"none\">\
         </datalist><p class=\"hint\"><code>all</code>, <code>none</code> (Minimal only), \
         or names from the bundle, e.g. <code>Desktop, Router</code>. Minimal is always \
         installed.</p></div>",
        escape(&values.profiles)
    );

    let _ = writeln!(
        out,
        "<div class=\"field\"><label class=\"check\"><input type=\"checkbox\" name=\"ufs\" \
         value=\"reprovision\"{}><span>Reprovision UFS if its layout differs from the \
         FlipperOS scheme</span></label><p class=\"hint\">Rewrites the device's logical \
         units, erasing the whole UFS device. Needed once on a factory-fresh one.</p></div>",
        if values.reprovision { " checked" } else { "" }
    );

    out.push_str(
        "<div class=\"field\"><label class=\"title\" for=\"bundle\">Update bundle</label>\
         <input type=\"file\" id=\"bundle\" name=\"bundle\" accept=\".zst,.tar.zst\" required>\
         <p class=\"hint\">A complete <code>flipperone-update-….tar.zst</code>. It is \
         unpacked on the device as it arrives and checked against its manifest.</p></div>\n",
    );

    out.push_str(
        "<div class=\"actions\"><button class=\"pill\" type=\"submit\">Install</button></div>\n\
         <div id=\"progress\" hidden><p class=\"status\" id=\"progress-text\"></p>\
         <progress id=\"progress-bar\" max=\"1\" value=\"0\"></progress>\
         <pre class=\"log\" id=\"progress-log\" hidden></pre>\
         <div id=\"progress-result\"></div></div>\n</form>\n",
    );

    let base = format!("http://{}", escape(host));
    let _ = write!(
        out,
        "<details class=\"api\"><summary>Installing without a browser</summary>\n\
         <p>Every page here answers a client that does not ask for HTML with plain text \
         (or JSON). Send the bundle as the request body and the settings in the query \
         string, or post the same form fields as this page:</p>\n\
         <pre>curl -T bundle.tar.zst '{base}/install?target=/dev/sda&amp;profiles=all'\n\
         curl -F target=/dev/sda -F profiles=Desktop -F bundle=@bundle.tar.zst {base}/install</pre>\n\
         <p>The response is the installation log as it happens, ending with a line \
         <code>status: done</code> or <code>status: failed: …</code>. A request refused \
         before the install starts gets a 4xx status and one line <code>error: …</code>. \
         Add <code>ufs=reprovision</code> to rewrite a mismatched UFS device.</p>\n\
         <pre>curl {base}/status            # JSON: can_upload, phase, progress, devices…\n\
         curl '{base}/log?follow=1'      # the log, following a running install\n\
         curl -X POST {base}/reboot      # once an install is done</pre>\n</details>\n"
    );

    out.push_str(CLOSE);
    out
}

/// The streamed log page, up to the first log line.
pub fn log_open(state: &AppState) -> String {
    let mut out = open("Installation log", state);
    out.push_str("<h1>Installation log</h1>\n");
    out.push_str(dry_run_callout(state));
    out.push_str(
        "<p class=\"status\" id=\"phase\" hidden></p>\
         <progress id=\"bar\" max=\"1\" value=\"0\" hidden></progress>\n\
         <pre class=\"log\" id=\"log\">",
    );
    out
}

/// One log line for [`log_open`]'s `<pre>`.
pub fn log_line(line: &str) -> String {
    let mut s = escape(line);
    s.push('\n');
    s
}

/// The end of the streamed log page: how it went, and what can be done next.
pub fn log_close(state: &AppState) -> String {
    let mut out = String::from("</pre>\n");
    out.push_str(&result_callout(state));
    if state.can_reboot() {
        out.push_str(
            "<form method=\"post\" action=\"/reboot\" class=\"actions\">\
             <button class=\"pill\" type=\"submit\">Reboot</button></form>\n",
        );
    }
    out.push_str("<p><a href=\"/\">Back to the installer</a></p>\n");
    out.push_str(CLOSE);
    out
}

fn result_callout(state: &AppState) -> String {
    match &state.phase {
        Phase::Done if state.dry_run => "<div class=\"callout ok\" id=\"result\"><p>\
            <strong>Dry run complete.</strong> Nothing was written.</p></div>\n"
            .to_string(),
        Phase::Done => "<div class=\"callout ok\" id=\"result\"><p><strong>Installed.</strong> \
            Reboot into the new system when ready.</p></div>\n"
            .to_string(),
        Phase::Failed(e) => format!(
            "<div class=\"callout error\" id=\"result\"><p><strong>Installation failed.</strong> \
             {}</p></div>\n",
            escape(e)
        ),
        other => format!(
            "<div class=\"callout warn\" id=\"result\"><p>The installer is {}.</p></div>\n",
            escape(&other.label())
        ),
    }
}

/// A short page saying one thing.
pub fn message(state: &AppState, title: &str, text: &str) -> String {
    let mut out = open(title, state);
    let _ = write!(
        out,
        "<h1>{}</h1>\n<p>{}</p>\n<p><a href=\"/\">Back to the installer</a></p>\n",
        escape(title),
        escape(text)
    );
    out.push_str(CLOSE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{StorageDevice, StorageKind};

    fn state() -> AppState {
        let mut s = AppState::default();
        s.board.model = "Flipper One <rev F>".into();
        s.board.board_id = "flipper-one".into();
        s.devices = vec![StorageDevice {
            path: "/dev/sda".into(),
            kind: StorageKind::Ufs,
            model: "BIWIN".into(),
            size_bytes: 128 << 30,
            removable: false,
            logical_block_size: 4096,
        }];
        s.selection.target_device = Some("/dev/sda".into());
        s
    }

    #[test]
    fn the_form_escapes_and_orders_its_fields() {
        let page = index(
            &state(),
            &FormValues {
                profiles: "<b>".into(),
                ..FormValues::default()
            },
            Some("bad \"thing\""),
            "192.168.241.1",
        );
        assert!(page.contains("Flipper One &lt;rev F&gt;"));
        assert!(page.contains("value=\"&lt;b&gt;\""));
        assert!(page.contains("bad &quot;thing&quot;"));
        assert!(page.contains("<option value=\"/dev/sda\" selected>"));
        // The file field comes after every setting.
        let file = page.find("name=\"bundle\"").unwrap();
        for field in ["name=\"target\"", "name=\"profiles\"", "name=\"ufs\""] {
            assert!(page.find(field).unwrap() < file, "{field}");
        }
        assert!(page.contains("http://192.168.241.1/install"));
    }

    #[test]
    fn the_log_page_ends_with_the_outcome() {
        let mut s = state();
        s.phase = Phase::Failed("disk on fire".into());
        let page = log_open(&s) + &log_line("a <line>") + &log_close(&s);
        assert!(page.contains("<pre class=\"log\" id=\"log\">a &lt;line&gt;\n</pre>"));
        assert!(page.contains("Installation failed.</strong> disk on fire"));
        assert!(!page.contains("action=\"/reboot\""));

        s.phase = Phase::Done;
        assert!(log_close(&s).contains("action=\"/reboot\""));
        s.dry_run = true;
        let close = log_close(&s);
        assert!(close.contains("Dry run complete"));
        assert!(!close.contains("action=\"/reboot\""));
    }
}
