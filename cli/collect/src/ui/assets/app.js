// qloudpulse-collect's local UI. The pages work without this file; it only saves clicks: a select that picks a view
// submits itself, and the rules editor refreshes its preview, the file it would save and its errors as you type.
"use strict";
(function () {
  document.querySelectorAll("select[data-autosubmit]").forEach(function (s) {
    s.addEventListener("change", function () { s.form.submit(); });
  });

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
        (j.costs || []).forEach(function (c) {
          var el = document.getElementById("cost" + c.i);
          if (el) { el.textContent = c.text; el.className = "small " + c.class; }
        });
      })
      .catch(function () { /* the page still works: Preview and Save post the form */ });
  }
  function soon(ms) { clearTimeout(timer); timer = setTimeout(refresh, ms); }
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
