import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const [base] = process.argv.slice(2);
if (!base) {
  console.error("usage: check_changed_rustfmt.mjs <base-sha>");
  process.exit(2);
}

function run(command, args) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  return result;
}

function countOf(raw) {
  return raw === undefined ? 1 : Number.parseInt(raw, 10);
}

function range(startRaw, countRaw) {
  const start = Math.max(1, Number.parseInt(startRaw, 10));
  const count = countOf(countRaw);
  return count === 0 ? { start, end: start } : { start, end: start + count - 1 };
}

function changedRanges(patch) {
  const ranges = [];
  for (const line of patch.split("\n")) {
    const match = line.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/);
    if (match) ranges.push(range(match[1], match[2]));
  }
  return ranges;
}

function formattingHunks(patch) {
  const hunks = [];
  let current = null;
  for (const line of patch.split("\n")) {
    const match = line.match(/^@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@/);
    if (match) {
      if (current) hunks.push(current);
      current = { range: range(match[1], match[2]), lines: [line] };
    } else if (current) {
      current.lines.push(line);
    }
  }
  if (current) hunks.push(current);
  return hunks;
}

function intersects(left, right) {
  return left.start <= right.end && right.start <= left.end;
}

// 按文件所属 crate 的 edition 跑 rustfmt:主工作区是 2024,relay-server 还是 2021,
// 两种 edition 的格式口径不同(`use` 里的排序等)。沿目录向上找最近的 Cargo.toml,
// 取它的 `edition = "…"`;写成 `edition.workspace = true` 或没写的按工作区的 2024。
const editionCache = new Map();
function editionOf(file) {
  let dir = path.dirname(path.resolve(file));
  const root = path.resolve(".");
  while (true) {
    if (editionCache.has(dir)) return editionCache.get(dir);
    const manifest = path.join(dir, "Cargo.toml");
    if (fs.existsSync(manifest)) {
      const text = fs.readFileSync(manifest, "utf8");
      const match = text.match(/^\s*edition\s*=\s*"(\d{4})"/m);
      const edition = match ? match[1] : "2024";
      editionCache.set(dir, edition);
      return edition;
    }
    const parent = path.dirname(dir);
    if (parent === dir || !dir.startsWith(root)) return "2024";
    dir = parent;
  }
}

const changed = run("git", [
  "diff",
  "--name-only",
  "--diff-filter=ACMR",
  "-z",
  base,
  "HEAD",
  "--",
  "*.rs",
]);
if (changed.status !== 0) {
  process.stderr.write(changed.stderr);
  process.exit(changed.status ?? 1);
}

const files = changed.stdout.split("\0").filter(Boolean);
const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "mini-term-rustfmt-"));
const relevant = [];
let ignoredBaseline = 0;

try {
  for (const [index, file] of files.entries()) {
    const sourceDiff = run("git", [
      "diff",
      "--unified=0",
      "--no-color",
      base,
      "HEAD",
      "--",
      file,
    ]);
    if (sourceDiff.status !== 0) {
      process.stderr.write(sourceDiff.stderr);
      process.exit(sourceDiff.status ?? 1);
    }
    const sourceRanges = changedRanges(sourceDiff.stdout);
    if (sourceRanges.length === 0) continue;

    const original = fs.readFileSync(file, "utf8");
    const formattedPath = path.join(tempDir, `${index}.rs`);
    fs.writeFileSync(formattedPath, original);
    const formatted = run("rustfmt", [
      "--edition",
      editionOf(file),
      "--config",
      "skip_children=true",
      formattedPath,
    ]);
    if (formatted.status !== 0) {
      process.stderr.write(formatted.stdout);
      process.stderr.write(formatted.stderr);
      process.exit(formatted.status ?? 1);
    }
    const formattedText = fs.readFileSync(formattedPath, "utf8");
    if (formattedText === original) continue;

    const diff = run("diff", [
      "-U0",
      "--label",
      file,
      "--label",
      file,
      file,
      formattedPath,
    ]);
    if (diff.status !== 0 && diff.status !== 1) {
      process.stderr.write(diff.stderr);
      process.exit(diff.status ?? 1);
    }

    for (const hunk of formattingHunks(diff.stdout)) {
      if (sourceRanges.some((sourceRange) => intersects(sourceRange, hunk.range))) {
        relevant.push({ file, text: hunk.lines.join("\n") });
      } else {
        ignoredBaseline += 1;
      }
    }
  }
} finally {
  fs.rmSync(tempDir, { recursive: true, force: true });
}

console.log(
  `rustfmt baseline ignored outside changed lines: ${ignoredBaseline}; changed-line formatting hunks: ${relevant.length}`,
);
if (relevant.length > 0) {
  for (const hunk of relevant) {
    console.error(`Formatting differs in ${hunk.file}:\n${hunk.text}`);
  }
  process.exit(1);
}
