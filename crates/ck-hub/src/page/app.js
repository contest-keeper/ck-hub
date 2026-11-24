const REFRESH_MS = 5000;
let sticks = [];
let filter = "all";
let query = "";
let org = null;
const opened = new Set();

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
const needsLook = (stick) => !isArchived(stick) && stick.warnings.length > 0;
const neverSeen = (stick) => !stick.last;
const isOffline = (stick) => !stick.online && !!stick.last;

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

// Commands to a stick for admins and owners, and what became of each
// command sent. The text typed and the step the person is at are kept here, because the list is
// drawn again at every refresh.
const commandLists = new Map();
const commandDrafts = new Map();
const ACTION_NAMES = { message: "Message", lock_screen: "Lock", unlock_screen: "Unlock", shutdown: "Shutdown" };

function commandPath(stick) {
  return "/api/v1/orgs/" + encodeURIComponent(org) + "/sticks/" + encodeURIComponent(stick.stick_id) + "/commands";
}

function draftOf(stick) {
  let draft = commandDrafts.get(stick.stick_id);
  if (!draft) {
    draft = { action: null, text: "", busy: false, note: "", bad: false };
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

function commandPanel(stick) {
  const box = el("div", "net cmd");
  box.appendChild(el("h3", "", "Commands"));
  const name = stick.team + " " + (stick.machine_number ?? "");
  const draft = draftOf(stick);
  if (myRole === "owner" || myRole === "admin") {
    const supportsUnlock = stick.last?.screen_unlock === true;
    if (!supportsUnlock && draft.action === "unlock_screen") draft.action = null;
    const bar = el("div", "cmd-bar");
    for (const action of Object.keys(ACTION_NAMES)) {
      if (!supportsUnlock && action === "unlock_screen") continue;
      const button = actionButton(ACTION_NAMES[action], "to " + name, () => {
        draft.action = action;
        draft.note = "";
        render();
      });
      button.disabled = draft.busy;
      button.setAttribute("aria-pressed", String(draft.action === action));
      bar.appendChild(button);
    }
    box.appendChild(bar);
    if (draft.action) {
      const ask = el("div", "cmd-confirm");
      if (draft.action === "message") {
        const input = el("textarea");
        input.maxLength = 500;
        input.rows = 2;
        input.value = draft.text;
        input.dataset.cmd = stick.stick_id;
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
      var yes = el("button", "btn pri", "Confirm");
      yes.type = "button";
      yes.disabled = draft.busy || (draft.action === "message" && !draft.text.trim());
      yes.addEventListener("click", () => sendCommand(stick, draft));
      const no = el("button", "btn", "Cancel");
      no.type = "button";
      no.addEventListener("click", () => {
        draft.action = null;
        render();
      });
      const buttons = el("div", "cmd-bar");
      buttons.appendChild(yes);
      buttons.appendChild(no);
      ask.appendChild(buttons);
      box.appendChild(ask);
    }
    if (draft.note) box.appendChild(el("p", draft.bad ? "bad" : "hint", draft.note));
  } else {
    box.appendChild(el("p", "muted", "Only an admin or an owner can send commands."));
  }

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

// Sticks whose "Delete now" was clicked once: the second click deletes.
const confirmingDelete = new Set();

async function changeStick(stick, method, suffix, body, done) {
  const path = "/api/v1/orgs/" + encodeURIComponent(org) + "/sticks/" + encodeURIComponent(stick.stick_id) + suffix;
  const result = await call(method, path, body);
  confirmingDelete.delete(stick.stick_id);
  if (result.ok) {
    notice(done, true);
    opened.delete(stick.stick_id);
    await refresh();
  } else {
    notice((result.data && result.data.error) || "The Hub refused (" + result.status + ").", false);
    render();
  }
}

function archivePanel(stick) {
  const box = el("div", "net cmd");
  box.appendChild(el("h3", "", "Archive"));
  const name = stick.team + " " + (stick.machine_number ?? "");
  const admin = myRole === "owner" || myRole === "admin";
  if (isArchived(stick)) {
    box.appendChild(el("p", "", "Archived. The Hub deletes this stick and everything kept about it on " +
      new Date(stick.delete_after).toLocaleDateString() + "."));
    if (!admin) return box;
    const bar = el("div", "cmd-bar");
    bar.appendChild(actionButton("Restore", name, () => changeStick(stick, "POST", "/restore", null, "Stick restored.")));
    const sure = confirmingDelete.has(stick.stick_id);
    bar.appendChild(actionButton(sure ? "Click again to delete everything" : "Delete now", name, () => {
      if (!confirmingDelete.has(stick.stick_id)) {
        confirmingDelete.add(stick.stick_id);
        render();
        return;
      }
      changeStick(stick, "DELETE", "", null, "Stick deleted.");
    }));
    box.appendChild(bar);
    return box;
  }
  if (!admin) {
    box.appendChild(el("p", "muted", "Only an admin can archive a stick."));
    return box;
  }
  box.appendChild(el("p", "muted", "An archived stick stops reporting and is deleted with all its data when the time is up. You can restore it before then."));
  const bar = el("div", "cmd-bar");
  for (const days of [7, 30]) {
    bar.appendChild(actionButton("Archive for " + days + " days", name, () =>
      changeStick(stick, "POST", "/archive", { days }, "Stick archived.")));
  }
  box.appendChild(bar);
  return box;
}

function render() {
  const body = document.getElementById("rows");
  // The list is drawn again at every refresh: keep the cursor in a message being typed.
  const typing = document.activeElement && document.activeElement.dataset ? document.activeElement.dataset.cmd : null;
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
  document.getElementById("n-all").textContent = counts.all;
  document.getElementById("n-online").textContent = counts.online;
  document.getElementById("n-offline").textContent = counts.offline;
  document.getElementById("n-attn").textContent = counts.attn;
  for (const name of ["all", "attn", "offline", "new", "archived"]) {
    document.getElementById("c-" + name).textContent = counts[name];
  }

  const shown = sticks.filter(matches).sort((a, b) =>
    rank(a) - rank(b) || (a.machine_number ?? 0) - (b.machine_number ?? 0));
  for (const stick of shown) {
    const last = stick.last;
    const row = el("tr", needsLook(stick) ? "flag" : "");
    cell(row, stick.machine_number ?? "", "num mono");
    const team = cell(row, "");
    team.appendChild(el("b", "", stick.team || "Stick " + stick.stick_id));
    team.appendChild(el("span", "sub", stick.site));
    if (stick.hardware && stick.hardware.rewritten) {
      team.appendChild(el("span", "sub", "Rewritten, written " + stick.hardware.write_count + " times"));
    }
    const status = cell(row, "");
    status.appendChild(el("span", "pill " + (isArchived(stick) ? "new" : stick.online ? "on" : last ? "off" : "new"),
      isArchived(stick) ? "archived" : stick.online ? "online" : (last ? "offline" : "not seen yet")));
    cell(row, stick.current_user ?? "", stick.current_user ? "" : "muted");
    cell(row, last ? ago(last.seconds_ago) : "—", last ? "" : "muted");
    const warnings = cell(row, "");
    if (stick.warnings.length === 0) warnings.className = "muted";
    if (stick.warnings.length === 0) warnings.textContent = last ? "No warnings" : "Waiting for the first heartbeat";
    for (const text of stick.warnings) warnings.appendChild(el("span", "warn", text));
    const open = opened.has(stick.stick_id);
    const toggle = el("button", "btn", open ? "Hide" : "Details");
    toggle.type = "button";
    toggle.setAttribute("aria-expanded", open ? "true" : "false");
    toggle.setAttribute("aria-label", (open ? "Hide" : "Show") + " details for " + stick.team + " " + (stick.machine_number ?? ""));
    toggle.addEventListener("click", () => {
      if (opened.has(stick.stick_id)) opened.delete(stick.stick_id); else opened.add(stick.stick_id);
      render();
    });
    cell(row, "").appendChild(toggle);
    body.appendChild(row);
    if (open) {
      const detail = el("tr", "detail");
      const td = el("td");
      td.colSpan = 7;
      td.appendChild(facts(stick));
      td.appendChild(archivePanel(stick));
      td.appendChild(commandPanel(stick));
      td.appendChild(proofPanel(stick));
      td.appendChild(networkPanel(stick));
      td.appendChild(recordingPanel(stick));
      detail.appendChild(td);
      body.appendChild(detail);
    }
  }
  if (typing) {
    const again = Array.from(document.querySelectorAll("textarea[data-cmd]")).find((n) => n.dataset.cmd === typing);
    if (again) {
      again.focus();
      again.setSelectionRange(again.value.length, again.value.length);
    }
  }
  document.getElementById("empty").style.display = shown.length ? "none" : "block";
  document.getElementById("empty").textContent = sticks.length
    ? "No stick matches. Change the search or the filter."
    : "No sticks yet. Make one with ck-forge and it appears here.";
}

// Keeper Forge opens /forge/connect?port=..&state=.. . The port and the state are only passed on:
// the Hub checks them and builds the address the browser goes back to.
const forgeAsk = location.pathname === "/forge/connect" ? new URLSearchParams(location.search) : null;
const imagesPage = location.pathname === "/download/image";
const imageDownloads = document.getElementById("image-downloads");
const imageList = document.getElementById("image-download-list");
const imageStatus = document.getElementById("image-download-status");
const imageRetry = document.getElementById("image-download-retry");
const imageOlder = document.getElementById("image-older");
const imageOlderList = document.getElementById("image-older-list");
const imageOlderSummary = document.getElementById("image-older-summary");

function imageItem(image) {
  const item = el("li", "tile");
  item.appendChild(el("h2", "", image.name + " · " + image.version));
  const details = [image.family, image.profile, image.filename, bytesText(image.size)].filter(Boolean).join(" · ");
  item.appendChild(el("p", "muted", details));
  const checksum = el("p", "");
  checksum.append("SHA-256 ", el("code", "", image.sha256));
  item.appendChild(checksum);
  const actions = el("div", "download-actions");
  actions.appendChild(link("Download image", image.url));
  if (image.source_offer_url) actions.appendChild(link("Read source offer", image.source_offer_url));
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
    imageOlderSummary.textContent = "Older versions (" + older.length + ")";
    imageOlder.hidden = older.length === 0;
  } catch (error) {
    imageStatus.textContent = "Keeper OS images are unavailable right now.";
    imageRetry.hidden = false;
  }
}

imageRetry.addEventListener("click", loadImageDownloads);

function showForge(email) {
  document.getElementById("content").style.display = "none";
  document.getElementById("forge").hidden = false;
  document.getElementById("forge-who").textContent = email;
}

document.getElementById("forge-yes").addEventListener("click", async () => {
  const result = document.getElementById("forge-result");
  const response = await fetch("/api/v1/forge/connect", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ port: Number(forgeAsk.get("port")), state: forgeAsk.get("state") || "" }),
  });
  if (!response.ok) { result.textContent = "That request did not come from Keeper Forge. Start the sign-in again in the forge."; return; }
  const { redirect } = await response.json();
  result.textContent = "Connected. You can close this tab and go back to Keeper Forge.";
  document.getElementById("forge-yes").hidden = true;
  location.assign(redirect);
});

