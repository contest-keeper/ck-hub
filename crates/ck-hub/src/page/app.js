// Written in two parts: the page must hold no outside address as text; this is only a name the browser needs.
const SVG_NS = "http" + "://www.w3.org/2000/svg";
const REFRESH_MS = 5000;
let sticks = [];
let filter = "all";
let query = "";
let org = null;
let selected = null;

function ago(seconds) {
  if (seconds < 5) return "just now";
  if (seconds < 90) return seconds + " s ago";
  if (seconds < 5400) return Math.round(seconds / 60) + " min ago";
  return Math.round(seconds / 3600) + " h ago";
}

function span(seconds) {
  if (seconds < 90) return seconds + " s";
  if (seconds < 5400) return Math.round(seconds / 60) + " min";
  return (seconds / 3600).toFixed(1) + " h";
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function cell(row, text, className) {
  const td = el("td", className, text);
  row.appendChild(td);
  return td;
}

const isArchived = (stick) => !!stick.archived_at;
const neverSeen = (stick) => !stick.last;
const isOffline = (stick) => !stick.online && !!stick.last;
const needsLook = (stick) => !isArchived(stick) && (stick.warnings.length > 0 || isOffline(stick));

// What each warning means in plain words and what to do about it. The Hub's warnings are free text,
// so a warning it does not know gets a general pointer.
function fixFor(warning) {
  if (/different users/.test(warning)) return "Ask the team who was logged in. If it was not planned, tell the contest organizer.";
  if (/two machines/.test(warning)) return "Check that the stick was not copied to another disk. If it was, make a new stick in Keeper Forge.";
  if (/^Settings refused/.test(warning)) return "Make this stick again in Keeper Forge if its settings are wrong.";
  if (/^Settings changed/.test(warning)) return "Someone may have edited the stick. Make it again in Keeper Forge if it should not have changed.";
  return "Open the details to see more.";
}

function issuesOf(stick) {
  if (isArchived(stick)) return [];
  const list = [];
  if (isOffline(stick)) {
    list.push({ text: "Offline for " + span(stick.last.seconds_ago), bad: true,
      fix: "Check the power cable and the network at " + stick.last.hostname + ". It reconnects by itself." });
  }
  for (const warning of stick.warnings) list.push({ text: warning, fix: fixFor(warning), bad: false });
  return list;
}

const teamName = (stick) => stick.team || "Stick " + stick.stick_id;

// Problems first, then offline sticks, then the rest by machine number.
function rank(stick) {
  if (needsLook(stick)) return 0;
  if (isOffline(stick)) return 1;
  if (neverSeen(stick)) return 2;
  return 3;
}

function matches(stick) {
  // Archived sticks have a place of their own; everywhere else they are out of the way.
  if (filter === "archived") return isArchived(stick) && matchesQuery(stick);
  if (isArchived(stick)) return false;
  return matchesFilter(stick) && matchesQuery(stick);
}

function matchesFilter(stick) {
  if (filter === "attn" && !needsLook(stick)) return false;
  if (filter === "offline" && !isOffline(stick)) return false;
  if (filter === "new" && !neverSeen(stick)) return false;
  return true;
}

function matchesQuery(stick) {
  if (!query) return true;
  const text = [stick.team, stick.stick_id, stick.site, stick.contest, stick.machine_number, stick.current_user,
    stick.last ? stick.last.hostname : ""].join(" ").toLowerCase();
  return text.includes(query);
}

function bytesText(n) {
  if (n < 1024) return n + " B";
  if (n < 1048576) return (n / 1024).toFixed(1) + " KB";
  return (n / 1048576).toFixed(1) + " MB";
}

// What each stick's proxy carried and refused, fetched when its details are open and again when
// the stick reports more.
const histories = new Map();
const onlyRefused = new Set();

async function fetchHistory(stick) {
  const before = histories.get(stick.stick_id);
  histories.set(stick.stick_id, { requests: stick.network_requests, data: before ? before.data : null, failed: false });
  let data = before ? before.data : null;
  let failed = false;
  try {
    const response = await fetch("/api/v1/orgs/" + encodeURIComponent(org) + "/sticks/" + encodeURIComponent(stick.stick_id) + "/network", { cache: "no-store" });
    if (!response.ok) throw new Error("HTTP " + response.status);
    data = await response.json();
  } catch (error) {
    failed = true;
  }
  histories.set(stick.stick_id, { requests: stick.network_requests, data, failed });
  render();
}

function table(head, rows) {
  const scroll = el("div", "scroll");
  const grid = el("table");
  const top = el("tr");
  for (const name of head) top.appendChild(el("th", "", name));
  grid.appendChild(top);
  for (const [cells, className] of rows) {
    const row = el("tr", className);
    cells.forEach((text, at) => cell(row, text, at === 1 && head[1] === "Address" ? "where mono" : ""));
    grid.appendChild(row);
  }
  scroll.appendChild(grid);
  return scroll;
}

// What the stick said was submitted, next to what Codeforces says the same people submitted.
const proofs = new Map();

async function fetchProof(stick) {
  const before = proofs.get(stick.stick_id);
  proofs.set(stick.stick_id, { at: Date.now(), version: stick.submissions + ":" + stick.network_requests, data: before ? before.data : null, failed: false });
  let data = before ? before.data : null;
  let failed = false;
  try {
    const response = await fetch("/api/v1/orgs/" + encodeURIComponent(org) + "/sticks/" + encodeURIComponent(stick.stick_id) + "/submissions", { cache: "no-store" });
    if (!response.ok) throw new Error("HTTP " + response.status);
    data = await response.json();
  } catch (error) {
    failed = true;
  }
  proofs.set(stick.stick_id, { at: Date.now(), version: stick.submissions + ":" + stick.network_requests, data, failed });
  render();
}

// Written in two parts: the page must hold no outside address as text; these are only links people click.
const CODEFORCES = "https" + "://codeforces.com";

function link(text, address) {
  const a = el("a", "", text);
  a.href = address;
  a.target = "_blank";
  a.rel = "noopener noreferrer";
  return a;
}

// A handle is shown as a link only when it is made of what a Codeforces handle is made of.
function handleLink(handle) {
  if (!handle) return document.createTextNode("—");
  if (!/^[A-Za-z0-9_.-]{1,24}$/.test(handle)) return document.createTextNode(handle);
  return link(handle, CODEFORCES + "/profile/" + handle);
}

function contestLink(contest) {
  return contest ? link(String(contest), CODEFORCES + "/contest/" + contest) : document.createTextNode("—");
}

function submissionLink(contest, id) {
  return link("#" + id, CODEFORCES + "/contest/" + contest + "/submission/" + id);
}

function proofTable(head, rows) {
  const scroll = el("div", "scroll");
  const grid = el("table");
  const top = el("tr");
  for (const name of head) top.appendChild(el("th", "", name));
  grid.appendChild(top);
  for (const [cells, className] of rows) {
    const row = el("tr", className);
    for (const content of cells) {
      const td = el("td");
      td.appendChild(typeof content === "string" ? document.createTextNode(content) : content);
      row.appendChild(td);
    }
    grid.appendChild(row);
  }
  scroll.appendChild(grid);
  return scroll;
}

function proofPanel(stick) {
  const box = el("div", "net");
  box.appendChild(el("h3", "", "Submissions"));
  const cached = proofs.get(stick.stick_id);
  const version = stick.submissions + ":" + stick.network_requests;
  if (!cached || cached.version !== version || Date.now() - cached.at > 30000) fetchProof(stick);
  if (!cached || !cached.data) {
    box.appendChild(el("p", "muted", cached && cached.failed ? "The submissions could not be loaded." : "Loading…"));
    return box;
  }
  const data = cached.data;
  const who = data.handles.length ? " by " + data.handles.join(", ") : "";
  const summary = el("p", data.suspect ? "bad" : "", data.submissions.length + " reported by the stick" + who + ". " +
    (data.suspect ? data.suspect + " on Codeforces not from this stick: SUSPECT." : "Every Codeforces submission seen was made from this stick."));
  box.appendChild(summary);
  if (data.judge_note) box.appendChild(el("p", "hint", data.judge_note));

  if (data.submissions.length) {
    box.appendChild(el("h4", "", "Reported by the stick"));
    box.appendChild(proofTable(["Time", "User", "Contest", "Problem", "Language", "Source", "On Codeforces"],
      data.submissions.map((s) => [[
        new Date(s.received_at).toLocaleTimeString(),
        handleLink(s.handle),
        contestLink(s.contest_id),
        s.problem || "—",
        s.language || "—",
        s.source_sha256 ? s.source_sha256 + (s.source_bytes ? " (" + s.source_bytes + " B" + (s.source_cut ? ", cut" : "") + ")" : "") : "—",
        !s.accepted ? "refused by the site" : s.judge_id ? submissionLink(s.contest_id, s.judge_id) : "not found",
      ], !s.accepted ? "" : s.judge_id ? "ok" : "unconfirmed"])));
  }
  if (data.judge.length) {
    box.appendChild(el("h4", "", "Seen on Codeforces"));
    box.appendChild(proofTable(["Time", "User", "Contest", "Problem", "Submission", "From this stick"],
      data.judge.map((j) => [[
        new Date(j.submitted_at).toLocaleTimeString(),
        handleLink(j.handles[0]),
        contestLink(j.contest_id),
        j.problem,
        submissionLink(j.contest_id, j.submission_id),
        j.suspect ? "SUSPECT: the stick did not report it" : "yes",
      ], j.suspect ? "refused" : "ok"])));
  }
  return box;
}

function networkPanel(stick) {
  const box = el("div", "net");
  box.appendChild(el("h3", "", "Network history"));
  if (!stick.network_requests) {
    box.appendChild(el("p", "muted", "Nothing reported yet. The stick sends what its proxy carried or refused every few seconds."));
    return box;
  }
  const cached = histories.get(stick.stick_id);
  if (!cached || cached.requests !== stick.network_requests) fetchHistory(stick);
  if (!cached || !cached.data) {
    box.appendChild(el("p", "muted", cached && cached.failed ? "The history could not be loaded." : "Loading…"));
    return box;
  }
  const data = cached.data;
  box.appendChild(el("p", "", data.requests + " requests, " + data.blocked + " refused, " + bytesText(data.bytes) + " carried. " + data.hidden + " background requests (files a page loads, tunnels, browser traffic) are not listed."));

  box.appendChild(table(["Site", "Requests", "Refused", "Data"],
    data.hosts.map((h) => [[h.host || "—", String(h.requests), String(h.blocked), bytesText(h.bytes)], h.blocked ? "refused" : ""])));

  const tools = el("label", "tools");
  const check = el("input");
  check.type = "checkbox";
  check.checked = onlyRefused.has(stick.stick_id);
  check.addEventListener("change", () => {
    if (check.checked) onlyRefused.add(stick.stick_id); else onlyRefused.delete(stick.stick_id);
    render();
  });
  tools.appendChild(check);
  tools.appendChild(el("span", "", "Only refused requests"));
  box.appendChild(tools);
  const shown = data.entries.filter((e) => !check.checked || e.blocked);
  box.appendChild(table(["Time", "Address", "Method", "Answer", "Data", "Took"],
    shown.map((e) => [[
      new Date(e.t * 1000).toLocaleTimeString(),
      e.host + e.path + (e.count > 1 ? "  x" + e.count : ""),
      e.method,
      e.blocked ? "refused" : String(e.status),
      bytesText(e.bytes),
      e.ms + " ms",
    ], e.blocked ? "refused" : ""])));
  if (data.requests > data.entries.length) {
    box.appendChild(el("p", "hint", "The newest " + data.entries.length + " requests are listed. The totals count all of them."));
  }
  return box;
}

// Commands to a stick for admins and owners, and what became of each command sent. The text typed
// and the step the person is at are kept here, because the list is drawn again at every refresh.
const commandLists = new Map();
const commandDrafts = new Map();
const ACTION_NAMES = { message: "Message", lock_screen: "Lock", unlock_screen: "Unlock", shutdown: "Shutdown" };

function commandPath(stick) {
  return "/api/v1/orgs/" + encodeURIComponent(org) + "/sticks/" + encodeURIComponent(stick.stick_id) + "/commands";
}

function draftOf(stick) {
  let draft = commandDrafts.get(stick.stick_id);
  if (!draft) {
    draft = { action: null, text: "", busy: false, note: "", bad: false, archiving: false };
    commandDrafts.set(stick.stick_id, draft);
  }
  return draft;
}

async function fetchCommands(stick) {
  const before = commandLists.get(stick.stick_id);
  commandLists.set(stick.stick_id, { at: Date.now(), data: before ? before.data : null, failed: false });
  let data = before ? before.data : null;
  let failed = false;
  try {
    const response = await fetch(commandPath(stick), { cache: "no-store" });
    if (!response.ok) throw new Error("HTTP " + response.status);
    data = await response.json();
  } catch (error) {
    failed = true;
  }
  commandLists.set(stick.stick_id, { at: Date.now(), data, failed });
  render();
}

async function sendCommand(stick, draft) {
  draft.busy = true;
  render();
  const action = draft.action === "message" ? { type: "message", text: draft.text.trim() } : { type: draft.action };
  const result = await call("POST", commandPath(stick), { action });
  draft.busy = false;
  if (result.ok) {
    draft.action = null;
    draft.text = "";
    draft.note = "Sent. The stick takes it with its next heartbeat.";
    draft.bad = false;
    commandLists.delete(stick.stick_id);
  } else {
    draft.note = (result.data && result.data.error) || "The command was not sent (HTTP " + result.status + ").";
    draft.bad = true;
  }
  render();
}

function commandText(command) {
  const action = command.action || {};
  if (action.type === "message") return "Message: " + action.text;
  return ACTION_NAMES[action.type] || String(action.type);
}

function commandState(command) {
  if (command.action?.type === "unlock_screen" && command.state === "expired") {
    return command.delivered_at ? "expired without acknowledgement" : "expired before delivery";
  }
  return command.refusal ? command.state + " (" + command.refusal.replaceAll("_", " ") + ")" : command.state;
}

// A button of the details panel. The key lets the refresh give the focus back to it.
function act(label, key, onClick, className) {
  const button = el("button", className || "btn sm", label);
  button.type = "button";
  button.dataset.key = "act:" + key;
  button.addEventListener("click", onClick);
  return button;
}

function actionRow(title, hint, button) {
  const item = el("li");
  const text = el("div");
  text.appendChild(el("b", "", title));
  text.appendChild(el("div", "hint", hint));
  item.appendChild(text);
  item.appendChild(button);
  return item;
}

// Actions on one stick. Commands need the stick online; the reason is said, not only shown by a grey button.
function commandPanel(stick) {
  const box = el("div", "stack");
  box.appendChild(el("h3", "", "Actions"));
  const name = teamName(stick) + " " + (stick.machine_number ?? "");
  const admin = myRole === "owner" || myRole === "admin";
  const draft = draftOf(stick);
  if (isArchived(stick)) {
    box.appendChild(el("p", "", "Archived. The Hub deletes this stick and everything kept about it on " +
      new Date(stick.delete_after).toLocaleDateString() + "."));
    if (!admin) return box;
    const bar = el("div", "cmd-bar");
    bar.appendChild(act("Restore", "restore", () => changeStick(stick, "POST", "/restore", null, "Stick restored.")));
    bar.appendChild(act("Delete now…", "delete", () => {
      confirmingDelete.add(stick.stick_id);
      render();
    }, "btn sm danger"));
    box.appendChild(bar);
    if (confirmingDelete.has(stick.stick_id)) {
      const ask = el("div", "cmd-confirm");
      ask.appendChild(el("p", "bad", "Delete " + name + " and everything kept about it? This cannot be undone."));
      const buttons = el("div", "cmd-bar");
      buttons.appendChild(act("Delete everything", "delete-yes", () => changeStick(stick, "DELETE", "", null, "Stick deleted."), "btn sm danger"));
      buttons.appendChild(act("Cancel", "delete-no", () => { confirmingDelete.delete(stick.stick_id); render(); }));
      ask.appendChild(buttons);
      box.appendChild(ask);
    }
    return box;
  }
  if (!admin) {
    box.appendChild(el("p", "muted", "Only an admin or an owner can send commands or archive a stick."));
    return box;
  }
  const supportsUnlock = stick.last?.screen_unlock === true;
  if (!supportsUnlock && draft.action === "unlock_screen") draft.action = null;
  const list = el("ul", "list");
  const rows = [
    ["lock_screen", "Lock screen", "Shows a lock screen to the contestant until you unlock.", "Lock…"],
    ["unlock_screen", "Unlock screen", "Lets the contestant use the computer again.", "Unlock…"],
    ["message", "Message", "Shows a short message on the contestant's screen.", "Send message…"],
    ["shutdown", "Shut down", "Turns the computer off. Unsaved work is lost, and someone must switch it on again.", "Shut down…"],
  ];
  for (const [action, title, hint, label] of rows) {
    if (action === "unlock_screen" && !supportsUnlock) continue;
    const button = act(label, action, () => {
      draft.action = action;
      draft.archiving = false;
      draft.note = "";
      render();
    });
    button.disabled = draft.busy || !stick.online;
    if (!stick.online) button.setAttribute("aria-describedby", "cmd-off");
    list.appendChild(actionRow(title, hint, button));
  }
  list.appendChild(actionRow("Archive", "Takes it out of the list. It is deleted with its data when the time is up, and you can restore it before then.",
    act("Archive…", "archive", () => { draft.archiving = true; draft.action = null; render(); }, "btn sm danger")));
  box.appendChild(list);
  if (!stick.online) box.appendChild(el("p", "hint", "Lock, Unlock, Message and Shut down work when the stick is online. This one is not.")).id = "cmd-off";

  if (draft.action) {
    const ask = el("div", "cmd-confirm");
    if (draft.action === "message") {
      const input = el("textarea");
      input.maxLength = 500;
      input.rows = 2;
      input.value = draft.text;
      input.dataset.key = "msg";
      input.setAttribute("aria-label", "Message for " + name);
      input.addEventListener("input", () => {
        draft.text = input.value;
        yes.disabled = draft.busy || !draft.text.trim();
      });
      ask.appendChild(input);
    }
    const what = {
      message: "Show this message on ",
      lock_screen: "Lock the screen of ",
      unlock_screen: "Unlock the screen of ",
      shutdown: "Shut down ",
    }[draft.action];
    ask.appendChild(el("p", draft.action === "shutdown" ? "bad" : "", what + name + "?" +
      (draft.action === "shutdown" ? " The contestant loses any unsaved work." : "")));
    var yes = act("Confirm", "confirm", () => sendCommand(stick, draft), "btn sm pri");
    yes.disabled = draft.busy || (draft.action === "message" && !draft.text.trim());
    const buttons = el("div", "cmd-bar");
    buttons.appendChild(yes);
    buttons.appendChild(act("Cancel", "cancel", () => { draft.action = null; render(); }));
    ask.appendChild(buttons);
    box.appendChild(ask);
  }
  if (draft.archiving) {
    const ask = el("div", "cmd-confirm");
    ask.appendChild(el("p", "", "Archive " + name + "? It stops reporting and is deleted with all its data when the time is up."));
    const buttons = el("div", "cmd-bar");
    for (const days of [7, 30]) {
      buttons.appendChild(act("Archive for " + days + " days", "archive-" + days, () => {
        draft.archiving = false;
        changeStick(stick, "POST", "/archive", { days }, "Stick archived.");
      }, "btn sm danger"));
    }
    buttons.appendChild(act("Cancel", "archive-no", () => { draft.archiving = false; render(); }));
    ask.appendChild(buttons);
    box.appendChild(ask);
  }
  if (draft.note) box.appendChild(el("p", draft.bad ? "bad" : "hint", draft.note));
  return box;
}

// What was sent to this stick, and what became of it.
function commandHistory(stick) {
  const box = el("div", "net");
  box.appendChild(el("h3", "", "Commands sent"));
  const cached = commandLists.get(stick.stick_id);
  const open = cached && cached.data && cached.data.some((c) => c.state === "queued" || c.state === "delivered");
  if (!cached || Date.now() - cached.at > (open ? 4000 : 30000)) fetchCommands(stick);
  if (!cached || !cached.data) {
    box.appendChild(el("p", "muted", cached && cached.failed ? "The commands could not be loaded." : "Loading…"));
    return box;
  }
  if (!cached.data.length) {
    box.appendChild(el("p", "muted", "No command has been sent to this stick."));
    return box;
  }
  box.appendChild(table(["Time", "Command", "By", "State"],
    cached.data.map((c) => [[
      new Date(c.created_at).toLocaleTimeString(),
      commandText(c),
      c.sent_by,
      commandState(c),
    ], c.state === "refused" || c.state === "failed" ? "refused" : ""])));
  return box;
}

// Recordings a stick uploaded: a list with a Watch link for each finished file. The link goes to the
// Hub, which sends a member of the organization on to a read-only link that works for a couple of
// hours; the file opens in its own tab.
const recordingLists = new Map();

async function fetchRecordings(stick) {
  const before = recordingLists.get(stick.stick_id);
  recordingLists.set(stick.stick_id, { at: Date.now(), data: before ? before.data : null, failed: false });
  let data = before ? before.data : null;
  let failed = false;
  try {
    const response = await fetch("/api/v1/orgs/" + encodeURIComponent(org) + "/sticks/" + encodeURIComponent(stick.stick_id) + "/recordings", { cache: "no-store" });
    if (!response.ok) throw new Error("HTTP " + response.status);
    data = await response.json();
  } catch (error) {
    failed = true;
  }
  recordingLists.set(stick.stick_id, { at: Date.now(), data, failed });
  render();
}

function recordingPanel(stick) {
  const box = el("div", "net");
  box.appendChild(el("h3", "", "Recordings"));
  const cached = recordingLists.get(stick.stick_id);
  if (!cached || Date.now() - cached.at > 30000) fetchRecordings(stick);
  if (!cached || !cached.data) {
    box.appendChild(el("p", "muted", cached && cached.failed ? "The recordings could not be loaded." : "Loading…"));
    return box;
  }
  if (!cached.data.length) {
    box.appendChild(el("p", "muted", "No recording has been uploaded from this stick. Recordings stay on the stick unless the profile says to upload them."));
    return box;
  }
  const watch = (r) => {
    if (!r.complete) return document.createTextNode("uploading " + r.chunks_received + " of " + r.chunks_total);
    return link("Watch", "/api/v1/orgs/" + encodeURIComponent(org) + "/recordings/" + encodeURIComponent(r.id) + "/view");
  };
  box.appendChild(proofTable(["Time", "File", "Kind", "Length", "Size", "Kept until", ""],
    cached.data.map((r) => [[
      new Date(r.started_at || r.created_at).toLocaleString(),
      r.file,
      r.kind === "video" ? "video" : "picture",
      r.duration_s ? span(r.duration_s) : "—",
      bytesText(r.size),
      r.expires_at ? new Date(r.expires_at).toLocaleDateString() : "—",
      watch(r),
    ], ""])));
  return box;
}

function facts(stick) {
  const last = stick.last;
  const list = el("dl", "facts");
  const add = (name, value) => {
    const pair = el("div");
    pair.appendChild(el("dt", "", name));
    pair.appendChild(el("dd", "", value));
    list.appendChild(pair);
  };
  add("Contest", stick.contest);
  add("Site", stick.site);
  add("Computer", last ? last.hostname : "—");
  add("Up for", last ? span(last.uptime_s) : "—");
  add("Recorder", last ? last.recorder : "—");
  add("Heartbeats", last ? String(last.heartbeats) : "—");
  add("Image", last ? last.image_version : "—");
  const disk = stick.hardware;
  if (disk) {
    add("USB disk", (disk.serial_hint ? "serial ending " + disk.serial_hint + ", " : "") + "id " + disk.id);
    add("Written", disk.write_count === 1 ? "once" : disk.write_count + " times (rewritten)");
  }
  add("Events", String(stick.events));
  add("Requests", String(stick.network_requests));
  add("Refused", String(stick.blocked_requests));
  add("Submissions", String(stick.submissions));
  const sub = stick.last_submission;
  if (sub) {
    const parts = [sub.problem, sub.language, sub.submission_id ? "#" + sub.submission_id : null].filter(Boolean);
    add("Last submission", (parts.length ? parts.join(", ") + ", " : "") + new Date(sub.received_at).toLocaleTimeString());
  }
  return list;
}

// Sticks whose "Delete now…" was clicked: the next click on "Delete everything" deletes.
const confirmingDelete = new Set();

async function changeStick(stick, method, suffix, body, done) {
  const path = "/api/v1/orgs/" + encodeURIComponent(org) + "/sticks/" + encodeURIComponent(stick.stick_id) + suffix;
  const result = await call(method, path, body);
  confirmingDelete.delete(stick.stick_id);
  if (result.ok) {
    notice(done, true);
    await refresh();
  } else {
    notice((result.data && result.data.error) || "The Hub refused (" + result.status + ").", false);
    render();
  }
}

const ICON_WARN = "M12 3 2 20h20L12 3zM12 10v4M12 17.5v.5";

function icon(path) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "ico");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  const shape = document.createElementNS(SVG_NS, "path");
  shape.setAttribute("d", path);
  svg.appendChild(shape);
  return svg;
}

