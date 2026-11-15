// A control-voltage output, measured by patching it into a DC-coupled input:
// what voltage it gives for what it is asked.
//
// **How.** The output is stepped through its range and held at each step
// while the input's mean is read (`itajara levels --json`: on a DC-coupled
// input the mean is the voltage on the socket). A straight line through the
// readings is the output's gain and offset:
//
//     volts measured = gain x volts asked + offset
//
// so the level that gives V volts is `(V - offset) / (gain x voltsPerLevel)`.
//
// **What it cannot tell apart.** The reading is of the output and the input
// together. An output 1% low into an input 1% high reads as perfect. Outputs
// measured on the same input are right relative to each other; a meter on
// one held voltage is what makes them right absolutely, and this does not
// ask for one.
//
// **Where it is kept.** Content in Amphora, kind `cv-output.v1`, collection
// `cv-outputs`, one record per measurement with every step it was fitted
// from. Nothing reads them yet: es9-daemon does not correct its outputs.
//
// This module does not know how a voltage is sent. `sweep` is handed `send`
// (set the output to a level, -1 to 1) and `read` (what the inputs now say),
// so it can be held to its judgement without an interface.

import { spawn } from "node:child_process";
import { ask, sorted } from "./latency.mjs";

export const KIND = "cv-output.v1";
export const COLLECTION = "cv-outputs";

// What a level of 1.0 is taken to be worth, at both ends: the interface's
// full scale. The rig's ES-9 is +/-10 V out and in.
export const VOLTS_PER_LEVEL = 10;

// The steps of a sweep, as levels: -10 V to 10 V in 1.25 V steps.
export const LEVELS = Array.from({ length: 17 }, (_, index) => (index - 8) / 8);

// A reading this close to full scale is the input's limit, not the voltage.
const CLIPPED = 0.995;
// The probe: half scale, and how far an input must move to be the one.
const PROBE = 0.5, MOVED = 0.05;

const mean = (values) => values.reduce((sum, value) => sum + value, 0) / values.length;

// The inputs of a device, read continuously. `read(skip, take)` drops the
// next `skip` lines (a line in progress when the voltage changed is part old
// and part new) and gives the `take` after them, each an array of means, one
// per input channel.
export const meter = (itajara, device, seconds = 180) => {
  const child = spawn(itajara, ["levels", "--device", device, "--seconds", String(seconds), "--json"], { stdio: ["ignore", "pipe", "pipe"] });
  let pending = "", complaint = "", ended = null;
  const waiting = [];
  const fail = (error) => { ended = error; for (const waiter of waiting.splice(0)) waiter.fail(error); };
  child.stdout.on("data", (chunk) => {
    pending += chunk;
    const lines = pending.split("\n");
    pending = lines.pop();
    for (const line of lines) {
      let said;
      try { said = JSON.parse(line); } catch { continue; }
      if (!Array.isArray(said.mean) || !waiting.length) continue;
      const waiter = waiting[0];
      if (waiter.skip > 0) { waiter.skip -= 1; continue; }
      waiter.lines.push(said.mean);
      if (waiter.lines.length >= waiter.take) { waiting.shift(); waiter.done(waiter.lines); }
    }
  });
  child.stderr.on("data", (chunk) => { complaint += chunk; });
  child.on("error", fail);
  child.on("close", () => fail(new Error(complaint.trim().split("\n").pop() || "the meter stopped")));
  return {
    read: (skip, take) => new Promise((done, fail) => {
      if (ended) return fail(ended);
      waiting.push({ skip, take, lines: [], done, fail });
    }),
    stop: () => child.kill("SIGTERM"),
  };
};

// The straight line through the steps that did not clip, in volts.
export const fit = (points) => {
  const used = points.filter((point) => !point.clipped);
  if (used.length < 2) return null;
  const asked = used.map((point) => point.sent * VOLTS_PER_LEVEL), got = used.map((point) => point.measured * VOLTS_PER_LEVEL);
  const askedMean = mean(asked), gotMean = mean(got);
  const spread = asked.reduce((sum, value) => sum + (value - askedMean) ** 2, 0);
  if (spread === 0) return null;
  const gain = asked.reduce((sum, value, index) => sum + (value - askedMean) * (got[index] - gotMean), 0) / spread;
  const offset = gotMean - gain * askedMean;
  const worst = Math.max(...asked.map((value, index) => Math.abs(got[index] - (gain * value + offset))));
  return { gain, offset_volts: offset, worst_volts: worst, fitted: used.length };
};

