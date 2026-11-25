// SPDX-License-Identifier: AGPL-3.0-only
// The console's few behaviours: copy buttons, the open tab, and filling a
// form in from what was picked (a dist's settings when its name is).
(function () {
  "use strict";
  // The theme: as the system has it, or as chosen here (this browser
  // only). Set before the page is drawn: this file is loaded in <head>.
  var THEMES = ["system", "light", "dark"];
  function getTheme() {
    try {
      return localStorage.getItem("pkgpod-theme") || "system";
    } catch (e) {
      return "system";
    }
  }
  function applyTheme(t) {
    if (t === "light" || t === "dark") {
      document.documentElement.setAttribute("data-theme", t);
    } else {
      document.documentElement.removeAttribute("data-theme");
    }
    document.querySelectorAll(".theme-switch").forEach(function (b) {
      b.textContent = "Theme: " + t;
    });
  }
  applyTheme(getTheme());
  document.addEventListener("DOMContentLoaded", function () {
    applyTheme(getTheme());
  });
  // Tabs: arrow keys, Home and End move between them and open the one
  // reached.
  document.addEventListener("keydown", function (ev) {
    var tab = ev.target.closest && ev.target.closest('[role="tablist"] [role="tab"]');
    if (!tab) return;
    var tabs = Array.prototype.slice.call(tab.parentElement.querySelectorAll('[role="tab"]'));
    var i = tabs.indexOf(tab);
    var to = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[ev.key];
    if (to === undefined) return;
    ev.preventDefault();
    var next = tabs[(to + tabs.length) % tabs.length];
    next.focus();
    next.click();
  });
  // Over plain http on a LAN address there is no navigator.clipboard.
  function fallback(text, done) {
    var t = document.createElement("textarea");
    t.value = text;
    t.setAttribute("readonly", "");
    t.className = "offscreen";
    document.body.appendChild(t);
    t.select();
    var ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (e) {
      ok = false;
    }
    document.body.removeChild(t);
    if (ok) done();
  }
  function copy(text, done) {
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(done, function () {
        fallback(text, done);
      });
    } else {
      fallback(text, done);
    }
  }
  // data-prefill on a field: {value: {field: value}}; when the field takes
  // one of the values, the form's other fields get theirs.
  function prefill(el) {
    var all;
    try {
      all = JSON.parse(el.getAttribute("data-prefill"));
    } catch (e) {
      return;
    }
    var vals = all && all[el.value.trim()];
    if (!vals || el.dataset.filled === el.value) return;
    el.dataset.filled = el.value;
    Object.keys(vals).forEach(function (k) {
      var f = el.form && el.form.elements[k];
      if (!f || f === el || f.length !== undefined && !f.tagName) return;
      var v = vals[k];
      if (f.type === "checkbox") {
        f.checked = v === true;
      } else if (Array.isArray(v)) {
        f.value = v.join(", ");
      } else {
        f.value = v === null || v === undefined ? "" : String(v);
      }
    });
  }
  ["input", "change"].forEach(function (t) {
    document.addEventListener(t, function (ev) {
      // What people change (not what prefill puts in) is sent.
      if (ev.isTrusted && ev.target.name) ev.target.dataset.dirty = "1";
      if (ev.target.hasAttribute && ev.target.hasAttribute("data-prefill")) {
        prefill(ev.target);
      }
    });
  });
  // Each cell knows its column's name, for narrow screens where a table
  // is shown a row at a time (the stylesheet puts the name before it).
  function labelTables(root) {
    (root || document).querySelectorAll(".table-wrap table").forEach(function (t) {
      var names = Array.prototype.map.call(t.querySelectorAll("thead th"), function (th) {
        return th.textContent.trim();
      });
      if (!names.length) return;
      t.querySelectorAll("tbody tr").forEach(function (tr) {
        Array.prototype.forEach.call(tr.children, function (td, i) {
          if (!td.hasAttribute("data-label")) td.setAttribute("data-label", names[i] || "");
        });
      });
    });
  }
  document.addEventListener("DOMContentLoaded", function () {
    labelTables(document);
  });
  document.addEventListener("htmx:afterSettle", function (ev) {
    labelTables(ev.detail.elt.closest ? ev.detail.elt.closest("main") || ev.detail.elt : document);
  });
  // A badge that does not load: its address does not reach the serving
  // listener from here (a console-only host, a proxy); the card says so.
  document.addEventListener("error", function (ev) {
    var img = ev.target;
    if (!img || img.tagName !== "IMG") return;
    var card = img.closest("#badges");
    var note = card && card.querySelector(".badge-broken");
    if (note) note.hidden = false;
  }, true);
  // A running job's log: its new lines come as a stream (Server-Sent
  // Events) and are added to what is shown, which is never replaced, so
  // a selection or the place read stay; followed at the bottom only when
  // the reader is there. The end gives the job's state.
  function streamLogs(root) {
    (root || document).querySelectorAll("[data-stream]").forEach(function (box) {
      if (box.dataset.streaming) return;
      box.dataset.streaming = "1";
      var pre = box.querySelector("pre.log");
      var es = new EventSource(box.dataset.stream);
      es.addEventListener("log", function (ev) {
        var atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24;
        var text = ev.data + "\n";
        if (pre.textContent && !/\n$/.test(pre.textContent)) text = "\n" + text;
        pre.appendChild(document.createTextNode(text));
        if (atBottom) pre.scrollTop = pre.scrollHeight;
      });
      es.addEventListener("end", function (ev) {
        es.close();
        var badge = box.querySelector(".job-state");
        if (badge) {
          badge.textContent = ev.data;
          badge.classList.remove("run");
          badge.classList.add(ev.data === "ok" ? "ok" : "bad");
        }
        var stop = box.querySelector(".job-stop");
        if (stop) stop.remove();
      });
      // Gone (the server restarting, the page left): the browser tries
      // again by itself, from the last line it had.
    });
  }
  document.addEventListener("DOMContentLoaded", function () {
    streamLogs(document);
  });
  document.addEventListener("htmx:afterSettle", function (ev) {
    streamLogs(ev.detail.elt);
  });
  // A form with data-sparse sends only the fields changed in it (an
  // unticked box as false): an operation keeps what it is not given.
  document.addEventListener("htmx:configRequest", function (ev) {
    var form = ev.detail.elt;
    if (!form || !form.hasAttribute || !form.hasAttribute("data-sparse")) return;
    var fd = ev.detail.formData;
    var p = ev.detail.parameters;
    var groups = {};
    Array.prototype.forEach.call(form.elements, function (el) {
      if (!el.name) return;
      var g = groups[el.name] || (groups[el.name] = { dirty: false, els: [] });
      g.els.push(el);
      if (el.dataset.dirty) g.dirty = true;
    });
    Object.keys(groups).forEach(function (n) {
      var g = groups[n];
      if (!g.dirty) {
        if (fd) fd.delete(n);
        if (p) delete p[n];
      } else if (g.els.length === 1 && g.els[0].type === "checkbox" && !g.els[0].checked) {
        if (fd) fd.set(n, "false");
        if (p) p[n] = "false";
      }
    });
  });
  function bytes(s) {
    var m = /^([0-9.]+)\s*(B|KiB|MiB|GiB|TiB)?$/.exec(s.trim());
    if (!m) return null;
    var k = { B: 1, KiB: 1024, MiB: 1048576, GiB: 1073741824, TiB: 1099511627776 }[m[2] || "B"];
    return parseFloat(m[1]) * k;
  }
  function sortBy(th) {
    var table = th.closest("table");
    var i = Array.prototype.indexOf.call(th.parentElement.children, th);
    var up = th.getAttribute("aria-sort") !== "ascending";
    table.querySelectorAll("th[aria-sort]").forEach(function (h) {
      h.removeAttribute("aria-sort");
    });
    th.setAttribute("aria-sort", up ? "ascending" : "descending");
    var body = table.tBodies[0];
    var rows = Array.prototype.slice.call(body.rows);
    var key = function (r) {
      var c = r.cells[i];
      return c ? c.textContent.trim() : "";
    };
    rows.sort(function (a, b) {
      var x = key(a), y = key(b);
      var bx = bytes(x), by = bytes(y);
      var c = bx !== null && by !== null ? bx - by : x.localeCompare(y, undefined, { numeric: true });
      return up ? c : -c;
    });
    rows.forEach(function (r) {
      body.appendChild(r);
    });
  }
  document.addEventListener("change", function (ev) {
    if (!ev.target.classList || !ev.target.classList.contains("ver-pick")) return;
    var picked = document.querySelectorAll("input.ver-pick:checked");
    if (picked.length > 2) ev.target.checked = false;
    var b = document.querySelector("button.ver-compare");
    var n = document.querySelectorAll("input.ver-pick:checked").length;
    if (b) b.disabled = n === 0;
  });
  // Rows chosen for a bulk action: the count, and choosing all.
  document.addEventListener("change", function (ev) {
    var form = ev.target.closest && ev.target.closest("#pkg-pick");
    if (!form) return;
    if (ev.target.classList.contains("pick-all")) {
      form.querySelectorAll("input[name=path]").forEach(function (c) {
        c.checked = ev.target.checked;
      });
    }
    var n = form.querySelectorAll("input[name=path]:checked").length;
    var out = form.querySelector(".pick-count");
    if (out) out.textContent = String(n);
  });
  // Repositories found by name, description or format.
  function filterRepos() {
    var q = document.getElementById("repo-q");
    var f = document.getElementById("repo-format");
    if (!q || !f) return;
    var words = q.value.toLowerCase().split(/\s+/).filter(Boolean);
    var shown = 0, all = 0;
    document.querySelectorAll("table.repos tbody tr").forEach(function (tr) {
      all++;
      var text = (tr.getAttribute("data-find") || "").toLowerCase();
      var ok = (!f.value || tr.getAttribute("data-format") === f.value) &&
        words.every(function (w) { return text.indexOf(w) >= 0; });
      tr.hidden = !ok;
      if (ok) shown++;
    });
    var none = document.querySelector(".repo-none");
    if (none) none.hidden = shown > 0;
    var c = document.querySelector(".repo-count");
    if (c) c.textContent = shown === all ? "" : shown + " of " + all;
  }
  document.addEventListener("input", function (ev) {
    if (ev.target.id === "repo-q" || ev.target.id === "repo-format") filterRepos();
  });
  // Mirrors' sync states on the list follow the jobs: every 10 s while
  // one runs, every minute otherwise, not while the tab is hidden. Only
  // a cell whose state changed is touched, so nothing flickers and the
  // filter stays as it is.
  document.addEventListener("DOMContentLoaded", function followSyncs() {
    if (!document.querySelector("td[data-sync]")) return;
    var timer = null;
    function later() {
      clearTimeout(timer);
      var busy = document.querySelector("td[data-sync].st-run");
      timer = setTimeout(poll, busy ? 10000 : 60000);
    }
    function poll() {
      if (document.hidden) { later(); return; }
      fetch("/c/repos/syncs", { credentials: "same-origin", cache: "no-store" })
        .then(function (r) { return r.ok ? r.json() : null; })
        .then(function (all) {
          if (!all) return;
          document.querySelectorAll("td[data-sync]").forEach(function (td) {
            var st = all[td.getAttribute("data-sync")];
            if (!st) return;
            var key = [st.state, st.content, st.when, st.job].join("|");
            if (td.getAttribute("data-shown") === key) return;
            td.setAttribute("data-shown", key);
            td.classList.remove("st-ok", "st-bad", "st-run");
            if (st.state) td.classList.add("st-" + st.state);
            var a = document.createElement("a");
            a.href = "/jobs/" + st.job;
            var at = document.createElement("span");
            at.className = "nowrap";
            at.textContent = st.when;
            a.append(st.content + " ", at);
            td.replaceChildren(a);
          });
        })
        .catch(function () {})
        .then(later);
    }
    document.addEventListener("visibilitychange", function () {
      if (!document.hidden) poll();
    });
    later();
  });
  document.addEventListener("change", function (ev) {
    if (ev.target.id === "repo-format") filterRepos();
  });
  // Files dropped on an upload form, listed; its progress while sent.
  function listFiles(input) {
    var ul = input.closest(".dropzone").querySelector(".drop-list");
    if (!ul) return;
    ul.textContent = "";
    Array.prototype.forEach.call(input.files, function (f) {
      var li = document.createElement("li");
      li.textContent = f.name + " (" + Math.ceil(f.size / 1024) + " KiB)";
      ul.appendChild(li);
    });
  }
  document.addEventListener("change", function (ev) {
    if (ev.target.type === "file" && ev.target.closest(".dropzone")) listFiles(ev.target);
  });
  ["dragenter", "dragover"].forEach(function (t) {
    document.addEventListener(t, function (ev) {
      var z = ev.target.closest && ev.target.closest(".dropzone");
      if (!z) return;
      ev.preventDefault();
      z.classList.add("over");
    });
  });
  document.addEventListener("dragleave", function (ev) {
    var z = ev.target.closest && ev.target.closest(".dropzone");
    if (z && !z.contains(ev.relatedTarget)) z.classList.remove("over");
  });
  document.addEventListener("drop", function (ev) {
    var z = ev.target.closest && ev.target.closest(".dropzone");
    if (!z) return;
    ev.preventDefault();
    z.classList.remove("over");
    var input = z.querySelector("input[type=file]");
    input.files = ev.dataTransfer.files;
    listFiles(input);
  });
  document.addEventListener("htmx:xhr:progress", function (ev) {
    var form = ev.target.closest && ev.target.closest(".upload-form");
    if (!form || !ev.detail.lengthComputable) return;
    var bar = form.querySelector(".upload-progress");
    var pct = Math.round((ev.detail.loaded / ev.detail.total) * 100);
    bar.hidden = false;
    bar.value = pct;
    form.querySelector(".upload-pct").textContent = pct < 100 ? pct + "%" : "taking it in…";
  });
  document.addEventListener("htmx:afterRequest", function (ev) {
    var form = ev.target.closest && ev.target.closest(".upload-form");
    if (!form) return;
    form.querySelector(".upload-progress").hidden = true;
    form.querySelector(".upload-pct").textContent = "";
  });
  document.addEventListener("click", function (ev) {
    var b = ev.target.closest("button.copy");
    if (b) {
      var src = b.parentElement.querySelector("pre, .copy-src");
      if (!src) return;
      copy(src.textContent.trim(), function () {
        var label = b.textContent;
        b.classList.add("done");
        b.textContent = "Copied";
        setTimeout(function () {
          b.classList.remove("done");
          b.textContent = label;
        }, 1400);
      });
      return;
    }
    var sw = ev.target.closest("button.theme-switch");
    if (sw) {
      var next = THEMES[(THEMES.indexOf(getTheme()) + 1) % THEMES.length];
      try {
        localStorage.setItem("pkgpod-theme", next);
      } catch (e) {
        /* not kept: this page only */
      }
      applyTheme(next);
      return;
    }
    // Compare the versions ticked (one: against the current).
    var cmp = ev.target.closest("button.ver-compare");
    if (cmp) {
      var seqs = Array.prototype.map.call(
        document.querySelectorAll("input.ver-pick:checked"),
        function (c) { return parseInt(c.value, 10); }
      ).sort(function (a, b) { return a - b; });
      if (!seqs.length || !window.htmx) return;
      var url = "/r/" + encodeURIComponent(cmp.getAttribute("data-repo")) + "/diff?from=" + seqs[0] +
        (seqs.length > 1 ? "&to=" + seqs[seqs.length - 1] : "");
      window.htmx.ajax("GET", url, "#ver-result");
      return;
    }
    var close = ev.target.closest(".close-detail");
    if (close) {
      close.closest(".pkg-detail").remove();
      return;
    }
    // Sort a table by a column's text (sizes by their bytes).
    var th = ev.target.closest("table.sortable th");
    if (th && !th.classList.contains("pick")) {
      sortBy(th);
      return;
    }
    // "change…" on a row: its name into the form, which fills the rest.
    var fill = ev.target.closest("button.fill");
    if (fill) {
      var to = document.getElementById(fill.getAttribute("data-for"));
      if (to) {
        // A form folded away is opened.
        var folded = to.closest("details");
        if (folded) folded.open = true;
        to.value = fill.getAttribute("data-value");
        prefill(to);
        to.focus();
        to.scrollIntoView({ block: "center" });
      }
      return;
    }
    var tab = ev.target.closest(".tabs .tab");
    if (tab) {
      tab.parentElement.querySelectorAll(".tab").forEach(function (t) {
        t.removeAttribute("aria-selected");
      });
      tab.setAttribute("aria-selected", "true");
    }
  });
})();