function statusPill(stick) {
  if (isArchived(stick)) return el("span", "pill new", "Archived");
  if (stick.online) return el("span", "pill on", "Online");
  return stick.last ? el("span", "pill off", "Offline") : el("span", "pill new", "Not seen yet");
}

function selectStick(id, scroll) {
  selected = id;
  render();
  if (scroll) document.getElementById("detail").scrollIntoView({ block: "nearest" });
}

function detailPanel(stick) {
  const box = document.getElementById("detail");
  box.replaceChildren();
  if (!stick) {
    box.appendChild(el("p", "muted", "Select a stick in the list to see its details and actions."));
    return;
  }
  const head = el("div", "row");
  head.appendChild(el("h2", "", teamName(stick)));
  head.appendChild(el("span", "grow"));
  head.appendChild(statusPill(stick));
  box.appendChild(head);
  for (const issue of issuesOf(stick)) {
    const note = el("div", "attn" + (issue.bad ? " bad" : ""));
    const why = el("div", "why");
    why.appendChild(el("b", "", issue.text));
    why.appendChild(el("span", "", issue.fix));
    note.appendChild(why);
    box.appendChild(note);
  }
  if (!isArchived(stick) && neverSeen(stick)) {
    const note = el("div", "attn");
    const why = el("div", "why");
    why.appendChild(el("b", "", "Not seen yet"));
    why.appendChild(el("span", "", "Plug the stick in and start the computer once. It appears here by itself."));
    note.appendChild(why);
    box.appendChild(note);
  }
  box.appendChild(facts(stick));
  box.appendChild(commandPanel(stick));
}

