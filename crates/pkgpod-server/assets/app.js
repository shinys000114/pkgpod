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
      if (ev.target.hasAttribute && ev.target.hasAttribute("data-prefill")) {
        prefill(ev.target);
      }
    });
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
