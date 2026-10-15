// qrtx scanner page: scan two codes, connect to both devices, introduce them.
import init, { Node, parseTicket, aDials, decodeQr, setLogger } from "./pkg/qrtx_web.js";

const $ = (sel) => document.querySelector(sel);
const els = {
  slots: [...document.querySelectorAll(".slot")],
  message: $("#message"),
  scan: $("#scan"),
  restart: $("#restart"),
  stopScan: $("#stop-scan"),
  viewfinder: $("#viewfinder"),
  video: $("#video"),
  frame: $("#frame"),
  pasteForm: $("#paste-form"),
  paste: $("#paste"),
  pasteBox: $("#paste-box"),
  direct: $("#direct"),
  directRecv: $("#direct-recv"),
  directSend: $("#direct-send"),
  directFiles: $("#direct-files"),
  directProgress: $("#direct-progress"),
  received: $("#received"),
  share: $("#share"),
  diag: $("#diag"),
  diagLog: $("#diag-log"),
  diagCopy: $("#diag-copy"),
};

// ---------- diagnostics ----------
// Shown in the "Diagnostics" panel. Never put the ticket (or the page URL,
// whose fragment holds it) in here: people paste this into public issues.

const t0 = performance.now();
const logLines = [];
let logDrawQueued = false;
function dlog(msg) {
  logLines.push(`${((performance.now() - t0) / 1000).toFixed(2).padStart(7)}  ${msg}`);
  if (logLines.length > 3000) logLines.splice(0, logLines.length - 3000);
  if (!logDrawQueued) {
    logDrawQueued = true;
    setTimeout(() => {
      logDrawQueued = false;
      els.diagLog.textContent = logLines.join("\n");
    }, 100);
  }
}
window.addEventListener("error", (e) => dlog(`page error: ${e.message} (${e.filename}:${e.lineno})`));
window.addEventListener("unhandledrejection", (e) => dlog(`unhandled rejection: ${errText(e.reason)}`));
els.diagCopy.addEventListener("click", async () => {
  const text = logLines.join("\n");
  try {
    await navigator.clipboard.writeText(text);
    els.diagCopy.textContent = "Copied";
  } catch {
    // older browsers: select it so the user can copy by hand
    getSelection().selectAllChildren(els.diagLog);
    els.diagCopy.textContent = "Selected; copy it";
  }
  setTimeout(() => (els.diagCopy.textContent = "Copy log"), 1500);
});

const SLOT_HINTS = [
  ["First computer", "scan the QR code <code>qrtx</code> prints"],
  ["Second computer", "scan the other one"],
];

// phase: "collect" (scanning/connecting), "pairing" (introducing two computers),
// "direct" (the phone itself sends/receives), "done", "failed"
const state = { phase: "collect", node: null, slots: [emptySlot(), emptySlot()], received: [] };

function emptySlot() {
  return { ticket: null, info: null, device: null, dev: null, status: "empty", label: "", error: "" };
}

let nodePromise;

// ---------- rendering ----------

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

const ROLE_TEXT = { pipe: "stdin/stdout", "listen-tcp": "listen-tcp", "connect-tcp": "connect-tcp" };

function renderSlot(i) {
  const s = state.slots[i];
  const el = els.slots[i];
  const title = el.querySelector(".title");
  const sub = el.querySelector(".sub");
  const pill = el.querySelector(".pill");
  const stateAttr = { empty: "", busy: "busy", ok: "ok", done: "done", error: "error" }[s.status];
  el.dataset.state = stateAttr;
  pill.textContent = s.label;
  el.querySelector(".num").textContent = s.status === "done" || s.status === "ok" ? "✓" : s.status === "error" ? "!" : String(i + 1);
  if (s.status === "empty") {
    title.textContent = SLOT_HINTS[i][0];
    sub.innerHTML = SLOT_HINTS[i][1];
    return;
  }
  const id = s.dev?.short ?? s.info?.short ?? "";
  title.textContent = s.dev?.name ?? "Computer " + (i + 1);
  const role = s.dev?.detail || ROLE_TEXT[s.info.role] || s.info.role;
  sub.innerHTML = s.status === "error"
    ? `${esc(s.error)}`
    : `${esc(role)} <span class="id">[${esc(id)}]</span>`;
}

