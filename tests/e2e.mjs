#!/usr/bin/env node
// End-to-end test: two real qrtx devices, the static site, and headless
// Chromium playing the phone. The browser's camera is a fake video made from
// the QR codes the devices print to their terminals, so this exercises the
// whole path: terminal QR -> camera scan -> wasm iroh pairing -> tunnel.
//
// usage: node tests/e2e.mjs <path/to/qrtx> [site-dir]
// needs: chromium (or $CHROME), network access to the n0 relays
import { spawn, execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync, rmSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";

const QRTX = path.resolve(process.env.QRTX_BIN ?? process.argv[2] ?? "target/release/qrtx");
const SITE = path.resolve(process.argv[3] ?? new URL("../site", import.meta.url).pathname);
const CHROME = process.env.CHROME ?? "chromium";
const TIMEOUT = 120_000;

const tmp = mkdtempSync(path.join(tmpdir(), "qrtx-e2e-"));
const cleanups = [];
const log = (...a) => console.log("[e2e]", ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function withTimeout(p, ms, what) {
  let t;
  const timeout = new Promise((_, rej) => (t = setTimeout(() => rej(new Error(`timeout: ${what}`)), ms)));
  try {
    return await Promise.race([p, timeout]);
  } finally {
    clearTimeout(t);
  }
}

// ---------- static site ----------

const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".wasm": "application/wasm" };

export function serveSite() {
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    let file = path.join(SITE, decodeURIComponent(url.pathname));
    if (!file.startsWith(SITE)) return res.writeHead(403).end();
    if (url.pathname.endsWith("/")) file = path.join(file, "index.html");
    if (!existsSync(file)) return res.writeHead(404).end();
    res.writeHead(200, { "content-type": MIME[path.extname(file)] ?? "application/octet-stream" });
    res.end(readFileSync(file));
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => {
    cleanups.push(() => server.close());
    resolve(`http://127.0.0.1:${server.address().port}/`);
  }));
}

// ---------- devices ----------

