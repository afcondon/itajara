// The judgement in volts.mjs, held without an interface: `send` and `read`
// are a pretend output and pretend inputs.
//
//   node --test scripts/volts.test.mjs

import assert from "node:assert/strict";
import { test } from "node:test";
import { fit, problems, sweep } from "./volts.mjs";

// Four inputs. The output is patched to `patched` through `through` (level in,
// level out); the others sit at a small offset. `decay` makes the patched
// input let go of a held voltage, as an AC-coupled one does.
const rig = ({ patched = 2, through = (level) => 0.99 * level - 0.002, decay = 1 } = {}) => {
  let now = 0, held = 0;
  return {
    send: async (level) => { now = level; held = 0; },
    read: async (skip, take) => Array.from({ length: take }, () => {
      held += 1 + skip / take;
      return [0.01, -0.02, 0, 0.003].map((idle, channel) =>
        channel === patched ? Math.max(-1, Math.min(1, through(now) * decay ** held)) : idle);
    }),
  };
};

test("a cable gives the line it is", async () => {
  const found = await sweep({ ...rig() });
  assert.equal(found.in_ch, 2);
  assert.ok(Math.abs(found.gain - 0.99) < 1e-9);
  assert.ok(Math.abs(found.offset_volts + 0.02) < 1e-9);
  assert.ok(found.worst_volts < 1e-9);
  assert.equal(found.points.length, 17);
  assert.deepEqual(problems(found), []);
});

test("the output is left at zero, whatever happened", async () => {
  const sent = [];
  const pretend = rig({ patched: -1 });
  await assert.rejects(sweep({ ...pretend, send: async (level) => { sent.push(level); } }), /no input moved/);
  assert.equal(sent.at(-1), 0);
});

test("a chosen input that stays still names the one that moved", async () => {
  await assert.rejects(sweep({ ...rig(), input: 0 }), (error) => error.elsewhere === 2);
});

test("an input that lets go of a held voltage is AC-coupled", async () => {
  await assert.rejects(sweep({ ...rig({ decay: 0.9 }) }), /AC-coupled/);
});

test("a socket is taken before the interface's own mix of the same output", async () => {
  const pretend = rig({ patched: 1 });
  const read = async (skip, take) => (await pretend.read(skip, take)).map((line, index) => [...line, 0]);
  let now = 0;
  const found = await sweep({
    send: async (level) => { now = level; await pretend.send(level); },
    read: async (skip, take) => (await read(skip, take)).map((line) => [...line.slice(0, 4), now]),
  });
  assert.equal(found.in_ch, 1);
});

test("steps beyond the input's range are left out of the line", async () => {
  const found = await sweep({ ...rig({ through: (level) => 1.2 * level }) });
  assert.ok(found.points.some((point) => point.clipped));
  assert.ok(Math.abs(found.gain - 1.2) < 1e-9);
});

test("an attenuator, an inverter and a bend are each refused", () => {
  const line = (through) => fit([-1, -0.5, 0, 0.5, 1].map((sent) => ({ sent, measured: through(sent) })));
  assert.match(problems(line((level) => 0.3 * level))[0], /not a cable/);
  assert.match(problems(line((level) => -level))[0], /not a cable/);
  assert.match(problems(line((level) => level - 0.05 * level * level))[0], /not on a line/);
  assert.match(problems({ ...line((level) => 0.99 * level), fitted: 3 })[0], /wants five/);
  assert.match(problems({ ...line((level) => level), fitted: 5 })[0], /no socket is/);
});