function render() {
  state.slots.forEach((_, i) => renderSlot(i));
  const busy = state.phase === "pairing";
  els.scan.hidden = !!scanner.stream || state.phase !== "collect" || bothFilled();
  els.scan.disabled = busy;
  els.restart.hidden = state.phase === "collect" && !state.slots.some((s) => s.status === "error");
  els.restart.textContent = state.phase === "done" ? "Connect two more" : "Start over";
  els.pasteBox.hidden = state.phase !== "collect";
  // the phone as an endpoint: offered once one computer that sends/receives is verified
  const solo = soloDevice();
  const role = solo?.dev.role;
  const offer = state.phase === "collect" && (role === "send" || role === "recv");
  els.directRecv.hidden = !(offer && role === "send");
  els.directSend.hidden = !(offer && role === "recv");
  els.direct.hidden = !offer && state.phase !== "direct" && state.received.length === 0;
}

/** The verified computer when it's the only one scanned so far. */
function soloDevice() {
  const ok = state.slots.filter((s) => s.status === "ok");
  const empty = state.slots.filter((s) => s.status === "empty" || s.status === "error");
  return ok.length === 1 && empty.length === 1 ? ok[0] : null;
}

function say(text, kind = "") {
  els.message.className = kind;
  els.message.innerHTML = text;
}

function bothFilled() {
  return state.slots.every((s) => s.status !== "empty" && s.status !== "error");
}

// ---------- adding codes ----------

/** Returns true if the code was taken. */
function addTicket(text) {
  if (state.phase !== "collect") return false;
  let info;
  try {
    info = parseTicket(text);
  } catch {
    flashMessage("That QR code is not from qrtx.", "warn");
    return false;
  }
  const existing = state.slots.findIndex((s) => s.info?.id === info.id);
  if (existing >= 0 && state.slots[existing].status !== "error") {
    if (!bothFilled()) flashMessage("Already scanned that one. Now scan the other computer.", "warn");
    return false;
  }
  let i = existing >= 0 ? existing : state.slots.findIndex((s) => s.status === "empty");
  if (i < 0) i = state.slots.findIndex((s) => s.status === "error");
  if (i < 0) return false;
  state.slots[i] = { ...emptySlot(), ticket: text, info, status: "busy", label: "connecting" };
  render();
  say(bothFilled() ? "Connecting…" : "Connecting. Scan the other computer meanwhile.");
  if (bothFilled()) stopScanner();
  connectSlot(i, text);
  return true;
}

async function connectSlot(i, ticket) {
  const { short, role, relay } = state.slots[i].info;
  const started = performance.now();
  dlog(`slot ${i + 1}: connecting to ${short} (${role}, relay ${relay ?? "none"})`);
  try {
    const node = await nodePromise;
    const device = await node.connect(ticket);
    dlog(`slot ${i + 1}: ${short} ok after ${((performance.now() - started) / 1000).toFixed(1)}s`);
    if (state.slots[i].ticket !== ticket) {
      device.close(); // replaced meanwhile
      return;
    }
    Object.assign(state.slots[i], { device, dev: device.info(), status: "ok", label: "verified" });
  } catch (e) {
    if (state.slots[i].ticket !== ticket) return;
    Object.assign(state.slots[i], { status: "error", label: "failed", error: errText(e) });
    dlog(`slot ${i + 1}: FAILED after ${((performance.now() - started) / 1000).toFixed(1)}s: ${errText(e)}`);
    say("Couldn't connect. Check that qrtx is still running there, then scan again. Details are under Diagnostics.", "err");
    els.diag.open = true;
  }
  render();
  maybePair();
}

