// json-diff.mjs — structural differ for captured fixture directories.
//
// Usage: node json-diff.mjs <oldDir> <newDir>
//   * for every `*.frames.jsonl` in oldDir: parse both files as JSONL.
//     - equal length  -> recursive walk, printing `[i] <path>: <old> -> <new>`
//       (leaf values truncated to 160 chars),
//     - unequal length-> print the first differing index with both frames
//       truncated.
//   * also compares `*.argv.json` and `initialize.json` as whole objects.
// Always exits 0 (stdout is the report; the caller, e.g. upstream-divergence.sh,
// treats differences as informational).

import fs from "node:fs";
import path from "node:path";

const truncate = (v, n = 160) => {
  const s = JSON.stringify(v);
  if (s === undefined) return "undefined";
  return s.length > n ? s.slice(0, n) + "…" : s;
};

// Recursively walk two JSON values, printing every leaf that differs, prefixed
// with `<i> <path>:`.
function walkDiff(a, b, prefix, i, emit) {
  if (JSON.stringify(a) === JSON.stringify(b)) return;
  if (typeof a !== "object" || a === null || typeof b !== "object" || b === null) {
    emit(`${prefix}: ${truncate(a)} -> ${truncate(b)}`);
    return;
  }
  const aKeys = new Set(Object.keys(a));
  const bKeys = new Set(Object.keys(b));
  for (const k of new Set([...aKeys, ...bKeys])) {
    walkDiff(a[k], b[k], prefix ? `${prefix}.${k}` : k, i, emit);
  }
}

function diffFramesJsonl(oldPath, newPath, name) {
  const readLines = (p) =>
    fs.readFileSync(p, "utf8").split("\n").filter((l) => l.trim() !== "");
  const oldLines = readLines(oldPath);
  const newLines = readLines(newPath);
  const parsedOld = oldLines.map((l) => JSON.parse(l));
  const parsedNew = newLines.map((l) => JSON.parse(l));

  console.log(`## ${name}`);
  if (oldLines.length !== newLines.length) {
    const n = Math.min(oldLines.length, newLines.length);
    let idx = -1;
    for (let k = 0; k < n; k++) {
      if (JSON.stringify(parsedOld[k]) !== JSON.stringify(parsedNew[k])) { idx = k; break; }
    }
    if (idx === -1) idx = n; // first beyond the shorter side
    const emit = (line) => console.log(`  ${line}`);
    emit(`[${idx}] ${truncate(parsedOld[idx])}`);
    emit(`[${idx}] ${truncate(parsedNew[idx])}`);
    emit(`(length ${oldLines.length} -> ${newLines.length})`);
    return;
  }
  for (let i = 0; i < parsedOld.length; i++) {
    const emit = (line) => console.log(`  [${i}] ${line}`);
    walkDiff(parsedOld[i], parsedNew[i], "", i, emit);
  }
}

function diffObject(oldPath, newPath, name) {
  const a = JSON.parse(fs.readFileSync(oldPath, "utf8"));
  const b = JSON.parse(fs.readFileSync(newPath, "utf8"));
  console.log(`## ${name}`);
  const emit = (line) => console.log(`  ${line}`);
  walkDiff(a, b, "", null, emit);
}

const [oldDir, newDir] = process.argv.slice(2);
if (!oldDir || !newDir) {
  console.error("usage: node json-diff.mjs <oldDir> <newDir>");
  process.exit(1);
}

const oldFiles = fs.readdirSync(oldDir).sort();
for (const f of oldFiles) {
  if (f.endsWith(".frames.jsonl")) {
    const oldPath = path.join(oldDir, f);
    const newPath = path.join(newDir, f);
    if (!fs.existsSync(newPath)) {
      console.log(`## ${f}`);
      console.log("  (new dir has no such fixture)");
      continue;
    }
    diffFramesJsonl(oldPath, newPath, f);
  } else if (f.endsWith(".argv.json")) {
    const oldPath = path.join(oldDir, f);
    const newPath = path.join(newDir, f);
    if (!fs.existsSync(newPath)) {
      console.log(`## ${f}`);
      console.log("  (new dir has no such fixture)");
      continue;
    }
    diffObject(oldPath, newPath, f);
  }
}

// initialize.json (one per dir) compared as a whole object.
if (fs.existsSync(path.join(oldDir, "initialize.json"))) {
  const newInit = path.join(newDir, "initialize.json");
  if (fs.existsSync(newInit)) {
    diffObject(path.join(oldDir, "initialize.json"), newInit, "initialize.json");
  }
}