function showLogin(visible) {
  document.getElementById("login").style.display = visible ? "flex" : "none";
  document.getElementById("content").style.display = visible || imagesPage ? "none" : "block";
  imageDownloads.hidden = visible || !imagesPage;
  document.getElementById("who").style.display = visible ? "none" : "inline-flex";
  document.getElementById("live").hidden = visible || imagesPage;
}

const RANK = { owner: 3, admin: 2, viewer: 1 };
const rolesBelow = (role) => Object.keys(RANK).filter((r) => RANK[r] < RANK[role]);
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
  box.className = good ? "" : "bad";
  box.style.display = "block";
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => { box.style.display = "none"; }, 20000);
}

function say2(id, text, good) {
  const box = document.getElementById(id);
  box.textContent = text;
  box.className = "form-msg" + (good ? "" : " bad");
}

function actionButton(label, what, onClick) {
  const button = el("button", "btn", label);
  button.type = "button";
  button.setAttribute("aria-label", label + " " + what);
  button.addEventListener("click", onClick);
  return button;
}

async function loadPeople() {
  const panel = document.getElementById("people");
  panel.hidden = !(myRole === "owner" || myRole === "admin");
  if (panel.hidden) return;
  const roles = rolesBelow(myRole);
  const select = document.getElementById("invite-role");
  if (Array.from(select.options).map((o) => o.value).join() !== roles.join()) {
    select.replaceChildren(...roles.map((r) => new Option(r, r)));
  }
  const base = "/api/v1/orgs/" + encodeURIComponent(org);
  const [members, invitations] = await Promise.all([call("GET", base + "/members"), call("GET", base + "/invitations")]);
  if (!members.ok || !invitations.ok) return;

  const memberRows = document.getElementById("members");
  memberRows.replaceChildren();
  for (const member of members.data) {
    const row = el("tr");
    cell(row, member.email);
    cell(row, member.role);
    const own = member.email === myEmail;
    const actions = cell(row, "");
    if (own || RANK[myRole] > RANK[member.role]) {
      actions.appendChild(actionButton(own ? "Leave" : "Remove", member.email, async () => {
        if (!confirm((own ? "Leave this organization?" : "Remove " + member.email + "?"))) return;
        const result = await call("DELETE", base + "/members/" + encodeURIComponent(member.user_id));
        if (!result.ok) say2("invite-msg", (result.data && result.data.error) || "That did not work.", false);
        await refresh();
      }));
    }
    memberRows.appendChild(row);
  }

  const inviteRows = document.getElementById("invitations");
  inviteRows.replaceChildren();
  for (const invitation of invitations.data) {
    const row = el("tr");
    cell(row, invitation.email);
    cell(row, invitation.role);
    cell(row, new Date(invitation.expires_at).toLocaleString());
    const actions = cell(row, "");
    if (RANK[myRole] > RANK[invitation.role]) {
      actions.appendChild(actionButton("Revoke", invitation.email, async () => {
        const result = await call("DELETE", base + "/invitations/" + encodeURIComponent(invitation.id));
        if (!result.ok) say2("invite-msg", (result.data && result.data.error) || "That did not work.", false);
        await loadPeople();
      }));
    }
    inviteRows.appendChild(row);
  }
  document.getElementById("no-invitations").style.display = invitations.data.length ? "none" : "block";
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
    const row = el("tr");
    cell(row, entry.name);
    cell(row, entry.slug, "mono");
    rows.appendChild(row);
  }
}

