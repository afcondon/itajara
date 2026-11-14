#!/usr/bin/env node
// The audio round trip's residual: measure it, keep it, and say whether the
// number in force can still be believed.
//
//   node scripts/latency.mjs check                 no cable; the pre-flight
//   node scripts/latency.mjs show
//   node scripts/latency.mjs measure [--write]     needs a loopback cable
//
//   options: --device AUDIO4c   --out-ch 0   --itajara <binary>
//
// **What the residual is.** The samples a sound takes from the engine's
// output to its input beyond what the buffers account for. The engine
// subtracts it when it places an overdub, so a wrong one puts every overdub
// late (or early) by the difference, and nothing in the sound says so.
//
// **Why the clients are part of the measurement.** The residual moves when
// another program opens the audio device: 252 samples with Ableton quit, 301
// with it running, 275 once Continuo joined (measured 2026-08-14 and -19; the
// last went unnoticed for five days, 0.48 ms on every overdub). So the record
// lists which audio programs were running, and `check` compares that list
// with the programs running now. It needs no cable and takes a second.
//
// **Where it is kept.** The record is content in Amphora (kind
// `audio-latency.v1`, collection `audio-latency`), with the five buffer sizes
// it was fitted from. `~/.itajara/calibration.conf` is a copy of it in a form
// the engine reads at startup and a person reads with `cat`; the engine needs
// no HTTP client and starts with the store down.
//
// The measurement itself is `itajara sweep --json`: this adds the client
// list, the judgement and the keeping. It was `deepstar latency` until
// 2026-11-14, when DeepStar was retired; the record's shape is unchanged, so
// what DeepStar stored still reads.

import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const KIND = "audio-latency.v1";
export const COLLECTION = "audio-latency";
const here = dirname(fileURLToPath(import.meta.url));
const store = () => process.env.AMPHORA_URL ?? "http://localhost:3024";
export const confPath = () => join(homedir(), ".itajara", "calibration.conf");

// The audio programs worth noticing, by a piece of their command line.
//
// A list kept by hand, because there is no supported way to ask which
// processes hold a device. It asks "is it running", not "has it the device
// open", so it over-reports: a needless re-measure costs ten minutes, a
// missed one costs every recording until somebody notices.
const KNOWN = [
  ["ableton", "Ableton Live"],
  ["continuo", "continuo"],
  ["itajara", "itajara"],
  ["sclang", "sclang"],
  ["scsynth", "scsynth"],
  ["es9-daemon", "es9-daemon"],
  ["logic", "Logic Pro"],
  ["reaper", "REAPER"],
];

// Which known audio programs appear in a listing of command lines.
export const clientsIn = (listing) =>
  KNOWN.filter(([, match]) => listing.split("\n").some((line) => line.includes(match)))
    .map(([name]) => name)
    .sort();

// This script's own command line names `itajara` (its path), so it is left out.
export const clientsNow = () =>
  clientsIn(
    execFileSync("ps", ["ax", "-o", "pid=,command="], { encoding: "utf8" })
      .split("\n")
      .filter((line) => Number(line.trim().split(/\s+/)[0]) !== process.pid)
      .join("\n")
  );

// Why a measurement should not be kept, most important first. None: keep it.
export const problems = (l) => {
  const found = [];
  if (!l.usable) found.push(l.reason || "the sweep did not produce a single constant");
  if (!l.device) found.push("no device, and the residual is a property of one");
  if (!(l.residual_samples > 0)) found.push(`a residual of ${Math.round(l.residual_samples ?? 0)} samples is not a round trip`);
  // Two buffers is what cpal is known to over-account by. Any other slope
  // means the model does not fit, and a residual taken from it is arithmetic.
  if (!(l.slope_buffers >= 1.9 && l.slope_buffers <= 2.1)) {
    found.push(`the slope is ${Number(l.slope_buffers ?? 0).toFixed(2)} buffers, not the 2.00 a symmetric over-account gives`);
  }
  if (!(l.clients ?? []).length) found.push("no client list, so nothing can tell later whether the configuration still matches");
  return found;
};

