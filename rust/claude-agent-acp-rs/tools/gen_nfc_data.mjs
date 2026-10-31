#!/usr/bin/env node
// Regenerate `src/session_index/nfc_data.rs`.
//
// Node 26's `String.prototype.normalize` is the oracle: it is the runtime the
// adapter ships against (upstream `normalizePath` calls it directly), so its
// Unicode version is the one the tables must match. The data comes from the
// matching UCD release:
//
//   curl -O https://www.unicode.org/Public/17.0.0/ucd/UnicodeData.txt
//   curl -O https://www.unicode.org/Public/17.0.0/ucd/extracted/DerivedCombiningClass.txt
//   curl -O https://www.unicode.org/Public/17.0.0/ucd/CompositionExclusions.txt
//
// Usage:
//   node tools/gen_nfc_data.mjs --ucd <dir>            # rewrite the file
//   node tools/gen_nfc_data.mjs --ucd <dir> --check    # fail if it differs
//
// `--ucd` defaults to `tools/ucd`. The generated file is checked into the repo;
// this script is a maintenance tool and is not part of the build.
//
// The script verifies its own output against Node before writing: every code
// point's NFD, every composition pair's NFC, and seeded random strings through
// a local UAX #15 NFC implementation built from the generated tables.

import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const crate = join(here, "..");
const outPath = join(crate, "src", "session_index", "nfc_data.rs");

const args = process.argv.slice(2);
let ucdDir = join(here, "ucd");
let check = false;
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--ucd") ucdDir = args[++i];
  else if (args[i] === "--check") check = true;
  else {
    console.error(`unknown argument: ${args[i]}`);
    process.exit(2);
  }
}

function read(name) {
  const path = join(ucdDir, name);
  if (!existsSync(path)) {
    console.error(`missing ${path}; see the header of this script for the URLs`);
    process.exit(2);
  }
  return readFileSync(path, "utf8");
}

const hex = (cp) => `0x${cp.toString(16).padStart(4, "0")}`;

// ---- UnicodeData.txt: canonical decompositions ------------------------------
const primary = new Map(); // cp -> [parts] (single-step canonical decomposition)
const ccc = new Map(); // cp -> combining class (non-zero only)

for (const line of read("UnicodeData.txt").split("\n")) {
  if (!line) continue;
  const f = line.split(";");
  const cp = parseInt(f[0], 16);
  const dec = f[5];
  if (dec && !dec.startsWith("<")) {
    primary.set(cp, dec.split(" ").map((x) => parseInt(x, 16)));
  }
  const klass = parseInt(f[3], 10);
  if (klass) ccc.set(cp, klass);
}

// DerivedCombiningClass.txt supersedes UnicodeData field 3 (it lists every
// non-zero class, including the ranges UnicodeData splits into single lines).
for (const line of read("DerivedCombiningClass.txt").split("\n")) {
  const text = line.split("#")[0].trim();
  if (!text) continue;
  const m = text.match(/^([0-9A-F]+)(?:\.\.([0-9A-F]+))?\s*;\s*(\d+)\s*$/);
  if (!m) continue;
  const a = parseInt(m[1], 16);
  const b = parseInt(m[2] ?? m[1], 16);
  const klass = parseInt(m[3], 10);
  if (!klass) continue;
  for (let cp = a; cp <= b; cp++) ccc.set(cp, klass);
}

// ---- Full canonical decomposition (NFD), Hangul excluded --------------------
function fullDecomposition(cp, out = []) {
  if (cp >= 0xac00 && cp <= 0xd7a3) {
    // Algorithmic; absent from the table.
    out.push(cp);
    return out;
  }
  const parts = primary.get(cp);
  if (!parts) {
    out.push(cp);
    return out;
  }
  for (const part of parts) fullDecomposition(part, out);
  return out;
}

const decompositions = [];
for (let cp = 0; cp <= 0x10ffff; cp++) {
  if (cp >= 0xd800 && cp <= 0xdfff) continue;
  if (cp >= 0xac00 && cp <= 0xd7a3) continue;
  if (!primary.has(cp)) continue;
  decompositions.push([cp, fullDecomposition(cp)]);
}

