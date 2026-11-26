// qloudpulse-collect's local UI. The pages work without this file; it only saves clicks: a select that picks a view
// submits itself, the status strip keeps itself current (asking this server only), and the rules editor refreshes its
// preview, the file it would save and its errors as you type.
"use strict";
(function () {
  document.querySelectorAll("select[data-autosubmit]").forEach(function (s) {
    s.addEventListener("change", function () { s.form.submit(); });
  });

  // The status strip. The server writes every time in UTC; here each also gets the browser's local time, the next
  // run a countdown and a running cycle its seconds.
  function two(n) { return (n < 10 ? "0" : "") + n; }
  function span(secs) {
    secs = Math.max(0, Math.round(secs));
    var h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
    if (h > 0) return h + " h " + m + " min";
    if (m > 0) return m + " min " + two(s) + " s";
    return s + " s";
  }
  function clock() {
    document.querySelectorAll("time[data-local]").forEach(function (t) {
      var at = new Date(t.getAttribute("datetime"));
      if (isNaN(at)) return;
      if (!t.dataset.utc) t.dataset.utc = t.textContent;
      var text = t.dataset.utc + " (" + two(at.getHours()) + ":" + two(at.getMinutes()) + " your time)";
      var away = (at - Date.now()) / 1000;
      if (t.hasAttribute("data-countdown")) text += away > 0 ? ", in " + span(away) : ", now";
      if (t.hasAttribute("data-elapsed")) text += ", " + span(-away) + " ago";
      t.textContent = text;
    });
  }
  clock();
  setInterval(clock, 1000);
  var asking = false;
  function follow() {
    var strip = document.getElementById("status");
    if (!strip || asking || document.hidden) return;
    asking = true;
    fetch("/status", { credentials: "same-origin" })
      .then(function (r) { return r.ok ? r.text() : Promise.reject(r.status); })
      .then(function (html) {
        var was = strip.dataset.rev;
        var box = document.createElement("div");
        box.innerHTML = html; // rendered and escaped by the server
        var fresh = box.firstElementChild;
        if (!fresh) return;
        strip.replaceWith(fresh);
        clock();
        // The same words elsewhere on the page: the sample's line on Analyze, the running cycle's row in Run history.
        [["[data-sample-text]", "[data-sample-phase]"], ["[data-cycle-text]", "[data-cycle-phase]"]].forEach(function (p) {
          var from = fresh.querySelector(p[0]);
          document.querySelectorAll(p[1]).forEach(function (el) {
            if (from) el.textContent = from.textContent;
          });
        });
        // A cycle or the sample started or ended: a page that lists them is out of date.
        if (fresh.dataset.rev !== was && document.body.hasAttribute("data-live")) location.reload();
      })
      .catch(function () { /* the server stopped: the strip keeps what it last said */ })
      .then(function () { asking = false; });
  }
  if (document.getElementById("status")) setInterval(follow, 2000);

  var form = document.getElementById("rules-form");
  if (!form) return;
  var timer = null;
  var seq = 0;
  function refresh() {
    var mine = ++seq;
    fetch("/rules/preview", {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams(new FormData(form)),
    })
      .then(function (r) { return r.ok ? r.json() : Promise.reject(r.status); })
      .then(function (j) {
        if (mine !== seq) return; // an older answer arriving late
        ["preview", "toml", "errors"].forEach(function (id) {
          var el = document.getElementById(id);
          if (el) el.innerHTML = j[id]; // rendered and escaped by the server
        });
        var save = document.getElementById("save");
        if (save) save.disabled = !j.can_save;
        var download = document.getElementById("download");
        if (download) download.disabled = !j.valid;
        (j.costs || []).forEach(function (c) {
          var el = document.getElementById("cost" + c.i);
          if (el) { el.textContent = c.text; el.className = "small " + c.class; }
        });
      })
      .catch(function () { /* the page still works: Preview and Save post the form */ });
  }
  function soon(ms) { clearTimeout(timer); timer = setTimeout(refresh, ms); }
  // Copy needs a secure context: localhost and HTTPS have one; a bare IP over plain HTTP does not, so no button.
  var copy = document.getElementById("copy-toml");
  if (copy && navigator.clipboard && window.isSecureContext) {
    copy.classList.remove("hidden");
    copy.addEventListener("click", function () {
      var pre = document.querySelector("#toml pre");
      if (!pre) return;
      navigator.clipboard.writeText(pre.textContent).then(
        function () { copy.textContent = "Copied"; setTimeout(function () { copy.textContent = "Copy"; }, 1500); },
        function () {
          // The browser refused the clipboard: select the file, so the usual keys copy it.
          var range = document.createRange();
          range.selectNodeContents(pre);
          var sel = window.getSelection();
          sel.removeAllRanges();
          sel.addRange(range);
          copy.textContent = "Selected: press Ctrl+C or ⌘C";
        }
      );
    });
  }
  form.addEventListener("input", function () { soon(250); });
  form.addEventListener("change", function (e) {
    var t = e.target;
    if (t.matches && t.matches("select[data-replace-toggle]")) {
      var row = t.closest("tr");
      if (row) row.classList.toggle("is-replace", t.value === "replace");
    }
    soon(30);
  });
})();