function errText(e) {
  return (e && (e.message || e.toString())) || "unknown error";
}

// ---------- pairing ----------

async function maybePair() {
  const [a, b] = state.slots;
  if (state.phase !== "collect") return;
  if (a.status !== "ok" || b.status !== "ok") {
    const one = state.slots.find((s) => s.status === "ok");
    const other = state.slots.find((s) => s.status === "empty");
    if (one && other) {
      const alt = { send: ", or receive the files on this phone", recv: ", or send files from this phone" }[one.dev.role] ?? "";
      say(`Connected to <b>${esc(one.dev.name)}</b>. Now scan the other computer${alt}.`);
    }
    return;
  }
  let aDialsB;
  try {
    aDialsB = aDials(a.dev.role, b.dev.role);
  } catch (e) {
    say(`These two can't be paired: ${esc(errText(e))}.`, "err");
    state.phase = "failed";
    render();
    return;
  }
  state.phase = "pairing";
  stopScanner();
  const [dialer, acceptor] = aDialsB ? [a, b] : [b, a];
  try {
    say("Introducing the computers…");
    setSlot(acceptor, "busy", "pairing");
    setSlot(dialer, "busy", "pairing");
    // the acceptor must know who to let in before the dialer shows up
    await acceptor.device.pair(dialer.device, false);
    setSlot(acceptor, "busy", "waiting");
    await dialer.device.pair(acceptor.device, true);
    setSlot(dialer, "busy", "dialing");
  } catch (e) {
    fail(e);
    return;
  }
  const results = await Promise.allSettled(
    state.slots.map(async (s) => {
      const r = await s.device.waitConnected();
      setSlot(s, "done", r.direct ? "linked · direct" : "linked");
      return r;
    }),
  );
  const failed = results.find((r) => r.status === "rejected");
  state.slots.forEach((s) => s.device.close());
  if (failed) {
    results.forEach((r, i) => {
      if (r.status === "rejected") setSlot(state.slots[i], "error", "failed", errText(r.reason));
    });
    fail(failed.reason);
    return;
  }
  state.phase = "done";
  say(`Connected <b>${esc(a.dev.name)}</b> ⇄ <b>${esc(b.dev.name)}</b>. You can close this page. Your data doesn't go through it.`, "ok");
  navigator.vibrate?.([40, 60, 40]);
  render();
}

// ---------- the phone as sender/receiver ----------

const MIME = {
  jpg: "image/jpeg", jpeg: "image/jpeg", png: "image/png", gif: "image/gif", webp: "image/webp", heic: "image/heic",
  mp4: "video/mp4", mov: "video/quicktime", mp3: "audio/mpeg", m4a: "audio/mp4", pdf: "application/pdf",
  txt: "text/plain", md: "text/markdown", json: "application/json", zip: "application/zip",
};
const mimeOf = (name) => MIME[name.split(".").pop().toLowerCase()] ?? "application/octet-stream";
const human = (n) => {
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  for (; n >= 1024 && i < u.length - 1; i++) n /= 1024;
  return i ? `${n.toFixed(1)} ${u[i]}` : `${n} B`;
};

function showProgress(done, total, label) {
  els.directProgress.hidden = false;
  els.directProgress.querySelector("span").style.width = total ? `${(100 * done) / total}%` : "0";
  els.directProgress.querySelector(".label").textContent = `${label}  ${human(done)} / ${human(total)}`;
}