function morePanel(stick) {
  const box = document.getElementById("more");
  box.hidden = !stick;
  box.replaceChildren();
  if (!stick) return;
  box.appendChild(el("h2", "", "More about " + teamName(stick)));
  box.appendChild(commandHistory(stick));
  box.appendChild(proofPanel(stick));
  box.appendChild(networkPanel(stick));
  box.appendChild(recordingPanel(stick));
}

// The list is drawn again at every refresh: give the focus back to what had it, and keep the cursor
// in a message being typed.
function keepFocus(draw) {
  const active = document.activeElement;
  const key = active && active.dataset ? active.dataset.key : null;
  const at = active && active.selectionStart !== undefined ? active.selectionStart : null;
  draw();
  if (!key) return;
  const again = Array.from(document.querySelectorAll("[data-key]")).find((n) => n.dataset.key === key);
  if (!again || again.disabled) return;
  again.focus();
  if (at !== null && again.setSelectionRange) again.setSelectionRange(at, at);
}

function render() {
  keepFocus(drawSticks);
}

function drawSticks() {
  const body = document.getElementById("rows");
  body.replaceChildren();
  const counts = { all: 0, online: 0, offline: 0, attn: 0, new: 0, archived: 0 };
  for (const stick of sticks) {
    if (isArchived(stick)) { counts.archived += 1; continue; }
    counts.all += 1;
    if (stick.online) counts.online += 1;
    if (isOffline(stick)) counts.offline += 1;
    if (needsLook(stick)) counts.attn += 1;
    if (neverSeen(stick)) counts.new += 1;
  }
  document.getElementById("n-online").textContent = counts.online;
  document.getElementById("n-of").textContent = "of " + counts.all + " online";
  document.getElementById("hero-note").textContent = counts.all === 0
    ? "No sticks yet. Make one with Keeper Forge and it appears here."
    : (counts.attn ? counts.attn + (counts.attn === 1 ? " stick needs" : " sticks need") + " a look." : "Nothing needs a look.") +
      (counts.new ? " " + counts.new + " not seen yet." : "");
  for (const name of ["all", "attn", "offline", "new", "archived"]) {
    document.getElementById("c-" + name).textContent = counts[name];
  }
  const navCount = document.getElementById("nav-count");
  navCount.hidden = counts.attn === 0;
  navCount.textContent = counts.attn;
  navCount.setAttribute("aria-label", counts.attn + " need a look");

  const list = document.getElementById("attn-list");
  list.replaceChildren();
  const problems = sticks.filter(needsLook).sort((a, b) => (a.machine_number ?? 0) - (b.machine_number ?? 0));
  list.hidden = problems.length === 0;
  for (const stick of problems) {
    const issue = issuesOf(stick)[0];
    const item = el("div", "attn" + (issue.bad ? " bad" : ""));
    item.appendChild(icon(ICON_WARN));
    const why = el("div", "why");
    why.appendChild(el("b", "", teamName(stick) + ": " + issue.text));
    why.appendChild(el("span", "", issue.fix));
    item.appendChild(why);
    const view = act("View", "view-" + stick.stick_id, () => selectStick(stick.stick_id, true));
    view.setAttribute("aria-label", "View " + teamName(stick));
    item.appendChild(view);
    list.appendChild(item);
  }

  const shown = sticks.filter(matches).sort((a, b) =>
    rank(a) - rank(b) || (a.machine_number ?? 0) - (b.machine_number ?? 0));
  for (const stick of shown) {
    const row = el("tr", (needsLook(stick) ? "flag " : "") + (stick.stick_id === selected ? "sel" : ""));
    const team = cell(row, "");
    const pick = el("button", "rowbtn", teamName(stick));
    pick.type = "button";
    pick.dataset.key = "row:" + stick.stick_id;
    pick.setAttribute("aria-label", "Show details for " + teamName(stick));
    if (stick.stick_id === selected) pick.setAttribute("aria-current", "true");
    pick.addEventListener("click", () => selectStick(stick.stick_id, true));
    team.appendChild(pick);
    team.appendChild(el("span", "sub", [stick.machine_number ? "Machine " + stick.machine_number : "", stick.last ? stick.last.hostname : stick.site].filter(Boolean).join(" · ")));
    cell(row, "").appendChild(statusPill(stick));
    cell(row, stick.current_user ?? "", stick.current_user ? "" : "muted");
    cell(row, stick.last ? ago(stick.last.seconds_ago) : "Never", stick.last ? "" : "muted");
    body.appendChild(row);
  }
  const empty = document.getElementById("empty");
  empty.hidden = shown.length > 0;
  empty.textContent = sticks.length
    ? "No stick matches. Clear the search or pick All."
    : "No sticks yet. Make one with Keeper Forge and it appears here.";
  document.getElementById("list-hint").textContent = "Showing " + shown.length + " of " +
    (filter === "archived" ? counts.archived : counts.all) + " sticks. Problems are listed first.";

  if (page !== "sticks") return;
  const current = sticks.find((s) => s.stick_id === selected);
  detailPanel(current);
  morePanel(current);
}

