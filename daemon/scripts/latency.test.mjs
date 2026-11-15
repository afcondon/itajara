// The judgement in latency.mjs, held to the cases DeepStar's tests held it to
// (internal/rig/latency_test.go, before DeepStar was retired 2026-11-14).
//
//   node --test scripts/latency.test.mjs
//
// With AMPHORA_URL set (by Amphora's witness, `make wire`), it also keeps a
// record in that store and reads it back.

import assert from "node:assert/strict";
import { test } from "node:test";
import { check, clientsIn, compare, confText, keep, problems, recordFor } from "./latency.mjs";

const good = { device: "AUDIO4c", residual_samples: 275, residual_ms: 5.7291, slope_buffers: 2.0,
  sample_rate: 48000, usable: true, clients: ["continuo", "sclang"], measured_at: "2026-08-19T15:34:30Z",
  points: [{ buffer: 64, measured: 147, implied: 275, channel: 0 }, { buffer: 128, measured: 19, implied: 275, channel: 0 }] };

test("the order of the programs does not matter", () => {
  assert.deepEqual(compare(["continuo", "es9-daemon", "sclang"], ["sclang", "es9-daemon", "continuo"]),
    { same: true, arrived: [], left: [] });
});

test("a program arriving is a change", () => {
  assert.deepEqual(compare(["continuo", "es9-daemon", "sclang"], ["continuo", "es9-daemon", "sclang", "ableton"]),
    { same: false, arrived: ["ableton"], left: [] });
});

test("a program leaving is a change", () => {
  assert.deepEqual(compare(["continuo", "es9-daemon", "sclang"], ["continuo", "sclang"]),
    { same: false, arrived: [], left: ["es9-daemon"] });
});

test("a clean measurement has no problems", () => assert.deepEqual(problems(good), []));

for (const [name, change, want] of [
  ["the sweep said so itself", { usable: false, reason: "the residual varies across buffer sizes" }, "varies across buffer sizes"],
  ["no device", { device: "" }, "property of one"],
  ["not a round trip", { residual_samples: 0 }, "not a round trip"],
  ["the model does not fit", { slope_buffers: 1.4 }, "not the 2.00"],
  ["nothing to compare later", { clients: [] }, "still matches"],
]) {
  test(`refused: ${name}`, () => assert.match(problems({ ...good, ...change })[0], new RegExp(want)));
}

test("the engine's copy has the lines the engine reads, in whole samples", () => {
  const text = confText(good, "1f713ed6");
  for (const line of ["device           = AUDIO4c", "residual_samples = 275\n", "clients          = continuo,sclang", "amphora          = 1f713ed6"]) {
    assert.ok(text.includes(line), `missing: ${line}`);
  }
});

test("programs are found by their command lines", () => {
  assert.deepEqual(clientsIn("/Applications/Ableton Live 12 Suite.app/Contents/MacOS/Live\n./target/release/itajara loop\nvim"), ["ableton", "itajara"]);
});

test("a record is kept, read back, and checked", { skip: !process.env.AMPHORA_URL }, async () => {
  const device = "TEST-" + Date.now();
  await assert.rejects(keep({ ...good, device, clients: [] }), /still matches/);
  assert.equal(await recordFor(device), null);
  const hash = await keep({ ...good, device });
  assert.equal(await keep({ ...good, device }), hash, "the same measurement is the same content");
  const record = await recordFor(device);
  assert.equal(record.residual_samples, 275);
  assert.deepEqual(record.points, good.points, "the readings are kept whole, not as empty objects");
  const result = await check(device);
  assert.equal(result.measured, true);
  assert.deepEqual(result.clientsMeasured, ["continuo", "sclang"]);
});