/** Start the direct mode: the computer is told to expect the phone itself. */
async function startDirect(s, myRole) {
  state.phase = "direct";
  stopScanner();
  setSlot(s, "busy", myRole === "recv" ? "sending to phone" : "receiving from phone");
  const node = await nodePromise;
  // the other slot is this phone now
  const me = state.slots.find((x) => x !== s);
  Object.assign(me, {
    ...emptySlot(),
    info: { short: node.id(), role: myRole },
    dev: { name: "this phone", detail: myRole === "recv" ? "receives the files" : "sends files", short: node.id() },
    status: "busy",
    label: myRole === "recv" ? "receiving" : "sending",
  });
  render();
  dlog(`direct: phone ${myRole === "recv" ? "receives from" : "sends to"} ${s.dev.short}`);
  await s.device.pairWithPhone(node, myRole);
  // the computer reports when our tunnel shows up; read that in the background
  const connected = s.device.waitConnected().catch((e) => dlog(`direct: ${errText(e)}`));
  return { node, connected };
}

async function directReceive() {
  const s = soloDevice();
  if (!s) return;
  try {
    const { node, connected } = await startDirect(s, "recv");
    const files = [];
    let total = 0, summary = "";
    say("Receiving…");
    await s.device.receiveFiles(node, (ev) => {
      if (ev instanceof Uint8Array) files[files.length - 1].parts.push(ev);
      else if (ev.t === "file") files.push({ path: ev.path, size: ev.size, parts: [] });
      else if (ev.t === "manifest") {
        total = ev.total;
        summary = ev.summary;
        showProgress(0, total, "receiving");
      } else if (ev.t === "progress") showProgress(ev.bytes, total, "receiving");
    });
    await connected;
    s.device.close();
    state.received = files.map((f) => ({
      path: f.path,
      file: new File(f.parts, f.path.split("/").pop(), { type: mimeOf(f.path) }),
    }));
    showReceived();
    setSlot(s, "done", "sent");
    setSlot(state.slots.find((x) => x !== s), "done", "received");
    state.phase = "done";
    say(`Received ${esc(summary)}. Save the files below.`, "ok");
    navigator.vibrate?.([40, 60, 40]);
    render();
  } catch (e) {
    fail(e);
  }
}

function showReceived() {
  els.received.replaceChildren(
    ...state.received.map(({ path, file }) => {
      const li = document.createElement("li");
      const a = document.createElement("a");
      a.href = URL.createObjectURL(file);
      a.download = file.name;
      a.textContent = path;
      const size = document.createElement("span");
      size.className = "size";
      size.textContent = human(file.size);
      li.append(a, size);
      return li;
    }),
  );
  const files = state.received.map((r) => r.file);
  els.share.hidden = !(navigator.canShare && navigator.canShare({ files }));
}

els.share.addEventListener("click", () => {
  navigator.share({ files: state.received.map((r) => r.file) }).catch((e) => dlog(`share: ${errText(e)}`));
});

els.directRecv.addEventListener("click", directReceive);
// picking files must happen in the click itself (iOS), the transfer starts on change
els.directSend.addEventListener("click", () => els.directFiles.click());
els.directFiles.addEventListener("change", async () => {
  const picked = [...els.directFiles.files];
  const s = soloDevice();
  if (!picked.length || !s) return;
  try {
    const { node, connected } = await startDirect(s, "send");
    const total = picked.reduce((n, f) => n + f.size, 0);
    say(`Sending ${picked.length} file${picked.length > 1 ? "s" : ""}…`);
    showProgress(0, total, "sending");
    await s.device.sendFiles(node, picked, (ev) => {
      if (ev.t === "progress") showProgress(ev.bytes, ev.total, "sending");
    });
    await connected;
    s.device.close();
    setSlot(s, "done", "received");
    setSlot(state.slots.find((x) => x !== s), "done", "sent");
    state.phase = "done";
    say(`Sent ${picked.length} file${picked.length > 1 ? "s" : ""} (${human(total)}) to <b>${esc(s.dev.name)}</b>.`, "ok");
    navigator.vibrate?.([40, 60, 40]);
    render();
  } catch (e) {
    fail(e);
  } finally {
    els.directFiles.value = "";
  }
});

function setSlot(s, status, label, error = "") {
  Object.assign(s, { status, label, error });
  render();
}