// Keeper Forge opens /forge/connect?port=..&state=.. . The port and the state are only passed on:
// the Hub checks them and builds the address the browser goes back to.
const forgeAsk = location.pathname === "/forge/connect" ? new URLSearchParams(location.search) : null;
const page = forgeAsk ? "forge" : location.pathname === "/download/image" ? "downloads" : location.pathname === "/people" ? "people" : "sticks";
const imageStatus = document.getElementById("image-download-status");
const imageRetry = document.getElementById("image-download-retry");
const imageOlder = document.getElementById("image-older");
const imageList = document.getElementById("image-download-list");
const imageOlderList = document.getElementById("image-older-list");
const TITLES = { login: "Log in", forge: "Connect Keeper Forge", sticks: "Sticks", people: "People", downloads: "Downloads" };

// What organizers call each Linux; a family not listed shows its own name capitalised.
const LINUX_NAMES = { ubuntu: "Ubuntu", debian: "Debian", mint: "Linux Mint" };
const isDevBuild = (image) => image.version.includes("-");
let shaCount = 0;

async function copyText(input, button) {
  try {
    await navigator.clipboard.writeText(input.value);
    imageStatus.textContent = "SHA-256 copied.";
    button.textContent = "Copied";
    setTimeout(() => { button.textContent = "Copy"; }, 2000);
  } catch (error) {
    input.select();
    imageStatus.textContent = "Press Ctrl+C to copy the selected SHA-256.";
  }
}

