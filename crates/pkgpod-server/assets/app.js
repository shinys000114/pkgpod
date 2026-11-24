// SPDX-License-Identifier: AGPL-3.0-only
// The console's few behaviours: copy buttons, the open tab, and filling a
// form in from what was picked (a dist's settings when its name is).
(function () {
  "use strict";
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