function fail(e) {
  dlog(`pairing failed: ${errText(e)}`);
  els.diag.open = true;
  state.phase = "failed";
  state.slots.forEach((s) => {
    if (s.status === "busy") setSlot(s, "error", "failed", errText(e));
  });
  say(`Pairing failed: ${esc(errText(e))}. Restart qrtx on both computers and scan again.`, "err");
  render();
}

let flashTimer;
function flashMessage(text, kind) {
  const prev = [els.message.innerHTML, els.message.className];
  say(text, kind);
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => {
    if (els.message.innerHTML === text) say(prev[0], prev[1]);
  }, 2500);
}

// ---------- camera ----------

const scanner = { stream: null, detector: null, timer: 0, lastText: "" };

async function startScanner() {
  if (scanner.stream) return;
  if (!navigator.mediaDevices?.getUserMedia) {
    say("This browser can't use the camera here. Paste the link instead.", "warn");
    els.pasteBox.open = true;
    return;
  }
  try {
    scanner.stream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: { ideal: "environment" }, width: { ideal: 1280 }, height: { ideal: 1280 } },
      audio: false,
    });
  } catch (e) {
    say(`Camera unavailable (${esc(errText(e))}). You can paste the link instead.`, "warn");
    els.pasteBox.open = true;
    return;
  }
  els.video.srcObject = scanner.stream;
  els.viewfinder.hidden = false;
  els.scan.hidden = true;
  try {
    await els.video.play();
  } catch {}
  if (!scanner.detector && "BarcodeDetector" in window) {
    try {
      const formats = await window.BarcodeDetector.getSupportedFormats();
      if (formats.includes("qr_code")) scanner.detector = new window.BarcodeDetector({ formats: ["qr_code"] });
    } catch {}
  }
  scanLoop();
}

function stopScanner() {
  clearTimeout(scanner.timer);
  scanner.stream?.getTracks().forEach((t) => t.stop());
  scanner.stream = null;
  els.video.srcObject = null;
  els.viewfinder.hidden = true;
  render();
}

async function scanLoop() {
  if (!scanner.stream) return;
  let text = null;
  const v = els.video;
  if (v.readyState >= 2 && v.videoWidth) {
    try {
      text = scanner.detector ? await detectNative(v) : detectWasm(v);
    } catch (e) {
      console.warn("qr detect failed", e);
      scanner.detector = null; // fall back to wasm decoding
    }
  }
  if (text && text !== scanner.lastText) {
    scanner.lastText = text;
    if (addTicket(text)) {
      navigator.vibrate?.(50);
      els.viewfinder.classList.add("hit");
      setTimeout(() => els.viewfinder.classList.remove("hit"), 400);
    }
  }
  if (scanner.stream) scanner.timer = setTimeout(scanLoop, scanner.detector ? 100 : 60);
}

async function detectNative(video) {
  const codes = await scanner.detector.detect(video);
  return codes.map((c) => c.rawValue).find((t) => t) ?? null;
}

function detectWasm(video) {
  // decode the centre square, scaled down: fast enough for live video
  const side = Math.min(video.videoWidth, video.videoHeight);
  const size = Math.min(side, 720);
  const canvas = els.frame;
  if (canvas.width !== size) canvas.width = canvas.height = size;
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  ctx.drawImage(video, (video.videoWidth - side) / 2, (video.videoHeight - side) / 2, side, side, 0, 0, size, size);
  const rgba = ctx.getImageData(0, 0, size, size).data;
  const luma = new Uint8Array(size * size);
  for (let i = 0, j = 0; j < luma.length; i += 4, j++) {
    luma[j] = (rgba[i] * 77 + rgba[i + 1] * 150 + rgba[i + 2] * 29) >> 8;
  }
  return decodeQr(luma, size, size) ?? null;
}

// ---------- wiring ----------

