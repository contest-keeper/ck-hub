// Look (modern, classic) and mode (light, dark, system) are set before first paint, so the page never flashes the wrong one.
// The Settings button and its panel are built here, so every page gets the same one.
(function () {
  // Written in two parts: the page must hold no outside address as text; this is only a name the browser needs.
  const SVG_NS = "http" + "://www.w3.org/2000/svg";
  var root = document.documentElement;
  function read(key) { try { return localStorage.getItem(key); } catch (error) { return null; } }
  function save(key, value) { try { localStorage.setItem(key, value); } catch (error) { /* the choice lasts until the page closes */ } }
  function forget(key) { try { localStorage.removeItem(key); } catch (error) { /* the choice lasts until the page closes */ } }
  function dark() { return window.matchMedia && matchMedia("(prefers-color-scheme: dark)").matches; }
  // Read once; a choice is kept here first, so it holds for this page even when storage refuses it.
  var stored = read("ck-mode");
  var chosen = {
    look: read("ck-theme") === "classic" ? "classic" : "modern",
    mode: stored === "dark" || stored === "light" ? stored : "system",
  };
  function look() { return chosen.look; }
  function mode() { return chosen.mode; }
  function resolvedMode() { return chosen.mode === "system" ? (dark() ? "dark" : "light") : chosen.mode; }
  function choose(name, key, value) {
    chosen[name] = value;
    if (value === "system") forget(key); else save(key, value);
  }
  function apply() { root.dataset.look = look(); root.dataset.mode = resolvedMode(); }
  apply();
  var media = window.matchMedia && matchMedia("(prefers-color-scheme: dark)");
  if (media) {
    var onMedia = function () { if (chosen.mode === "system") apply(); };
    if (media.addEventListener) media.addEventListener("change", onMedia);
    else if (media.addListener) media.addListener(onMedia);
  }

  function make(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text) node.textContent = text;
    return node;
  }

  function option(label, note, preview, on, pick) {
    var button = make("button", "set-opt");
    button.type = "button";
    button.dataset.opt = label;
    button.setAttribute("aria-pressed", String(on));
    if (preview) { var p = make("span", "set-prev " + preview); p.appendChild(make("i")); p.appendChild(make("i")); button.appendChild(p); }
    button.appendChild(make("b", "", label));
    if (note) button.appendChild(make("span", "", note));
    button.addEventListener("click", pick);
    return button;
  }

  function build(slot) {
    var wrap = make("span", "set-wrap");
    var up = !!slot.closest(".side");
    var button = make("button", "btn sm set-btn");
    var icon = document.createElementNS(SVG_NS, "svg");
    icon.setAttribute("width", "16"); icon.setAttribute("height", "16"); icon.setAttribute("viewBox", "0 0 24 24"); icon.setAttribute("fill", "none");
    icon.setAttribute("stroke", "currentColor"); icon.setAttribute("stroke-width", "2"); icon.setAttribute("stroke-linecap", "round"); icon.setAttribute("aria-hidden", "true");
    var gear = document.createElementNS(SVG_NS, "path");
    gear.setAttribute("d", "M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M4.9 19.1 7 17M17 7l2.1-2.1");
    var hub = document.createElementNS(SVG_NS, "circle");
    hub.setAttribute("cx", "12"); hub.setAttribute("cy", "12"); hub.setAttribute("r", "3");
    icon.appendChild(hub); icon.appendChild(gear); button.appendChild(icon); button.appendChild(document.createTextNode("Settings"));
    button.type = "button";
    button.setAttribute("aria-haspopup", "dialog");
    button.setAttribute("aria-expanded", "false");
    var panel = null;

    function close(refocus) {
      if (!panel) return;
      panel.remove();
      panel = null;
      button.setAttribute("aria-expanded", "false");
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("click", onOutside, true);
      if (refocus) button.focus();
    }
    function onKey(event) { if (event.key === "Escape") close(true); }
    function onOutside(event) { if (!wrap.contains(event.target)) close(false); }

    function draw() {
      var again = panel;
      panel = make("div", "set-panel " + (up ? "up" : "down"));
      panel.setAttribute("role", "dialog");
      panel.setAttribute("aria-label", "Appearance settings");
      var head = make("div", "set-head");
      head.appendChild(make("h2", "", "Appearance"));
      head.appendChild(make("span", "grow"));
      var done = make("button", "btn", "Done");
      done.type = "button";
      done.setAttribute("aria-label", "Close settings");
      done.addEventListener("click", function () { close(true); });
      head.appendChild(done);
      panel.appendChild(head);
      var groups = [
        ["Look", [["Modern", "", "m", look() === "modern", function () { choose("look", "ck-theme", "modern"); change(); }],
                  ["Classic", "", "c", look() === "classic", function () { choose("look", "ck-theme", "classic"); change(); }]]],
        ["Theme", [["Light", "", "", mode() === "light", function () { choose("mode", "ck-mode", "light"); change(); }],
                   ["Dark", "", "", mode() === "dark", function () { choose("mode", "ck-mode", "dark"); change(); }],
                   ["System", "", "", mode() === "system", function () { choose("mode", "ck-mode", "system"); change(); }]]],
      ];
      groups.forEach(function (group) {
        var box = make("div");
        box.setAttribute("role", "group");
        box.setAttribute("aria-label", group[0]);
        box.appendChild(make("h3", "", group[0]));
        var opts = make("div", "set-opts" + (group[1].length === 3 ? " three" : ""));
        group[1].forEach(function (o) { opts.appendChild(option(o[0], o[1], o[2], o[3], o[4])); });
        box.appendChild(opts);
        panel.appendChild(box);
      });
      panel.appendChild(make("p", "hint", "Saved on this computer."));
      if (again) again.replaceWith(panel); else wrap.appendChild(panel);
    }
    function change() {
      var focused = document.activeElement && document.activeElement.dataset && document.activeElement.dataset.opt;
      apply();
      draw();
      var back = focused && panel.querySelector("button[data-opt='" + focused + "']");
      if (back) back.focus();
    }

    button.addEventListener("click", function () {
      if (panel) { close(true); return; }
      draw();
      button.setAttribute("aria-expanded", "true");
      document.addEventListener("keydown", onKey);
      document.addEventListener("click", onOutside, true);
      panel.querySelector("button[aria-pressed='true']").focus();
    });
    wrap.appendChild(button);
    slot.replaceWith(wrap);
  }

  document.addEventListener("DOMContentLoaded", function () {
    Array.prototype.forEach.call(document.querySelectorAll("[data-settings]"), build);
  });
})();
