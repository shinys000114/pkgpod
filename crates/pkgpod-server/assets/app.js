// SPDX-License-Identifier: AGPL-3.0-only
// The console's few behaviours: copy buttons and the open tab.
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
    var tab = ev.target.closest(".tabs .tab");
    if (tab) {
      tab.parentElement.querySelectorAll(".tab").forEach(function (t) {
        t.removeAttribute("aria-selected");
      });
      tab.setAttribute("aria-selected", "true");
    }
  });
})();