async function refresh() {
  const problem = document.getElementById("problem");
  try {
    let me = await fetch("/api/v1/me", { cache: "no-store" });
    if (me.status === 401) {
      if (imagesPage && !imageDownloads.hidden) { location.reload(); return; }
      showLogin(true);
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
    document.getElementById("who-name").textContent = account.email + (current ? " (" + current.role + ")" : "");
    const openImageDownloads = imagesPage && imageDownloads.hidden;
    showLogin(false);
    if (forgeAsk) { showForge(account.email); return; }
    if (imagesPage) {
      problem.style.display = "none";
      if (openImageDownloads) await loadImageDownloads();
      return;
    }
    await loadStaff(account.staff === true);
    if (!org) {
      sticks = [];
      render();
      await loadPeople();
      problem.textContent = "You are not a member of any organization yet. Ask an owner to add you.";
      problem.style.display = "block";
      return;
    }
    const response = await fetch("/api/v1/orgs/" + encodeURIComponent(org) + "/sticks", { cache: "no-store" });
    if (!response.ok) throw new Error("HTTP " + response.status);
    sticks = await response.json();
    render();
    await loadPeople();
    document.getElementById("live-text").textContent = "Live · updated " + new Date().toLocaleTimeString();
    problem.style.display = "none";
  } catch (error) {
    problem.textContent = imagesPage ? "The Hub cannot be reached." : "The Hub cannot be reached. The list below is the last one received.";
    problem.style.display = "block";
  }
}

document.getElementById("org").addEventListener("change", (event) => {
  org = event.target.value;
  refresh();
});

function setTheme(theme) {
  document.documentElement.dataset.theme = theme;
  document.getElementById("theme-classic").setAttribute("aria-pressed", String(theme === "classic"));
  document.getElementById("theme-modern").setAttribute("aria-pressed", String(theme === "modern"));
  try { localStorage.setItem("ck-theme", theme); } catch (error) { /* the choice lasts until the page closes */ }
}

document.getElementById("theme-classic").addEventListener("click", () => setTheme("classic"));
document.getElementById("theme-modern").addEventListener("click", () => setTheme("modern"));
setTheme(document.documentElement.dataset.theme);

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

const MODES = {
  login: { title: "Log in to Keeper Hub", intro: imagesPage ? "Log in with your organizer account to download Keeper OS." : "See which contest machines are online and which need attention.", email: true, pass: "Password", autocomplete: "current-password", button: "Log in", path: "/api/v1/login" },
  register: { title: "Create an account", intro: "We mail you a link to confirm your address. After that, an owner of your organization adds you to it.", email: true, pass: "Choose a password (12 characters or more)", autocomplete: "new-password", button: "Create account", path: "/api/v1/register", done: "Check your email for the link." },
  forgot: { title: "Reset your password", intro: "Enter your email and we mail you a link.", email: true, pass: null, button: "Send link", path: "/api/v1/password-reset", done: "If that address has an account, a link is on its way." },
  reset: { title: "Choose a new password", intro: "At least 12 characters.", email: false, pass: "New password", autocomplete: "new-password", button: "Change password", path: "/api/v1/password-reset/confirm", done: "Password changed. Log in with it." },
};
let mode = "login";
let resetToken = null;

function say(text, good) {
  const message = document.getElementById("login-error");
  message.textContent = text;
  message.className = good ? "ok" : "";
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
  }
  document.getElementById("login-submit").textContent = m.button;
  document.getElementById("to-register").hidden = next !== "login";
  document.getElementById("to-forgot").hidden = next !== "login";
  document.getElementById("to-login").hidden = next === "login";
  say("", false);
}

