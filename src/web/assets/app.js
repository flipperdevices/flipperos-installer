// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

// Niceties for the installer's pages. Nothing depends on this script: the form
// posts and the log page streams without it. With it, the form shows how much
// of the bundle has been sent and then the install log in place, and the log
// page keeps itself scrolled and shows a progress bar.
(function () {
  "use strict";

  function $(id) {
    return document.getElementById(id);
  }

  function mib(bytes) {
    return (bytes / 1048576).toFixed(0) + " MiB";
  }

  function callout(kind, strong, text) {
    var div = document.createElement("div");
    div.className = "callout " + kind;
    var p = document.createElement("p");
    var b = document.createElement("strong");
    b.textContent = strong;
    p.appendChild(b);
    if (text) {
      p.appendChild(document.createTextNode(" " + text));
    }
    div.appendChild(p);
    return div;
  }

  function nearBottom(el) {
    return el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  }

  // Read /status, handing `done` the parsed state, or null if it failed.
  function getStatus(done) {
    var xhr = new XMLHttpRequest();
    xhr.open("GET", "/status");
    xhr.setRequestHeader("Accept", "application/json");
    xhr.onload = function () {
      var s = null;
      try {
        s = JSON.parse(xhr.responseText);
      } catch (e) {
        /* s stays null */
      }
      done(s);
    };
    xhr.onerror = function () {
      done(null);
    };
    xhr.send();
  }

  // Poll /status for the progress bar while `active()` holds.
  function pollProgress(bar, label, active) {
    function tick() {
      getStatus(function (s) {
        if (s) {
          bar.hidden = false;
          bar.value = s.progress;
          if (label) {
            label.hidden = false;
            label.textContent = s.phase_label + " " + Math.round(s.progress * 100) + "%";
          }
        }
        if (active()) {
          setTimeout(tick, s ? 1000 : 3000);
        }
      });
    }
    tick();
  }

  // The same form the log page offers, so a finished install can be rebooted
  // into from where it was started.
  function rebootForm() {
    var form = document.createElement("form");
    form.method = "post";
    form.action = "/reboot";
    form.className = "actions";
    var button = document.createElement("button");
    button.className = "pill";
    button.type = "submit";
    button.textContent = "Reboot";
    form.appendChild(button);
    return form;
  }

  // Say how a successful run ended, as the log page does: a dry run wrote
  // nothing, a real one is ready to boot.
  function showSuccess(result) {
    getStatus(function (s) {
      if (s && s.dry_run) {
        result.appendChild(callout("ok", "Dry run complete.", "Nothing was written."));
        return;
      }
      result.appendChild(callout("ok", "Installed.", "Reboot into the new system when ready."));
      if (s && s.can_reboot) {
        result.appendChild(rebootForm());
      }
    });
  }

  // The log page streams in; this script runs before its body exists.
  function followLogPage() {
    var stick = true;
    var hooked = false;
    var timer = setInterval(function () {
      var log = $("log");
      if (!log) {
        return;
      }
      if (!hooked) {
        hooked = true;
        log.addEventListener("scroll", function () {
          stick = nearBottom(log);
        });
        pollProgress($("bar"), $("phase"), function () {
          return document.readyState === "loading";
        });
      }
      if (stick) {
        log.scrollTop = log.scrollHeight;
      }
      if (document.readyState !== "loading") {
        clearInterval(timer);
        if (stick) {
          log.scrollTop = log.scrollHeight;
        }
      }
    }, 250);
  }

  function enhanceForm(form) {
    if (!window.FormData || !window.XMLHttpRequest) {
      return;
    }
    form.addEventListener("submit", function (ev) {
      ev.preventDefault();
      var button = form.querySelector("button[type=submit]");
      var box = $("progress");
      var text = $("progress-text");
      var bar = $("progress-bar");
      var log = $("progress-log");
      var result = $("progress-result");
      var running = true;

      button.disabled = true;
      box.hidden = false;
      log.hidden = true;
      log.textContent = "";
      result.textContent = "";
      bar.value = 0;
      text.textContent = "Uploading…";

      var xhr = new XMLHttpRequest();
      xhr.open("POST", form.action);
      xhr.setRequestHeader("Accept", "text/plain");

      xhr.upload.onprogress = function (e) {
        if (e.lengthComputable) {
          bar.value = e.loaded / e.total;
          text.textContent = "Uploading: " + mib(e.loaded) + " of " + mib(e.total);
        }
      };
      xhr.upload.onload = function () {
        text.textContent = "Starting the installation…";
        bar.value = 0;
      };
      xhr.onprogress = function () {
        if (xhr.status !== 200) {
          return;
        }
        var stick = log.hidden || nearBottom(log);
        log.hidden = false;
        log.textContent = xhr.responseText;
        if (stick) {
          log.scrollTop = log.scrollHeight;
        }
      };
      xhr.onreadystatechange = function () {
        if (xhr.readyState === 2 && xhr.status === 200) {
          text.textContent = "Installing…";
          pollProgress(bar, null, function () {
            return running;
          });
        }
      };
      xhr.onload = function () {
        running = false;
        var body = xhr.responseText;
        if (xhr.status !== 200) {
          box.hidden = false;
          text.textContent = "";
          result.appendChild(
            callout("error", "Not installed.", body.replace(/^error:\s*/, "").trim())
          );
          button.disabled = false;
          return;
        }
        log.textContent = body;
        log.scrollTop = log.scrollHeight;
        var lines = body.trim().split("\n");
        var last = lines[lines.length - 1] || "";
        if (last === "status: done") {
          bar.value = 1;
          text.textContent = "";
          showSuccess(result);
        } else {
          text.textContent = "";
          result.appendChild(
            callout("error", "Installation failed.", last.replace(/^status:\s*(failed:\s*)?/, ""))
          );
          button.disabled = false;
        }
      };
      xhr.onerror = function () {
        running = false;
        text.textContent = "";
        result.appendChild(
          callout("error", "Connection lost.", "The device stopped answering. Its own screen shows what it is doing.")
        );
        button.disabled = false;
      };
      xhr.send(new FormData(form));
    });
  }

  if (location.pathname === "/log") {
    followLogPage();
  }
  document.addEventListener("DOMContentLoaded", function () {
    var form = $("install");
    if (form) {
      enhanceForm(form);
    }
  });
})();