function imageItem(image) {
  const item = el("section", "card");
  const linux = LINUX_NAMES[image.family] || image.family.charAt(0).toUpperCase() + image.family.slice(1);
  const recommended = image.latest && !isDevBuild(image);
  const head = el("div", "row");
  head.appendChild(el("h2", "", linux + " · " + image.version));
  if (recommended) head.appendChild(el("span", "pill on", "Recommended"));
  if (isDevBuild(image)) head.appendChild(el("span", "pill warn", "Development build"));
  head.appendChild(el("span", "grow"));
  head.appendChild(el("span", "muted", bytesText(image.size)));
  item.appendChild(head);
  if (recommended) item.appendChild(el("p", "muted", "The stable release. Choose this unless support asks for another version."));
  if (isDevBuild(image)) item.appendChild(el("p", "muted", "For testing new features. Do not use it for a real contest."));
  item.appendChild(el("p", "muted", [image.name, image.profile, image.filename].filter(Boolean).join(" · ")));
  const field = el("div", "field");
  const id = "sha-" + (shaCount += 1);
  const label = el("label", "", "SHA-256 (to check the file)");
  label.htmlFor = id;
  field.appendChild(label);
  const line = el("div", "row");
  const input = el("input", "input mono");
  input.id = id;
  input.readOnly = true;
  input.value = image.sha256;
  line.appendChild(input);
  const copy = el("button", "btn sm", "Copy");
  copy.type = "button";
  copy.setAttribute("aria-label", "Copy the SHA-256 of " + linux + " " + image.version);
  copy.addEventListener("click", () => copyText(input, copy));
  line.appendChild(copy);
  field.appendChild(line);
  item.appendChild(field);
  const actions = el("div", "row");
  actions.appendChild(link("Download image", image.url)).className = "btn" + (recommended ? " pri" : "");
  if (image.source_offer_url) actions.appendChild(link("Source code offer", image.source_offer_url)).className = "btn";
  item.appendChild(actions);
  return item;
}