function restart() {
  state.slots.forEach((s) => s.device?.close());
  els.received.querySelectorAll("a").forEach((a) => URL.revokeObjectURL(a.href));
  els.received.replaceChildren();
  els.share.hidden = true;
  els.directProgress.hidden = true;
  state.received = [];
  state.phase = "collect";
  state.slots = [emptySlot(), emptySlot()];
  scanner.lastText = "";
  say("");
  render();
}

els.scan.addEventListener("click", startScanner);
els.stopScan.addEventListener("click", stopScanner);
els.restart.addEventListener("click", restart);
els.pasteForm.addEventListener("submit", (ev) => {
  ev.preventDefault();
  const text = els.paste.value.trim();
  if (text && addTicket(text)) els.paste.value = "";
});

// a code scanned with the camera app while this page is already open
window.addEventListener("hashchange", () => {
  if (location.hash.length <= 1 || !nodePromise) return;
  const href = location.href;
  history.replaceState(null, "", location.pathname + location.search);
  addTicket(href);
});

// diagrams: honour reduced motion by freezing them on their most telling frame
const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)");
function syncDiagrams() {
  document.querySelectorAll("svg.dg-anim").forEach((svg, i) => {
    if (reduceMotion.matches) {
      svg.pauseAnimations();
      svg.setCurrentTime(i === 0 ? 8 : 0.6);
    } else {
      svg.unpauseAnimations();
    }
  });
}
reduceMotion.addEventListener?.("change", syncDiagrams);
syncDiagrams();

// click a command to copy it
document.querySelectorAll(".cmd").forEach((el) =>
  el.addEventListener("click", async () => {
    if (getSelection().toString()) return; // let people select by hand
    try {
      await navigator.clipboard.writeText(el.querySelector("pre").textContent);
      el.classList.add("copied");
      setTimeout(() => el.classList.remove("copied"), 1200);
    } catch {}
  }),
);

// show commands for wherever this copy of the site is hosted
if (location.protocol.startsWith("http")) {
  document.querySelectorAll(".origin").forEach((el) => (el.textContent = location.origin));
}

async function boot() {
  render();
  // opened by scanning a code with the phone's camera app: the ticket is in the fragment
  const fromUrl = location.hash.length > 1 ? location.href : null;
  if (fromUrl) history.replaceState(null, "", location.pathname + location.search);
  const keepDots = new URLSearchParams(location.search).has("relaydots");
  dlog(`page ${location.origin}${location.pathname}${keepDots ? " (relaydots)" : ""}`);
  dlog(`browser ${navigator.userAgent}`);
  dlog(
    `secure context ${isSecureContext}, WebAssembly ${typeof WebAssembly !== "undefined"}, WebSocket ${typeof WebSocket !== "undefined"}, ` +
      `BarcodeDetector ${"BarcodeDetector" in window}, camera API ${!!navigator.mediaDevices?.getUserMedia}, online ${navigator.onLine}`,
  );
  try {
    await init();
  } catch (e) {
    dlog(`wasm init failed: ${errText(e)}`);
    say(`This browser can't run qrtx (${esc(errText(e))}).`, "err");
    els.scan.disabled = true;
    els.diag.open = true;
    return;
  }
  dlog(`wasm ready after ${((performance.now() - t0) / 1000).toFixed(2)}s`);
  try {
    setLogger((line) => dlog(line), "warn,iroh=debug,iroh_relay=debug,qrtx_web=debug");
  } catch (e) {
    dlog(`no rust logs: ${errText(e)}`);
  }
  nodePromise = Node.create(keepDots);
  nodePromise.then((node) => dlog(`our endpoint ${node.id()}`));
  nodePromise.catch((e) => {
    dlog(`networking failed to start: ${errText(e)}`);
    say(`Couldn't start networking: ${esc(errText(e))}`, "err");
    els.diag.open = true;
  });
  if (fromUrl && addTicket(fromUrl)) startScanner();
}

// handy for debugging and automated tests
window.qrtx = { state, addTicket, restart };

boot();