for (const [id, target] of [["to-register", "register"], ["to-forgot", "forgot"], ["to-login", "login"]]) {
  document.getElementById(id).addEventListener("click", (event) => { event.preventDefault(); setMode(target); });
}

document.getElementById("login").addEventListener("submit", async (event) => {
  event.preventDefault();
  const m = MODES[mode];
  say("", false);
  const email = document.getElementById("user").value;
  const password = document.getElementById("pass").value;
  const body = mode === "forgot" ? { email }
    : mode === "reset" ? { token: resetToken, password }
    : { email, password };
  let response;
  try {
    response = await fetch(m.path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  } catch (error) {
    say("Could not reach the Hub.", false);
    return;
  }
  document.getElementById("pass").value = "";
  if (mode === "login" && response.ok) { await refresh(); return; }
  if (response.ok) {
    const done = m.done;
    if (mode === "reset") { resetToken = null; setMode("login"); }
    say(done, true);
    return;
  }
  try { say((await response.json()).error, false); }
  catch (error) { say("That did not work.", false); }
});

document.getElementById("invite-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const email = document.getElementById("invite-email").value;
  const role = document.getElementById("invite-role").value;
  say2("invite-msg", "", true);
  try {
    const result = await call("POST", "/api/v1/orgs/" + encodeURIComponent(org) + "/invitations", { email, role });
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

if (imagesPage) setMode("login");
openedFromMail().then(refresh);
window.addEventListener("hashchange", () => openedFromMail());
setInterval(refresh, REFRESH_MS);