async function loadImageDownloads() {
  imageRetry.hidden = true;
  imageStatus.textContent = "Loading images…";
  try {
    const result = await call("GET", "/api/v1/images");
    if (result.status === 401) { location.reload(); return; }
    const images = result.data?.images;
    if (!result.ok || !Array.isArray(images)) throw new Error("No image list");
    imageStatus.textContent = images.length ? "" : "No Keeper OS images are available yet.";
    const newest = images.filter((image) => image.latest);
    const older = images.filter((image) => !image.latest);
    imageList.replaceChildren(...newest.map(imageItem));
    imageOlderList.replaceChildren(...older.map(imageItem));
    document.getElementById("image-older-summary").textContent = "Older versions (" + older.length + ")";
    imageOlder.hidden = older.length === 0;
  } catch (error) {
    imageStatus.textContent = "Keeper OS images are unavailable right now.";
    imageRetry.hidden = false;
  }
}

imageRetry.addEventListener("click", loadImageDownloads);

// Keeper Forge opens /forge/connect?code=K7QM-42XP: the code it shows on its own screen. The older
// /forge/connect?port=..&state=.. still works for a Forge that has not been updated.
const pairingCode = forgeAsk ? (forgeAsk.get("code") || "").toUpperCase() : "";
const validPairingCode = /^[A-Z0-9]{4}-?[A-Z0-9]{4}$/.test(pairingCode);

function showForge(email, organization) {
  document.getElementById("forge-who").textContent = email;
  document.getElementById("forge-org").textContent = organization || "None yet";
  const pairing = forgeAsk.has("code");
  document.getElementById("forge-code-box").hidden = !pairing;
  if (pairing) document.getElementById("forge-code").textContent = pairingCode;
  document.getElementById("forge-yes").textContent = pairing ? "Yes, connect Forge" : "Connect Keeper Forge";
  document.getElementById("forge-no").textContent = pairing ? "It is not my code" : "Cancel";
  if (pairing && !validPairingCode) {
    forgeMessage("bad", "This link has no valid code. Start the sign-in again in Keeper Forge.");
    document.getElementById("forge-yes").disabled = true;
  }
}

function forgeMessage(kind, text) {
  const result = document.getElementById("forge-result");
  result.hidden = false;
  result.className = "banner " + kind;
  result.textContent = text;
}

function forgeDone(text) {
  forgeMessage("ok", text);
  document.getElementById("forge-yes").hidden = true;
  document.getElementById("forge-no").hidden = true;
}

async function answerPairing(approve) {
  const response = await fetch("/api/v1/forge/pairings/answer", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code: pairingCode, approve }),
  });
  if (response.ok) return true;
  document.getElementById("forge-yes").disabled = false;
  forgeMessage("bad", response.status === 429 ? "Too many tries. Wait a few minutes, then start again in Keeper Forge."
    : "That code is not waiting any more. Start the sign-in again in Keeper Forge.");
  return false;
}

document.getElementById("forge-no").addEventListener("click", async () => {
  if (!forgeAsk.has("code")) { location.assign("/"); return; }
  document.getElementById("forge-yes").disabled = true;
  try { if (await answerPairing(false)) forgeDone("Not connected. If you did not start this, you can close the tab."); }
  catch (error) {
    // An invalid code stays unanswerable; a valid one can still be approved after a network failure.
    document.getElementById("forge-yes").disabled = !validPairingCode;
    forgeMessage("bad", "Could not reach the Hub. Check the connection and try again.");
  }
});

document.getElementById("forge-yes").addEventListener("click", async () => {
  const button = document.getElementById("forge-yes");
  const fail = (text) => { forgeMessage("bad", text); button.disabled = false; };
  button.disabled = true;
  if (forgeAsk.has("code")) {
    try { if (await answerPairing(true)) forgeDone("Keeper Forge is connected. You can close this tab; Keeper Forge carries on by itself within a few seconds."); }
    catch (error) { fail("Could not reach the Hub. Check the connection and try again."); }
    return;
  }
  let response;
  try {
    response = await fetch("/api/v1/forge/connect", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ port: Number(forgeAsk.get("port")), state: forgeAsk.get("state") || "" }),
    });
  } catch (error) { fail("Could not reach the Hub. Check the connection and try again."); return; }
  if (!response.ok) { fail("That request did not come from Keeper Forge. Start the sign-in again in Keeper Forge."); return; }
  const { redirect } = await response.json();
  forgeDone("Connected. You can close this tab and go back to Keeper Forge.");
  location.assign(redirect);
});

// One view at a time. Pages without the sidebar (log in, Keeper Forge) have the bar on top instead.
let view = null;
function show(next) {
  view = next;
  const inApp = next !== "login" && next !== "forge";
  document.getElementById("login").hidden = next !== "login";
  document.getElementById("forge").hidden = next !== "forge";
  document.getElementById("view-sticks").hidden = next !== "sticks";
  document.getElementById("view-people").hidden = next !== "people";
  document.getElementById("view-downloads").hidden = next !== "downloads";
  document.getElementById("side").hidden = !inApp;
  document.getElementById("topline").hidden = inApp;
  document.getElementById("main").classList.toggle("stage", !inApp);
  for (const [name, id] of [["sticks", "nav-sticks"], ["people", "nav-people"], ["downloads", "nav-downloads"]]) {
    const link = document.getElementById(id);
    if (next === name) link.setAttribute("aria-current", "page"); else link.removeAttribute("aria-current");
  }
  document.getElementById("tb-title").textContent = "Keeper Hub - " + TITLES[next];
  document.title = TITLES[next] + " · Keeper Hub";
}

const RANK = { owner: 3, admin: 2, viewer: 1 };
const rolesBelow = (role) => Object.keys(RANK).filter((r) => RANK[r] < RANK[role]);
const ROLE_TEXT = { owner: "Full control, including people.", admin: "Manages sticks and invites people.", viewer: "Can look at sticks. Cannot change anything." };
const capital = (word) => word.charAt(0).toUpperCase() + word.slice(1);
let inviteToken = null;
let myRole = null;
let myEmail = null;
let noticeTimer = null;