// ---- Composition pairs ------------------------------------------------------
const excluded = new Set();
for (const line of read("CompositionExclusions.txt").split("\n")) {
  const text = line.split("#")[0].trim();
  if (!text) continue;
  const m = text.match(/^([0-9A-F]+)(?:\.\.([0-9A-F]+))?/);
  if (!m) continue;
  const a = parseInt(m[1], 16);
  const b = parseInt(m[2] ?? m[1], 16);
  for (let cp = a; cp <= b; cp++) excluded.add(cp);
}

const compositions = [];
for (const [cp, parts] of primary) {
  if (parts.length !== 2 || excluded.has(cp)) continue;
  // A decomposition whose first part is not a starter never composes.
  if (ccc.get(parts[0]) ?? 0) continue;
  compositions.push([parts[0], parts[1], cp]);
}
compositions.sort((x, y) => x[0] - y[0] || x[1] - y[1] || x[2] - y[2]);

// ---- Combining-class ranges -------------------------------------------------
const classEntries = [...ccc.entries()].sort((a, b) => a[0] - b[0]);
const classes = [];
for (const [cp, klass] of classEntries) {
  const last = classes[classes.length - 1];
  if (last && last[2] === klass && last[1] + 1 === cp) last[1] = cp;
  else classes.push([cp, cp, klass]);
}

// ---- Oracle verification ----------------------------------------------------
// NFD is Node's directly.
for (let cp = 0; cp <= 0x10ffff; cp++) {
  if (cp >= 0xd800 && cp <= 0xdfff) continue;
  // Hangul decomposes algorithmically; the table omits it.
  if (cp >= 0xac00 && cp <= 0xd7a3) continue;
  const s = String.fromCodePoint(cp);
  const nfd = [...s.normalize("NFD")].map((c) => c.codePointAt(0));
  const table = decompositions.find(([k]) => k === cp);
  const expected = table ? table[1] : [cp];
  if (JSON.stringify(nfd) !== JSON.stringify(expected)) {
    console.error(`NFD mismatch U+${cp.toString(16)}: node=${nfd} table=${expected}`);
    process.exit(1);
  }
}
for (const [a, b, c] of compositions) {
  const composed = String.fromCodePoint(a, b).normalize("NFC");
  if (composed !== String.fromCodePoint(c)) {
    console.error(`composition mismatch ${hex(a)}+${hex(b)} -> ${hex(c)}`);
    process.exit(1);
  }
}

// A local NFC over the generated tables, compared with Node on seeded random
// strings: this is what validates the combining classes (ordering and the
// composition-blocking rule).
const classLookup = (cp) => {
  let lo = 0;
  let hi = classes.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const [a, b, k] = classes[mid];
    if (cp < a) hi = mid - 1;
    else if (cp > b) lo = mid + 1;
    else return k;
  }
  return 0;
};
const decompLookup = new Map(decompositions);
const composeLookup = new Map(compositions.map(([a, b, c]) => [`${a},${b}`, c]));

function localNfc(text) {
  const d = [];
  for (const ch of text) {
    const cp = ch.codePointAt(0);
    if (cp >= 0xac00 && cp <= 0xd7a3) {
      const index = cp - 0xac00;
      const l = 0x1100 + Math.floor(index / 588);
      const v = 0x1161 + Math.floor((index % 588) / 28);
      const t = index % 28;
      d.push([l, 0], [v, 0]);
      if (t) d.push([0x11a7 + t, 0]);
    } else if (decompLookup.has(cp)) {
      for (const part of decompLookup.get(cp)) d.push([part, classLookup(part)]);
    } else {
      d.push([cp, classLookup(cp)]);
    }
  }
  // Canonical ordering.
  for (let i = 0; i < d.length; ) {
    if (d[i][1] === 0) {
      i++;
      continue;
    }
    let start = i;
    while (i < d.length && d[i][1] !== 0) i++;
    const run = d.slice(start, i);
    run.sort((x, y) => x[1] - y[1]);
    d.splice(start, run.length, ...run);
  }
  // Canonical composition.
  const out = [];
  let starter = -1;
  let lastClass = 0;
  for (const [cp, klass] of d) {
    if (starter >= 0 && (lastClass === 0 || lastClass < klass)) {
      const first = out[starter];
      let composed = composeLookup.get(`${first},${cp}`);
      // Hangul composes algorithmically (absent from the table).
      if (composed === undefined) {
        if (first >= 0x1100 && first <= 0x1112 && cp >= 0x1161 && cp <= 0x1175) {
          composed = 0xac00 + ((first - 0x1100) * 21 + (cp - 0x1161)) * 28;
        } else if (
          first >= 0xac00 &&
          first <= 0xd7a3 &&
          (first - 0xac00) % 28 === 0 &&
          cp > 0x11a7 &&
          cp <= 0x11c2
        ) {
          composed = first + (cp - 0x11a7);
        }
      }
      if (composed !== undefined) {
        out[starter] = composed;
        continue;
      }
    }
    if (klass === 0) starter = out.length;
    lastClass = klass;
    out.push(cp);
  }
  return String.fromCodePoint(...out);
}