// Why a measurement should not be kept, most important first. None: keep it.
export const problems = (record) => {
  const found = [];
  if (!(record.fitted >= 5)) found.push(`only ${record.fitted ?? 0} steps were inside the input's range, and a line wants five`);
  // No socket is this good. Some input channels of an interface carry its own
  // mixer, and an output routed there comes back as the number that was sent.
  if (Math.abs(record.gain - 1) < 1e-4 && Math.abs(record.offset_volts) < 2e-4) found.push("the reading is exactly what was asked, which no socket is: that input carries the interface's own mix, not a voltage on a cable");
  if (!(record.gain > 0.5 && record.gain < 2)) found.push(`a gain of ${Number(record.gain).toFixed(3)} is not a cable: something in the path attenuates, amplifies or inverts`);
  if (record.worst_volts > 0.05) found.push(`the steps are not on a line (one is ${record.worst_volts.toFixed(3)} V off it): something in the path is not a cable`);
  return found;
};

// One sweep. `input` is the channel to read (from zero), or null for
// whichever input moves when the output does. `onStep` is given each step as
// it is read. Resolves to `{ in_ch, rest, points, ...fit }`; rejects with a
// sentence when the patch is not what a sweep needs. The output is left at
// zero either way. `stopped()` is asked before each step.
export const sweep = async ({ send, read, input = null, onStep = () => {}, levels = LEVELS, stopped = () => false }) => {
  const held = async (skip = 2, take = 3) => {
    const lines = await read(skip, take);
    return lines[0].map((_, channel) => mean(lines.map((line) => line[channel])));
  };
  try {
    await send(0);
    const rest = await held();
    await send(PROBE);
    const probed = await held();
    const moved = probed.map((value, channel) => value - rest[channel]);
    // An input that moved by exactly what was sent carries the interface's
    // own mix, not a socket: taken only if nothing else answered.
    const exact = (value) => Math.abs(value - PROBE) < 1e-5;
    const better = (value, best) => (exact(value) !== exact(best) ? exact(best) : Math.abs(value) > Math.abs(best));
    const loudest = moved.reduce((best, value, channel) => (Math.abs(value) >= MOVED && (Math.abs(moved[best]) < MOVED || better(value, moved[best])) ? channel : best), 0);
    const answered = Math.abs(moved[loudest]) >= MOVED;
    if (input === null && !answered) throw new Error("no input moved when the output went to 5 V. Is the cable in, and the input DC-coupled?");
    if (input !== null && Math.abs(moved[input] ?? 0) < MOVED) {
      const error = new Error("the chosen input did not move when the output went to 5 V");
      if (answered) error.elsewhere = loudest;
      throw error;
    }
    const channel = input ?? loudest;
    // An AC-coupled input follows the step and then lets go of it.
    const later = (await held(6, 3))[channel];
    if (Math.abs(later - probed[channel]) > 0.1 * Math.abs(moved[channel])) {
      throw new Error(`the reading fell from ${(probed[channel] * VOLTS_PER_LEVEL).toFixed(2)} V to ${(later * VOLTS_PER_LEVEL).toFixed(2)} V while the output held 5 V: this input is AC-coupled`);
    }
    const points = [];
    for (const level of levels) {
      if (stopped()) throw new Error("stopped before it finished");
      await send(level);
      const lines = (await read(2, 3)).map((line) => line[channel]);
      const measured = mean(lines);
      const point = { sent: level, measured, spread: Math.max(...lines) - Math.min(...lines), clipped: Math.abs(measured) >= CLIPPED };
      points.push(point);
      onStep(point);
    }
    const line = fit(points);
    if (!line) throw new Error("every step was at the input's limit, so there is nothing to fit");
    return { in_ch: channel, rest_volts: rest[channel] * VOLTS_PER_LEVEL, points, volts_per_level: VOLTS_PER_LEVEL, ...line };
  } finally {
    await send(0);
  }
};

// Keep a record: content, a name for it, and its place in the collection.
export const keep = async (record) => {
  const found = problems(record);
  if (found.length) throw new Error(`not kept: ${found[0]}`);
  const { hash } = await ask("POST", "/content", { kind: KIND, payload: JSON.stringify(sorted(record)) });
  await ask("POST", "/labels", { contentHash: hash, name: `cv-${record.output.label}`, source: "quadrat" });
  const kept = await ask("GET", `/favorites?collection=${COLLECTION}`);
  if (!kept.some((favourite) => favourite.contentHash === hash)) await ask("POST", "/favorites", { contentHash: hash, collection: COLLECTION });
  return hash;
};

// Every kept record, oldest first.
export const records = async () => {
  const found = [];
  for (const favourite of await ask("GET", `/favorites?collection=${COLLECTION}`)) {
    const content = await ask("GET", `/content/${favourite.contentHash}`);
    try { found.push({ ...JSON.parse(content.payload), hash: favourite.contentHash }); } catch { /* not a record of ours */ }
  }
  return found.sort((a, b) => String(a.measured_at).localeCompare(String(b.measured_at)));
};