async function call(method, path, body) {
  const response = await fetch(path, {
    method,
    cache: "no-store",
    headers: body ? { "Content-Type": "application/json" } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  let data = null;
  try { data = await response.json(); } catch (error) { /* no body, or not JSON */ }
  return { ok: response.ok, status: response.status, data };
}

function notice(text, good) {
  const box = document.getElementById("notice");
  box.textContent = text;
  box.className = "banner " + (good ? "ok" : "bad");
  box.style.display = "flex";
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => { box.style.display = "none"; }, 20000);
}

function say2(id, text, good) {
  const box = document.getElementById(id);
  box.textContent = text;
  box.className = "form-msg" + (good ? "" : " bad");
}

function actionButton(label, what, onClick, key) {
  const button = el("button", "btn sm" + (/^(Remove|Leave|Cancel)/.test(label) ? " danger" : ""), label);
  button.type = "button";
  button.dataset.key = "people:" + key;
  button.setAttribute("aria-label", label.replace("…", "") + " " + what);
  button.addEventListener("click", onClick);
  return button;
}

async function loadPeople() {
  const admin = myRole === "owner" || myRole === "admin";
  document.getElementById("people").hidden = !admin;
  document.getElementById("people-denied").hidden = admin;
  if (!admin) return;
  const roles = rolesBelow(myRole).reverse();
  const base = "/api/v1/orgs/" + encodeURIComponent(org);
  const [members, invitations] = await Promise.all([call("GET", base + "/members"), call("GET", base + "/invitations")]);
  if (!members.ok || !invitations.ok) {
    say2("people-msg", "The people list could not be loaded. It will try again.", false);
    return;
  }
  keepFocus(() => {
    const choices = document.getElementById("invite-roles");
    if (choices.dataset.roles !== roles.join()) {
      choices.dataset.roles = roles.join();
      choices.replaceChildren(choices.firstElementChild, ...roles.map((r, at) => {
        const card = el("label", "role");
        const radio = el("input");
        radio.type = "radio";
        radio.name = "invite-role";
        radio.value = r;
        radio.checked = at === 0;
        card.appendChild(radio);
        card.appendChild(el("b", "", capital(r)));
        card.appendChild(el("span", "", ROLE_TEXT[r]));
        return card;
      }));
    }

    document.getElementById("mem-count").textContent = members.data.length;
    const memberRows = document.getElementById("members");
    memberRows.replaceChildren();
    for (const member of members.data) {
      const row = el("li");
      const text = el("div");
      const name = el("b", "", member.email);
      text.appendChild(name);
      const own = member.email === myEmail;
      if (own) { text.appendChild(document.createTextNode(" ")); text.appendChild(el("span", "pill new", "You")); }
      text.appendChild(el("div", "hint", capital(member.role) + ": " + ROLE_TEXT[member.role]));
      row.appendChild(text);
      row.appendChild(el("span", "muted", capital(member.role)));
      if (own || RANK[myRole] > RANK[member.role]) {
        row.appendChild(actionButton(own ? "Leave…" : "Remove…", member.email, async () => {
          if (!confirm((own ? "Leave this organization?" : "Remove " + member.email + "?"))) return;
          const result = await call("DELETE", base + "/members/" + encodeURIComponent(member.user_id));
          if (!result.ok) say2("people-msg", (result.data && result.data.error) || "That did not work.", false);
          await refresh();
        }, "m-" + member.user_id));
      }
      memberRows.appendChild(row);
    }

    const inviteRows = document.getElementById("invitations");
    inviteRows.replaceChildren();
    for (const invitation of invitations.data) {
      const row = el("li");
      const text = el("div");
      text.appendChild(el("b", "", invitation.email));
      text.appendChild(el("div", "hint", capital(invitation.role) + " · the link works until " + new Date(invitation.expires_at).toLocaleString()));
      row.appendChild(text);
      if (RANK[myRole] > RANK[invitation.role]) {
        row.appendChild(actionButton("Send again", invitation.email, async () => {
          const result = await call("POST", base + "/invitations", { email: invitation.email, role: invitation.role });
          say2("people-msg", result.ok ? "If that address can join, a new link is on its way to " + invitation.email + "." : (result.data && result.data.error) || "That did not work.", result.ok);
        }, "s-" + invitation.id));
        row.appendChild(actionButton("Cancel invitation", invitation.email, async () => {
          const result = await call("DELETE", base + "/invitations/" + encodeURIComponent(invitation.id));
          if (!result.ok) say2("people-msg", (result.data && result.data.error) || "That did not work.", false);
          await loadPeople();
        }, "c-" + invitation.id));
      }
      inviteRows.appendChild(row);
    }
    document.getElementById("no-invitations").hidden = invitations.data.length > 0;
  });
}

async function loadStaff(isStaff) {
  const panel = document.getElementById("staff");
  panel.hidden = !isStaff;
  if (!isStaff) return;
  const result = await call("GET", "/api/v1/staff/organizations");
  if (!result.ok) return;
  const rows = document.getElementById("all-orgs");
  rows.replaceChildren();
  for (const entry of result.data) {
    const row = el("li");
    row.appendChild(document.createTextNode(entry.name));
    row.appendChild(el("span", "grow"));
    row.appendChild(el("span", "muted mono", entry.slug));
    rows.appendChild(row);
  }
  document.getElementById("orgs-summary").textContent = result.data.length + (result.data.length === 1 ? " existing organization" : " existing organizations");
}

async function refresh() {
  const problem = document.getElementById("problem");
  try {
    let me = await fetch("/api/v1/me", { cache: "no-store" });
    if (me.status === 401) {
      show("login");
      problem.style.display = "none";
      return;
    }
    if (me.ok && inviteToken) {
      const token = inviteToken;
      inviteToken = null;
      const result = await call("POST", "/api/v1/invitations/accept", { token });
      if (result.ok) {
        org = result.data.organization.slug;
        notice("You joined " + result.data.organization.name + " as " + result.data.role + ".", true);
      } else {
        notice((result.data && result.data.error) || "That invitation did not work.", false);
      }
      me = await fetch("/api/v1/me", { cache: "no-store" });
    }
    if (!me.ok) throw new Error("HTTP " + me.status);
    const account = await me.json();
    const orgs = account.organizations;
    const select = document.getElementById("org");
    if (!orgs.some((o) => o.slug === org)) org = orgs.length ? orgs[0].slug : null;
    if (select.options.length !== orgs.length) {
      select.replaceChildren(...orgs.map((o) => new Option(o.name, o.slug)));
    }
    select.value = org || "";
    select.hidden = orgs.length < 2;
    const current = orgs.find((o) => o.slug === org);
    myRole = current ? current.role : null;
    myEmail = account.email;
    document.getElementById("org-name").textContent = current ? current.name : "No organization yet";
    document.getElementById("org-name").hidden = orgs.length > 1;
    document.getElementById("who-name").textContent = account.email + (current ? " · " + capital(current.role) : "");
    document.getElementById("nav-people").hidden = !(myRole === "owner" || myRole === "admin" || account.staff === true);
    const openImageDownloads = page === "downloads" && view !== "downloads";
    show(page);
    if (forgeAsk) { showForge(account.email, current ? current.name : ""); return; }
    if (page === "people") await loadStaff(account.staff === true);
    if (openImageDownloads) await loadImageDownloads();
    if (!org) {
      sticks = [];
      render();
      if (page === "people") await loadPeople();
      problem.textContent = "You are not a member of any organization yet. Ask an owner to add you.";
      problem.style.display = "flex";
      return;
    }
    const response = await fetch("/api/v1/orgs/" + encodeURIComponent(org) + "/sticks", { cache: "no-store" });
    if (!response.ok) throw new Error("HTTP " + response.status);
    sticks = await response.json();
    render();
    if (page === "people") await loadPeople();
    document.getElementById("live-text").textContent = "Live · updated " + new Date().toLocaleTimeString();
    problem.style.display = "none";
  } catch (error) {
    problem.textContent = page === "sticks" ? "The Hub cannot be reached. The list below is the last one received." : "The Hub cannot be reached. Check the connection; this page tries again by itself.";
    problem.style.display = "flex";
  }
}

document.getElementById("org").addEventListener("change", (event) => {
  org = event.target.value;
  refresh();
});

const MODES = {
  login: { title: "Welcome back", intro: page === "downloads" ? "Log in with your organizer account to download Keeper OS." : "Log in to see which contest computers are online.", email: true, pass: "Password", autocomplete: "current-password", button: "Log in", path: "/api/v1/login" },
  register: { title: "Create an account", intro: "We email you a link to confirm your address. After that, an owner of your organization adds you to it.", email: true, pass: "Password", help: "At least 12 characters.", autocomplete: "new-password", button: "Create account", path: "/api/v1/register", done: "Check your email for the link." },
  forgot: { title: "Reset your password", intro: "Enter your email. We email you a link that works for one hour.", email: true, pass: null, button: "Send reset link", path: "/api/v1/password-reset", done: "Check your email. If that address has an account, a link is on its way." },
  reset: { title: "Choose a new password", intro: "Pick a password you do not use anywhere else.", email: false, pass: "New password", help: "At least 12 characters.", autocomplete: "new-password", button: "Change password", path: "/api/v1/password-reset/confirm", done: "Password changed. Log in with it." },
};
let mode = "login";
let resetToken = null;

function say(text, good) {
  const message = document.getElementById("login-error");
  message.textContent = text;
  message.hidden = !text;
  message.className = "banner " + (good ? "ok" : "bad");
}

function setMode(next) {
  mode = next;
  const m = MODES[next];
  document.getElementById("login-title").textContent = m.title;
  document.getElementById("login-intro").textContent = m.intro;
  document.getElementById("email-row").hidden = !m.email;
  document.getElementById("user").required = m.email;
  document.getElementById("pass-row").hidden = m.pass === null;
  document.getElementById("pass").required = m.pass !== null;
  if (m.pass) {
    document.getElementById("pass-label").textContent = m.pass;
    document.getElementById("pass").autocomplete = m.autocomplete;
    const help = document.getElementById("pass-help");
    help.hidden = !m.help;
    help.textContent = m.help || "";
  }
  setPasswordShown(false);
  document.getElementById("login-submit").textContent = m.button;
  document.getElementById("to-forgot").hidden = next !== "login";
  document.getElementById("new-here").hidden = next !== "login";
  document.getElementById("to-login").hidden = next === "login";
  say("", false);
}

function setPasswordShown(shown) {
  document.getElementById("pass").type = shown ? "text" : "password";
  const toggle = document.getElementById("pw-toggle");
  toggle.textContent = shown ? "Hide" : "Show";
  toggle.setAttribute("aria-pressed", String(shown));
}

document.getElementById("pw-toggle").addEventListener("click", () => setPasswordShown(document.getElementById("pass").type === "password"));

for (const [id, target] of [["to-register", "register"], ["to-forgot", "forgot"], ["to-login", "login"]]) {
  document.getElementById(id).addEventListener("click", (event) => { event.preventDefault(); setMode(target); });
}

document.getElementById("login").addEventListener("submit", async (event) => {
  event.preventDefault();
  const m = MODES[mode];
  say("", false);
  const email = document.getElementById("user").value.trim();
  const password = document.getElementById("pass").value;
  // Said next to the form, in words that say what to do, before anything is sent.
  if (m.email && !/^[^@\s]+@[^@\s]+$/.test(email)) { say("Type your email address, like name@school.example.", false); document.getElementById("user").focus(); return; }
  if (m.pass !== null && !password) { say("Type your password.", false); document.getElementById("pass").focus(); return; }
  const body = mode === "forgot" ? { email }
    : mode === "reset" ? { token: resetToken, password }
    : { email, password };
  const submit = document.getElementById("login-submit");
  submit.disabled = true;
  let response;
  try {
    response = await fetch(m.path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  } catch (error) {
    submit.disabled = false;
    say("Could not reach the Hub. Check the connection and try again.", false);
    return;
  }
  submit.disabled = false;
  document.getElementById("pass").value = "";
  if (mode === "login" && response.ok) { await refresh(); return; }
  if (response.ok) {
    const done = m.done;
    if (mode === "reset") { resetToken = null; setMode("login"); }
    say(done, true);
    return;
  }
  try { say((await response.json()).error, false); }
  catch (error) { say("That did not work. Try again in a moment.", false); }
});

document.getElementById("invite-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const email = document.getElementById("invite-email").value;
  const chosen = document.querySelector("input[name='invite-role']:checked");
  say2("invite-msg", "", true);
  if (!chosen) { say2("invite-msg", "Choose what they can do.", false); return; }
  try {
    const result = await call("POST", "/api/v1/orgs/" + encodeURIComponent(org) + "/invitations", { email, role: chosen.value });
    if (result.ok) {
      document.getElementById("invite-email").value = "";
      say2("invite-msg", "If that address can join, a link is on its way to " + email + ".", true);
      await loadPeople();
    } else {
      say2("invite-msg", (result.data && result.data.error) || "That did not work.", false);
    }
  } catch (error) { say2("invite-msg", "Could not reach the Hub.", false); }
});

document.getElementById("staff-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const body = {
    slug: document.getElementById("staff-slug").value,
    name: document.getElementById("staff-name").value,
    owner_email: document.getElementById("staff-owner").value,
  };
  say2("staff-msg", "", true);
  try {
    const result = await call("POST", "/api/v1/staff/organizations", body);
    if (result.ok) {
      for (const id of ["staff-slug", "staff-name", "staff-owner"]) document.getElementById(id).value = "";
      say2("staff-msg", "Made. The owner got a link by mail.", true);
      await loadStaff(true);
    } else {
      say2("staff-msg", (result.data && result.data.error) || "That did not work.", false);
    }
  } catch (error) { say2("staff-msg", "Could not reach the Hub.", false); }
});