// Whether the programs running now are the ones a record was measured with.
export const compare = (measuredWith, now) => {
  const arrived = now.filter((c) => !measuredWith.includes(c)).sort();
  const left = measuredWith.filter((c) => !now.includes(c)).sort();
  return { same: arrived.length === 0 && left.length === 0, arrived, left };
};

// The copy the engine reads. Not JSON: a person reads it once, when they
// first doubt it, and `residual_samples = 275` is more use then than a brace.
export const confText = (l, hash) => `# Itajara audio calibration, written by \`scripts/latency.mjs measure --write\`.
# Do not edit by hand: the record in Amphora is the measurement, this is a
# copy of it, and an edit here is a number with no evidence behind it.
#
# The residual moves when another program opens the audio device. \`clients\`
# is what was running when this was measured; if other programs are running
# now, measure again. \`scripts/latency.mjs check\` says which.

device           = ${l.device}
residual_samples = ${Math.round(l.residual_samples)}
residual_ms      = ${Number(l.residual_ms).toFixed(4)}
slope_buffers    = ${Number(l.slope_buffers).toFixed(2)}
sample_rate      = ${l.sample_rate}
measured_at      = ${l.measured_at}
clients          = ${l.clients.join(",")}
amphora          = ${hash}
`;

const ask = async (method, path, body) => {
  const answer = await fetch(store() + path, {
    method,
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(8000),
  });
  const text = await answer.text();
  if (!answer.ok) throw new Error(`the store answered ${answer.status} to ${method} ${path}: ${text.slice(0, 200)}`);
  return text === "ok" ? text : JSON.parse(text);
};

// Every kept record, oldest first.
export const records = async () => {
  const kept = await ask("GET", `/favorites?collection=${COLLECTION}`);
  const found = [];
  for (const favourite of kept) {
    const content = await ask("GET", `/content/${favourite.contentHash}`);
    try {
      found.push({ ...JSON.parse(content.payload), hash: favourite.contentHash });
    } catch {
      // Content in this collection that is not a record is not ours to judge.
    }
  }
  return found.sort((a, b) => String(a.measured_at).localeCompare(String(b.measured_at)));
};

// The newest record for a device, or null: an unmeasured interface is normal.
export const recordFor = async (device) =>
  (await records()).filter((l) => String(l.device).toLowerCase() === device.toLowerCase()).pop() ?? null;

// Whether the number in force can be believed. Never throws: this is asked
// from a web page, which wants a sentence either way.
export const check = async (device = "AUDIO4c") => {
  let now;
  try {
    now = clientsNow();
  } catch (error) {
    return { ok: false, device, says: `could not list the running programs: ${error.message}` };
  }
  let record;
  try {
    record = await recordFor(device);
  } catch (error) {
    return { ok: false, device, clientsNow: now, says: `the store did not answer, so the record could not be read: ${error.message}` };
  }
  if (!record) {
    return { ok: false, device, measured: false, clientsNow: now,
      says: `${device} has never been measured, so the engine is using its built-in guess of 252 samples` };
  }
  const { same, arrived, left } = compare(record.clients ?? [], now);
  const changes = [arrived.length ? `${arrived.join(", ")} now running` : "", left.length ? `${left.join(", ")} no longer running` : ""]
    .filter(Boolean).join("; ");
  return {
    ok: same, device, measured: true, same, arrived, left,
    residualSamples: record.residual_samples, residualMs: record.residual_ms,
    measuredAt: record.measured_at, clientsMeasured: record.clients ?? [], clientsNow: now,
    says: same
      ? `${Math.round(record.residual_samples)} samples, measured ${record.measured_at} with the same programs running as now`
      : `${Math.round(record.residual_samples)} samples was measured with other programs running (${changes}): measure again`,
  };
};

