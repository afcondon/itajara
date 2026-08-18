#!/usr/bin/env node
// Publish a saved take to Amphora.
//
//   node scripts/publish-take.mjs ~/.itajara/takes/<name> [--label "..."]
//
// Itajara writes takes to disk and stops there; this puts the *manifest* into
// the Atlantis artefact store, so a phrase played on the guitar becomes a thing
// the rig can name, find and derive from. Deliberately a separate step and a
// separate process: the daemon owns buffers and the sample clock and has no
// business holding an HTTP client, which is the same split that keeps it out of
// the MIDI path.
//
// **The audio stays on disk.** Amphora's `content.payload` is TEXT hashed as a
// string — it was built for small recipes (its 78 rows are calibration tables,
// patterns and scenes in 8 MB), and a single eight-layer take would be some
// 61 MB of base64. So what goes in is the manifest, and the WAVs stay under the
// takes directory.
//
// The consequence to know about: the audio is therefore NOT content-addressed,
// so the dedup and machine-to-machine merge properties Amphora claims do not
// extend to it. A take is only whole on the machine that recorded it. Teaching
// Amphora a blob store would fix that, and is the reason `sha256` is recorded
// below rather than left out — the manifest already names the bytes it means,
// so moving them into a real blob store later is a migration and not a redesign.
//
// Why `sha256` per layer is not optional: without it two takes with the same
// layer lengths and shapes — trivially common, since a loop's length is the
// thing you keep — would produce byte-identical manifests, hash to one address,
// and the second take would silently become the first. Content-addressing an
// incomplete description is worse than not content-addressing at all.
//
// Paths are relative on purpose. An absolute path baked into the hashed payload
// would make the same take hash differently on two machines and would rot the
// moment the takes directory moved; where the takes directory *is* stays a
// local question.

import { createHash } from "node:crypto";
import { readFile, readdir } from "node:fs/promises";
import { basename, join, resolve } from "node:path";

const AMPHORA = process.env.AMPHORA_URL ?? "http://localhost:3024";

const args = process.argv.slice(2);
const dir = args.find((a) => !a.startsWith("--"));
const labelIdx = args.indexOf("--label");
const label = labelIdx >= 0 ? args[labelIdx + 1] : null;

if (!dir) {
  console.error("usage: publish-take.mjs <take-dir> [--label \"name\"]");
  process.exit(2);
}

const takeDir = resolve(dir.replace(/^~/, process.env.HOME ?? "~"));
const takeName = basename(takeDir);

const manifestPath = join(takeDir, "take.json");
let onDisk;
try {
  onDisk = JSON.parse(await readFile(manifestPath, "utf8"));
} catch (e) {
  console.error(`no readable take.json in ${takeDir}: ${e.message}`);
  process.exit(1);
}

// Hash each layer's actual bytes, and check the manifest is describing files
// that are really there. A take that lost a WAV should fail here rather than
// publish a manifest pointing at nothing.
const present = new Set(await readdir(takeDir));
const layers = [];
for (const l of onDisk.layers ?? []) {
  if (!present.has(l.file)) {
    console.error(`${takeName}: manifest names ${l.file}, which is not in the directory`);
    process.exit(1);
  }
  const bytes = await readFile(join(takeDir, l.file));
  layers.push({
    file: l.file,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    bytes: bytes.length,
    len: l.len,
    period: l.period,
    phase: l.phase,
  });
}

if (layers.length === 0) {
  console.error(`${takeName}: no layers to publish`);
  process.exit(1);
}

// Key order is fixed by construction here, which is what makes the payload
// canonical: JSON.stringify preserves insertion order, so the same take always
// produces the same bytes and therefore the same address.
const payload = JSON.stringify({
  version: 1,
  take: takeName,
  sampleRate: onDisk.sampleRate,
  loopFrames: onDisk.loopFrames,
  loopSecs: onDisk.loopSecs,
  layers,
});

async function post(path, body) {
  const res = await fetch(`${AMPHORA}${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`${path} → ${res.status} ${text}`);
  return JSON.parse(text);
}

const { hash, deduped } = await post("/content", { kind: "itajara-take", payload });

console.log(`${deduped ? "already stored" : "stored"} ${takeName}`);
console.log(`  hash    ${hash}`);
console.log(`  layers  ${layers.length}, loop ${onDisk.loopSecs?.toFixed?.(3) ?? "?"} s`);

if (label) {
  const l = await post("/labels", { contentHash: hash, name: label, source: "itajara" });
  console.log(`  label   ${label} (id ${l.id ?? "?"})`);
}

// The path SuperDirt needs. Printed rather than stored, because it is a fact
// about this machine and not about the take.
console.log(`  audio   ${takeDir}`);