document.getElementById("logout").addEventListener("click", async () => {
  try { await fetch("/api/v1/logout", { method: "POST" }); } catch (error) { /* refresh shows it */ }
  await refresh();
});

// A link from a mail carries its token after the #, so it never reaches a server log, and a mail
// scanner that fetches the page does not use the link up.
async function openedFromMail() {
  const [kind, token] = location.hash.slice(1).split("=");
  if (kind !== "verify" && kind !== "reset" && kind !== "invite") return;
  history.replaceState(null, "", location.pathname);
  if (kind === "invite") {
    // Used once the browser has a session: right away if there is one, otherwise after the login.
    inviteToken = token;
    setMode("login");
    say("Log in, or create an account, with the address the invitation was sent to.", true);
    return;
  }
  if (kind === "reset") {
    // The form only shows to someone who is logged out, so end this browser's session first.
    try { await fetch("/api/v1/logout", { method: "POST" }); } catch (error) { /* refresh shows it */ }
    resetToken = token;
    setMode("reset");
    return;
  }
  try {
    const response = await fetch("/api/v1/verify-email", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token }) });
    if (response.ok) say("Email confirmed. You can log in now.", true);
    else say("That link is not valid any more.", false);
  } catch (error) { say("Could not reach the Hub.", false); }
}

if (page === "downloads") setMode("login");
document.getElementById("search").addEventListener("input", (event) => {
  query = event.target.value.trim().toLowerCase();
  render();
});

for (const chip of document.querySelectorAll(".chip")) {
  chip.addEventListener("click", () => {
    filter = chip.dataset.filter;
    for (const other of document.querySelectorAll(".chip")) {
      other.setAttribute("aria-pressed", String(other === chip));
    }
    render();
  });
}


openedFromMail().then(refresh);
window.addEventListener("hashchange", () => openedFromMail());
setInterval(refresh, REFRESH_MS);
