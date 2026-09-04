// The Friend's server: the static page, and the two things a browser cannot
// do for itself — write a take's notes to disk, and run `msm harvest`.
//
// Zero dependencies and one file on purpose. The daemon must not spawn a
// process (audio thread, and shaping is msm's job); the page cannot; so this
// is the seam, and it is deliberately as small as a seam can be. Node 18+.
//
//   node server.mjs            serves ./static on :3029
//   PORT=3030 MSM=/path/msm    override the port and the msm binary
//
//   GET  /api/takes                the takes under ~/.itajara/takes, newest first
//   GET  /api/takes/:name/notes    the take's notes.json, or {}
//   PUT  /api/takes/:name/notes    write it
//   GET  /api/sticks               mounted volumes that look like an Arbhar stick
//   POST /api/harvest              { take, module, stick, bank, scene, overwrite,
//                                    allLayers, dryRun } → runs msm harvest,
//                                    answers { ok, output }

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { spawn } from "node:child_process";

const PORT = Number(process.env.PORT || 3029);
const MSM = process.env.MSM || "msm";
const STATIC = path.join(path.dirname(new URL(import.meta.url).pathname), "static");
const TAKES = path.join(os.homedir(), ".itajara", "takes");

const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".css": "text/css", ".json": "application/json" };

// A take name that cannot leave the takes directory: the same rule the daemon
// applies, so the name the page sends is the folder the daemon made.
const safe = (s) => String(s).replace(/[^A-Za-z0-9_-]/g, "_") || "take";

function json(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => {
      try { resolve(data ? JSON.parse(data) : {}); } catch (e) { reject(e); }
    });
    req.on("error", reject);
  });
}

function takes() {
  if (!fs.existsSync(TAKES)) return [];
  return fs.readdirSync(TAKES)
    .filter((n) => fs.existsSync(path.join(TAKES, n, "export.json")))
    .map((n) => {
      const st = fs.statSync(path.join(TAKES, n, "export.json"));
      let loops = 0, kind = "";
      try {
        const m = JSON.parse(fs.readFileSync(path.join(TAKES, n, "export.json"), "utf8"));
        loops = m.loops.length;
        kind = m.kind;
      } catch {}
      return { name: n, kind, savedAt: st.mtimeMs, loops, harvested: fs.existsSync(path.join(TAKES, n, "datasheet.json")) };
    })
    // Only what a harvest can read: a flat `ex` set has no layers.
    .filter((t) => t.kind === "layers")
    .sort((a, b) => b.savedAt - a.savedAt);
}

function sticks() {
  const vols = "/Volumes";
  if (!fs.existsSync(vols)) return [];
  return fs.readdirSync(vols)
    .map((n) => path.join(vols, n))
    .filter((p) => { try { return fs.statSync(path.join(p, "_arbhar_library")).isDirectory(); } catch { return false; } });
}

function harvest(body) {
  const args = ["harvest", safe(body.take), "--module", body.module || "arbhar"];
  if (body.stick) args.push("--stick", String(body.stick));
  if (body.bank) args.push("--bank", String(Number(body.bank)));
  if (body.scene) args.push("--scene", String(body.scene).replace(/[^0-9_]/g, ""));
  if (body.overwrite) args.push("--overwrite");
  if (body.allLayers) args.push("--all-layers");
  if (body.dryRun) args.push("--dry-run");
  return new Promise((resolve) => {
    let out = "";
    let child;
    try {
      child = spawn(MSM, args);
    } catch (e) {
      return resolve({ ok: false, output: `could not start ${MSM}: ${e.message}` });
    }
    child.stdout.on("data", (c) => (out += c));
    child.stderr.on("data", (c) => (out += c));
    child.on("error", (e) => resolve({ ok: false, output: `could not run ${MSM}: ${e.message}. Is msm built and on the PATH (cargo install --path SamplesProject/msm)?` }));
    child.on("close", (code) => resolve({ ok: code === 0, output: `$ ${MSM} ${args.join(" ")}\n${out}` }));
  });
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, "http://x");
  const m = url.pathname.match(/^\/api\/takes\/([^/]+)\/notes$/);
  try {
    if (url.pathname === "/api/takes" && req.method === "GET") return json(res, 200, takes());
    if (url.pathname === "/api/sticks" && req.method === "GET") return json(res, 200, sticks());
    if (m && req.method === "GET") {
      const p = path.join(TAKES, safe(m[1]), "notes.json");
      return json(res, 200, fs.existsSync(p) ? JSON.parse(fs.readFileSync(p, "utf8")) : {});
    }
    if (m && req.method === "PUT") {
      const dir = path.join(TAKES, safe(m[1]));
      fs.mkdirSync(dir, { recursive: true });
      const body = await readBody(req);
      fs.writeFileSync(path.join(dir, "notes.json"), JSON.stringify(body, null, 2) + "\n");
      return json(res, 200, { ok: true, path: path.join(dir, "notes.json") });
    }
    if (url.pathname === "/api/harvest" && req.method === "POST") {
      const body = await readBody(req);
      return json(res, 200, await harvest(body));
    }
    if (url.pathname.startsWith("/api/")) return json(res, 404, { error: "no such route" });

    // Static, rooted, no traversal.
    let file = path.normalize(path.join(STATIC, url.pathname === "/" ? "index.html" : url.pathname));
    if (!file.startsWith(STATIC)) return json(res, 403, { error: "outside static" });
    if (!fs.existsSync(file) || fs.statSync(file).isDirectory()) file = path.join(STATIC, "index.html");
    res.writeHead(200, { "content-type": TYPES[path.extname(file)] || "application/octet-stream", "cache-control": "no-cache" });
    fs.createReadStream(file).pipe(res);
  } catch (e) {
    json(res, 500, { error: e.message });
  }
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`the Friend on http://localhost:${PORT}/  (takes in ${TAKES}, msm = ${MSM})`);
});