let seed = 0x9e3779b97f4a7c15n;
const next = () => {
  seed ^= seed << 13n;
  seed ^= seed >> 7n;
  seed ^= seed << 17n;
  seed &= 0xffffffffffffffffn;
  return Number(seed & 0x7fffffffn);
};
const starters = [0x61, 0x65, 0x41, 0x45, 0x3b1, 0x3c9, 0x627, 0x915, 0x1100, 0x1161, 0x4e00];
const marks = [...ccc.keys()];
for (let i = 0; i < 200_000; i++) {
  const cps = [];
  for (let n = 1 + (next() % 6); n > 0; n--) {
    cps.push(next() % 4 === 0 ? starters[next() % starters.length] : marks[next() % marks.length]);
  }
  const s = String.fromCodePoint(...cps);
  const want = s.normalize("NFC");
  const got = localNfc(s);
  if (got !== want) {
    console.error(
      `NFC mismatch for ${cps.map((c) => hex(c)).join(" ")}: node=${[...want].map((c) => hex(c.codePointAt(0)))} local=${[...got].map((c) => hex(c.codePointAt(0)))}`,
    );
    process.exit(1);
  }
}

// ---- Emit -------------------------------------------------------------------
const lines = [];
lines.push("//! Canonical decomposition, composition and combining-class tables for NFC.");
lines.push("//!");
lines.push("//! Generated data, not hand-written, by `tools/gen_nfc_data.mjs` from the");
lines.push("//! Unicode 17.0.0 UCD (`UnicodeData.txt`, `DerivedCombiningClass.txt`,");
lines.push("//! `CompositionExclusions.txt`; https://www.unicode.org/Public/17.0.0/ucd/).");
lines.push("//! Node 26's `String.prototype.normalize` is the oracle: the script checks");
lines.push("//! every code point's NFD, every composition pair's NFC and seeded random");
lines.push("//! strings against it. Hangul syllables compose and decompose");
lines.push("//! algorithmically and are absent here.");
lines.push("");
lines.push("/// Full canonical decomposition of a code point, as `NFD` reports it.");
lines.push("pub const DECOMPOSITIONS: &[(u32, &[u32])] = &[");
for (const [cp, parts] of decompositions) {
  lines.push(`    (${hex(cp)}, &[${parts.map(hex).join(", ")}]),`);
}
lines.push("];");
lines.push("");
lines.push("/// `(starter, combining mark)` pairs that compose to the third code point.");
lines.push("pub const COMPOSITIONS: &[(u32, u32, u32)] = &[");
for (const [a, b, c] of compositions) {
  lines.push(`    (${hex(a)}, ${hex(b)}, ${hex(c)}),`);
}
lines.push("];");
lines.push("");
lines.push("/// Ranges of code points that share a non-zero canonical combining class.");
lines.push("pub const COMBINING_CLASSES: &[(u32, u32, u8)] = &[");
for (const [a, b, k] of classes) {
  lines.push(`    (${hex(a)}, ${hex(b)}, ${k}),`);
}
lines.push("];");
lines.push("");

const generated = lines.join("\n");
const current = existsSync(outPath) ? readFileSync(outPath, "utf8") : "";
if (check) {
  if (generated !== current) {
    console.error(`${outPath} is stale; run node tools/gen_nfc_data.mjs`);
    process.exit(1);
  }
  console.log(`${outPath} is up to date (Unicode 17.0.0, ${decompositions.length} decompositions, ${compositions.length} compositions, ${classes.length} class ranges)`);
} else {
  writeFileSync(outPath, generated);
  console.log(`wrote ${outPath} (Unicode 17.0.0, ${decompositions.length} decompositions, ${compositions.length} compositions, ${classes.length} class ranges)`);
}
