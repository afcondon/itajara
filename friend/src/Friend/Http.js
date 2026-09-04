// The page's four calls to its own server, and the shaping of notes between
// what the page holds (flat strings, a row per loop) and what msm reads
// (numbers where they are numbers, loops keyed by number). The wire shape
// belongs here so the PureScript can hold what a form holds.

const j = (r) => {
  if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
  return r.json();
};

const toWire = (n) => ({
  title: n.title,
  key: n.key,
  bpm: n.bpm.trim() === "" || isNaN(Number(n.bpm)) ? null : Number(n.bpm),
  timbre: n.timbre,
  uses: n.uses,
  notes: n.notes,
  tags: n.tags.split(",").map((t) => t.trim()).filter((t) => t !== ""),
  loops: Object.fromEntries(
    n.loops
      .filter((l) => l.title || l.key || l.timbre || l.uses || l.notes)
      .map((l) => [String(l.loop), { title: l.title, key: l.key, timbre: l.timbre, uses: l.uses, notes: l.notes }])
  ),
});

const s = (v) => (v == null ? "" : String(v));

const fromWire = (w) => ({
  title: s(w.title),
  key: s(w.key),
  bpm: w.bpm == null ? "" : String(w.bpm),
  timbre: s(w.timbre),
  uses: s(w.uses),
  notes: s(w.notes),
  tags: Array.isArray(w.tags) ? w.tags.join(", ") : s(w.tags),
  loops: Object.entries(w.loops || {}).map(([k, l]) => ({
    loop: Number(k),
    title: s(l.title),
    key: s(l.key),
    timbre: s(l.timbre),
    uses: s(l.uses),
    notes: s(l.notes),
  })),
});

export const loadNotesImpl = (take) => () =>
  fetch(`/api/takes/${encodeURIComponent(take)}/notes`).then(j).then(fromWire);

export const saveNotesImpl = (take) => (notes) => () =>
  fetch(`/api/takes/${encodeURIComponent(take)}/notes`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(toWire(notes)),
  }).then(j).then(() => {});

export const listSticksImpl = () => fetch("/api/sticks").then(j);

export const listTakesImpl = () => fetch("/api/takes").then(j).then((ts) => ts.map((t) => t.name));

export const harvestImpl = (req) => () =>
  fetch("/api/harvest", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(req),
  }).then(j);