const measure = (itajara, device, outChannel) => {
  const clients = clientsNow();
  const said = execFileSync(itajara, ["sweep", "--device", device, "--out-ch", String(outChannel), "--json"],
    { encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], timeout: 5 * 60 * 1000 });
  // The object is the last line that opens a brace: `--json` silences the
  // prose, but a warning from the audio library is not this tool's to stop.
  const line = said.split("\n").map((l) => l.trim()).filter((l) => l.startsWith("{")).pop();
  if (!line) throw new Error("the sweep printed no result; is the loopback cable in?");
  return { ...JSON.parse(line), kind: KIND, label: JSON.parse(line).device, clients,
    measured_at: new Date().toISOString().replace(/\.\d+Z$/, "Z"), measured_by: "itajara latency" };
};

// Content, a name for it, and its place in the collection. The payload's keys
// are sorted so the same measurement always has the same hash.
const keep = async (l) => {
  const found = problems(l);
  if (found.length) throw new Error(`not kept: ${found[0]}`);
  const payload = JSON.stringify(l, Object.keys(l).sort());
  const { hash } = await ask("POST", "/content", { kind: KIND, payload });
  await ask("POST", "/labels", { contentHash: hash, name: `latency-${l.device}`, source: "itajara" });
  const kept = await ask("GET", `/favorites?collection=${COLLECTION}`);
  if (!kept.some((favourite) => favourite.contentHash === hash)) {
    await ask("POST", "/favorites", { contentHash: hash, collection: COLLECTION });
  }
  return hash;
};

const show = (l) => {
  console.log(`\n  ${l.device} at ${l.sample_rate} Hz, output ${l.out_ch}`);
  console.log(`  Residual: ${Math.round(l.residual_samples)} samples (${Number(l.residual_ms).toFixed(2)} ms), slope ${Number(l.slope_buffers).toFixed(2)} buffers`);
  if (l.points?.length) {
    console.log("\n  buffer   measured    implied");
    for (const p of l.points) console.log(`  ${String(p.buffer).padStart(6)}  ${p.measured.toFixed(1).padStart(9)}  ${p.implied.toFixed(1).padStart(9)}`);
    console.log("\n  A true residual shows the same implied figure at every buffer size.");
  }
  if (l.clients?.length) console.log(`\n  Measured with these running: ${l.clients.join(", ")}`);
  if (l.measured_at) console.log(`  Measured at ${l.measured_at}`);
  if (!l.usable && l.reason) console.log(`\n  ! ${l.reason}`);
};

const main = async (args) => {
  const option = (name, otherwise) => {
    const at = args.indexOf(name);
    return at < 0 ? otherwise : args[at + 1];
  };
  const verb = args.find((a) => ["measure", "check", "show"].includes(a)) ?? "check";
  const device = option("--device", "AUDIO4c");
  if (verb === "check") {
    const result = await check(device);
    console.log(args.includes("--json") ? JSON.stringify(result, null, 2) : `${device}: ${result.says}`);
    if (!args.includes("--json") && result.measured) {
      console.log(`  measured with: ${result.clientsMeasured.join(", ")}\n  running now:   ${result.clientsNow.join(", ")}`);
    }
    return result.ok ? 0 : 1;
  }
  if (verb === "show") {
    const record = await recordFor(device);
    if (!record) return console.log(`no record for ${device}.`), 1;
    show(record);
    return 0;
  }
  const itajara = option("--itajara", resolve(here, "../target/release/itajara"));
  console.log(`Measuring ${device}, output ${option("--out-ch", "0")}, through the loopback cable.`);
  const l = measure(itajara, device, option("--out-ch", "0"));
  show(l);
  const found = problems(l);
  if (found.length) return console.log(`\n  Not fit to keep: ${found[0]}`), 1;
  if (!args.includes("--write")) return console.log("\n  Not kept. Run again with --write to keep it and give it to the engine."), 0;
  const hash = await keep(l);
  mkdirSync(dirname(confPath()), { recursive: true });
  writeFileSync(confPath(), confText(l, hash));
  console.log(`\n  Kept as ${hash.slice(0, 12)}, and written to ${confPath()}.\n  Restart Itajara to use it; it prints the residual and where it came from when it starts.`);
  return 0;
};

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).then((code) => process.exit(code), (error) => {
    console.error(String(error.message ?? error));
    process.exit(1);
  });
}

// For the test of the copy the engine reads.
export const readConf = () => readFileSync(confPath(), "utf8");
export { keep, measure };