/** Starts qrtx; resolves once it printed its QR code. */
export function startDevice(name, args, site, { stdin = "ignore", bin = QRTX } = {}) {
  const proc = spawn(bin, args, {
    env: { ...process.env, QRTX_SITE: site, QRTX_NAME: name, RUST_LOG: process.env.RUST_LOG ?? "warn" },
    stdio: [stdin === "ignore" ? "ignore" : "pipe", "pipe", "pipe"],
  });
  cleanups.push(() => proc.kill("SIGKILL"));
  const dev = { name, proc, stderr: "", stdout: [], exited: null };
  proc.stdout.on("data", (d) => dev.stdout.push(d));
  dev.exited = new Promise((resolve) => proc.on("exit", (code) => resolve(code)));
  return new Promise((resolve, reject) => {
    proc.stderr.on("data", (d) => {
      dev.stderr += d;
      if (process.env.VERBOSE) process.stderr.write(`[${name}] ${d}`);
      const m = dev.stderr.match(/(https?:\/\/\S+#Q1\S+)/);
      if (m && !dev.url) {
        dev.url = m[1];
        dev.qr = parseTerminalQr(dev.stderr);
        resolve(dev);
      }
    });
    proc.on("exit", (code) => reject(new Error(`${name} exited (${code}) before showing a code:\n${dev.stderr}`)));
  });
}

/** Turns the half-block QR the CLI drew back into a module grid. */
function parseTerminalQr(text) {
  const rows = [];
  for (const raw of text.split("\n")) {
    if (!raw.includes("\x1b[38;5;16;48;5;231m")) continue;
    const line = raw.replace(/\x1b\[[0-9;]*m/g, "").replace(/^ {2}/, "");
    const top = [], bottom = [];
    for (const ch of line) {
      top.push(ch === "█" || ch === "▀");
      bottom.push(ch === "█" || ch === "▄");
    }
    rows.push(top, bottom);
  }
  if (!rows.length) throw new Error("no QR code found in output");
  return rows;
}

// ---------- fake camera ----------

/** Writes a y4m video that alternates between the given QR grids. */
function writeY4m(file, grids, { w = 640, h = 480, fps = 10, secondsEach = 2 } = {}) {
  const ySize = w * h, cSize = (w / 2) * (h / 2);
  const frames = grids.map((grid) => {
    const y = Buffer.alloc(ySize, 235);
    const n = grid.length;
    const scale = Math.floor((Math.min(w, h) * 0.8) / n);
    const ox = Math.floor((w - n * scale) / 2), oy = Math.floor((h - n * scale) / 2);
    for (let r = 0; r < n; r++)
      for (let c = 0; c < grid[r].length; c++)
        if (grid[r][c])
          for (let dy = 0; dy < scale; dy++) y.fill(20, (oy + r * scale + dy) * w + ox + c * scale, (oy + r * scale + dy) * w + ox + (c + 1) * scale);
    return Buffer.concat([Buffer.from("FRAME\n"), y, Buffer.alloc(cSize * 2, 128)]);
  });
  const parts = [Buffer.from(`YUV4MPEG2 W${w} H${h} F${fps}:1 Ip A1:1 C420jpeg\n`)];
  for (const f of frames) for (let i = 0; i < fps * secondsEach; i++) parts.push(f);
  writeFileSync(file, Buffer.concat(parts));
}

// ---------- chromium over CDP ----------

export async function startChrome(video) {
  const profile = path.join(tmp, `chrome-${Date.now()}`);
  const args = [
    "--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-gpu",
    `--user-data-dir=${profile}`, "--remote-debugging-port=0",
    "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
    ...(video ? [`--use-file-for-fake-video-capture=${video}`] : []),
    "about:blank",
  ];
  const proc = spawn(CHROME, args, { stdio: ["ignore", "ignore", "pipe"] });
  cleanups.push(() => proc.kill("SIGKILL"));
  const wsUrl = await withTimeout(new Promise((resolve) => {
    let buf = "";
    proc.stderr.on("data", (d) => {
      buf += d;
      const m = buf.match(/DevTools listening on (ws:\/\/\S+)/);
      if (m) resolve(m[1]);
    });
  }), 20_000, "chromium start");
  const port = new URL(wsUrl).port;
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  const page = targets.find((t) => t.type === "page");
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
    } else if (msg.method === "Runtime.consoleAPICalled" && process.env.VERBOSE) {
      console.log("[page]", msg.params.args.map((a) => a.value ?? a.description).join(" "));
    } else if (msg.method === "Runtime.exceptionThrown") {
      console.log("[page exception]", msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text);
    }
  };
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    pending.set(++id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
    return r.result.value;
  };
  await send("Runtime.enable");
  await send("Page.enable");
  const close = () => { ws.close(); proc.kill("SIGKILL"); };
  return { send, evaluate, close };
}

export async function waitForPairing(chrome) {
  const start = Date.now();
  let last = "";
  while (Date.now() - start < TIMEOUT) {
    const s = await chrome.evaluate(`(() => {
      const q = window.qrtx;
      if (!q) return null;
      return { phase: q.state.phase, slots: q.state.slots.map(s => [s.status, s.label, s.error]),
               msg: document.querySelector('#message').textContent };
    })()`).catch(() => null);
    if (s) {
      const line = JSON.stringify(s);
      if (line !== last) log("page:", line);
      last = line;
      if (s.phase === "done") return s;
      if (s.phase === "failed") throw new Error(`pairing failed: ${s.msg}`);
    }
    await sleep(250);
  }
  throw new Error("pairing timed out");
}

// ---------- scenarios ----------

async function tcpScenario(site) {
  log("== tcp: listen-tcp <-> connect-tcp, paired through the fake camera");
  const echo = net.createServer((s) => s.pipe(s));
  await new Promise((r) => echo.listen(0, "127.0.0.1", r));
  cleanups.push(() => echo.close());
  const localPort = 20000 + Math.floor(Math.random() * 20000);
  const server = await startDevice("e2e-server", ["listen-tcp", "--host", `127.0.0.1:${echo.address().port}`], site);
  const laptop = await startDevice("e2e-laptop", ["connect-tcp", "--addr", `127.0.0.1:${localPort}`], site);
  log("server code:", server.url);
  log("laptop code:", laptop.url);

  const video = path.join(tmp, "tcp.y4m");
  writeY4m(video, [server.qr, laptop.qr]);
  const chrome = await startChrome(video);
  await chrome.send("Page.navigate", { url: site });
  await withTimeout((async () => { while (!(await chrome.evaluate("!!window.qrtx").catch(() => false))) await sleep(100); })(), 20_000, "page load");
  await chrome.evaluate("document.querySelector('#scan').click()");
  await waitForPairing(chrome);
  chrome.close();

  // two connections through the tunnel, both echoed by the server side
  for (const n of [1, 2]) {
    const payload = randomBytes(256 * 1024);
    const got = await withTimeout(new Promise((resolve, reject) => {
      const chunks = [];
      const sock = net.connect(localPort, "127.0.0.1", () => sock.end(payload));
      sock.on("data", (d) => chunks.push(d));
      sock.on("end", () => resolve(Buffer.concat(chunks)));
      sock.on("error", reject);
    }), 30_000, `tcp echo #${n}`);
    if (!got.equals(payload)) throw new Error(`echo #${n} mismatch: sent ${payload.length}, got ${got.length}`);
    log(`tcp echo #${n} ok (${payload.length} bytes)`);
  }
  server.proc.kill("SIGINT");
  laptop.proc.kill("SIGINT");
}

async function pipeScenario(site) {
  log("== pipe: stdin -> stdout, first code via URL fragment, second pasted");
  const payload = randomBytes(8 * 1024 * 1024);
  const sender = await startDevice("e2e-sender", [], site, { stdin: "pipe" });
  const receiver = await startDevice("e2e-receiver", ["pipe"], site, { stdin: "pipe" });
  receiver.proc.stdin.end(); // nothing to send back
  sender.proc.stdin.on("error", () => {});
  sender.proc.stdin.end(payload);

  const chrome = await startChrome(null);
  // like opening the first code with the phone's camera app
  await chrome.send("Page.navigate", { url: sender.url });
  await withTimeout((async () => { while (!(await chrome.evaluate("!!window.qrtx").catch(() => false))) await sleep(100); })(), 20_000, "page load");
  const hash = await chrome.evaluate("location.hash");
  if (hash) throw new Error("ticket was left in the URL fragment");
  await chrome.evaluate(`document.querySelector('#paste-box').open = true;
    document.querySelector('#paste').value = ${JSON.stringify(receiver.url)};
    document.querySelector('#paste-form').requestSubmit();`);
  await waitForPairing(chrome);
  chrome.close();

  const [codeS, codeR] = await withTimeout(Promise.all([sender.exited, receiver.exited]), 60_000, "pipe processes to exit");
  const got = Buffer.concat(receiver.stdout);
  const sha = (b) => createHash("sha256").update(b).digest("hex").slice(0, 16);
  if (codeS !== 0 || codeR !== 0) throw new Error(`exit codes sender=${codeS} receiver=${codeR}\n${sender.stderr}\n${receiver.stderr}`);
  if (!got.equals(payload)) throw new Error(`payload mismatch: sent ${payload.length} (${sha(payload)}), got ${got.length} (${sha(got)})`);
  log(`pipe ok: ${got.length} bytes, sha256 ${sha(got)}…, both sides exited 0`);
}

async function wrongRolesScenario(site) {
  log("== incompatible roles are refused before anything is set up");
  const a = await startDevice("e2e-a", ["listen-tcp", "--host", "127.0.0.1:1"], site);
  const b = await startDevice("e2e-b", ["listen-tcp", "--host", "127.0.0.1:2"], site);
  const chrome = await startChrome(null);
  await chrome.send("Page.navigate", { url: site });
  await withTimeout((async () => { while (!(await chrome.evaluate("!!window.qrtx").catch(() => false))) await sleep(100); })(), 20_000, "page load");
  await chrome.evaluate(`window.qrtx.addTicket(${JSON.stringify(a.url)}); window.qrtx.addTicket(${JSON.stringify(b.url)});`);
  try {
    await waitForPairing(chrome);
    throw new Error("expected pairing to fail");
  } catch (e) {
    if (!/can't be paired/.test(e.message)) throw e;
    log("refused as expected:", e.message);
  }
  chrome.close();
  a.proc.kill("SIGINT");
  b.proc.kill("SIGINT");
}

async function sshScenario(site) {
  log("== ssh: `qrtx ssh` <-> `qrtx sshd` against a real unprivileged sshd, one session only");
  let sshdBin;
  try {
    sshdBin = execFileSync("sh", ["-c", "command -v sshd"]).toString().trim();
  } catch {
    log("sshd not found, skipping");
    return;
  }
  const dir = path.join(tmp, "ssh");
  mkdirSync(dir);
  for (const k of ["host_key", "client_key"]) execFileSync("ssh-keygen", ["-q", "-t", "ed25519", "-N", "", "-f", path.join(dir, k)]);
  writeFileSync(path.join(dir, "authorized_keys"), readFileSync(path.join(dir, "client_key.pub")));
  const port = 20000 + Math.floor(Math.random() * 20000);
  writeFileSync(path.join(dir, "sshd_config"), [
    `Port ${port}`, "ListenAddress 127.0.0.1", `HostKey ${dir}/host_key`, `PidFile ${dir}/sshd.pid`,
    `AuthorizedKeysFile ${dir}/authorized_keys`, "StrictModes no", "UsePAM no",
    "PasswordAuthentication no", "KbdInteractiveAuthentication no", "",
  ].join("\n"));
  const sshd = spawn(sshdBin, ["-D", "-e", "-f", path.join(dir, "sshd_config")], { stdio: ["ignore", "ignore", "pipe"] });
  let sshdLog = "";
  sshd.stderr.on("data", (d) => (sshdLog += d));
  cleanups.push(() => sshd.kill("SIGKILL"));
  await withTimeout((async () => {
    for (;;) {
      const ok = await new Promise((r) => net.connect(port, "127.0.0.1").on("connect", function () { this.destroy(); r(true); }).on("error", () => r(false)));
      if (ok) return;
      await sleep(100);
    }
  })(), 10_000, `sshd to listen\n${sshdLog}`);

  const server = await startDevice("e2e-sshd", ["sshd", "--host", `127.0.0.1:${port}`], site);
  const client = await startDevice("e2e-ssh", [
    "ssh", "-F", "/dev/null", "-i", path.join(dir, "client_key"), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
    "-o", `UserKnownHostsFile=${dir}/known_hosts`, "-o", "StrictHostKeyChecking=accept-new",
    "e2e-host", "echo hello-from-sshd; uname -s",
  ], site);
  const chrome = await startChrome(null);
  await chrome.send("Page.navigate", { url: site });
  await withTimeout((async () => { while (!(await chrome.evaluate("!!window.qrtx").catch(() => false))) await sleep(100); })(), 20_000, "page load");
  await chrome.evaluate(`window.qrtx.addTicket(${JSON.stringify(server.url)}); window.qrtx.addTicket(${JSON.stringify(client.url)});`);
  await waitForPairing(chrome);
  chrome.close();

  const code = await withTimeout(client.exited, 30_000, "ssh to finish");
  const out = Buffer.concat(client.stdout).toString();
  if (code !== 0 || !out.includes("hello-from-sshd")) throw new Error(`ssh failed (exit ${code}): ${out}\n${client.stderr}\n${sshdLog}`);
  log(`ssh ok: ${JSON.stringify(out.trim())}`);
  const serverCode = await withTimeout(server.exited, 15_000, "qrtx sshd to exit after one session");
  if (serverCode !== 0) throw new Error(`qrtx sshd exited ${serverCode}\n${server.stderr}`);
  if (!existsSync(path.join(dir, "known_hosts")) || !readFileSync(path.join(dir, "known_hosts"), "utf8").startsWith("e2e-host ")) {
    throw new Error("host key was not recorded under the destination name");
  }
  log("qrtx sshd exited by itself after the session; host key recorded as e2e-host");
}

/** Opens the page and adds both codes as if they had been scanned. */
export async function pairViaPage(site, ...urls) {
  const chrome = await startChrome(null);
  await chrome.send("Page.navigate", { url: site });
  await withTimeout((async () => { while (!(await chrome.evaluate("!!window.qrtx").catch(() => false))) await sleep(100); })(), 20_000, "page load");
  await chrome.evaluate(urls.map((u) => `window.qrtx.addTicket(${JSON.stringify(u)});`).join(""));
  try {
    return await waitForPairing(chrome);
  } finally {
    chrome.close();
  }
}

export { cleanups, log, sleep };

const scenarios = { tcp: tcpScenario, pipe: pipeScenario, roles: wrongRolesScenario, ssh: sshScenario };
const only = process.env.SCENARIOS?.split(",") ?? Object.keys(scenarios);

if (import.meta.main) {
let failed = false;
try {
  const site = await serveSite();
  log("site at", site, "binary", QRTX);
  for (const name of only) {
    await scenarios[name](site);
  }
  log("all good");
} catch (e) {
  failed = true;
  console.error("[e2e] FAILED:", e.message);
} finally {
  for (const c of cleanups.reverse()) try { c(); } catch {}
  rmSync(tmp, { recursive: true, force: true });
  process.exit(failed ? 1 : 0);
}
}
