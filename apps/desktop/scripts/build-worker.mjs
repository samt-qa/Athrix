#!/usr/bin/env node
// Builds the Tauri sidecars and stages them in src-tauri/binaries:
//
//   src-tauri/binaries/athrix-extract-worker-<target-triple>[.exe]  (isolated PDF extraction)
//   src-tauri/binaries/athrix-mcp-<target-triple>[.exe]             (MCP server for AI clients)
//
// `bundle.externalBin` in tauri.conf.json lists it, so:
// - tauri-build (the app's build script) copies it next to the app executable in
//   target/<profile>/ on every cargo build of athrix-desktop. That is where
//   `athrix_extract::worker::default_program()` looks, in dev and in tests. NOTE: tauri-build
//   fails when the file is missing, so run this script once after a fresh checkout
//   (`npm run worker`) before `cargo build -p athrix-desktop`.
// - `tauri build` installs it next to Athrix.exe (without the triple suffix).
//
// Why separate cargo invocations and target dir: cargo features are per package, so building
// the worker with `--features pdf` in the same invocation as the app (or as athrix-mcp, which
// must not parse PDFs in-process either) would turn PDF parsing on there too (feature
// unification). So each sidecar gets its own cargo invocation. A separate target dir (target/worker) keeps
// the two feature sets from invalidating each other's artifacts and avoids waiting on the main
// target dir's lock while `tauri dev` / an IDE builds the app.
//
// Usage: node scripts/build-worker.mjs [--release | --debug] [--target <triple>]
// Without a flag the profile follows Tauri's TAURI_ENV_DEBUG (set for before*Command hooks):
// debug for `tauri dev` / `tauri build --debug`, release for `tauri build`; outside Tauri the
// default is debug. The triple follows --target, else TAURI_ENV_TARGET_TRIPLE, else the host
// triple from `rustc -vV`.

import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, renameSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Each sidecar: its binary name and the cargo arguments that build it.
const SIDECARS = [
  { bin: "athrix-extract-worker", args: ["-p", "athrix-extract", "--bin", "athrix-extract-worker", "--features", "pdf"] },
  { bin: "athrix-mcp", args: ["-p", "athrix-mcp", "--bin", "athrix-mcp"] },
];
const here = dirname(fileURLToPath(import.meta.url));
const appDir = resolve(here, "..");
const tauriDir = join(appDir, "src-tauri");
const workspace = resolve(appDir, "..", "..");
const targetDir = join(workspace, "target", "worker");

function fail(message) {
  console.error(`[build-worker] ${message}`);
  process.exit(1);
}

function parseArgs(argv) {
  const opts = { release: undefined, target: undefined };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--release") opts.release = true;
    else if (a === "--debug") opts.release = false;
    else if (a === "--target") opts.target = argv[++i];
    else if (a.startsWith("--target=")) opts.target = a.slice("--target=".length);
    else fail(`unknown argument ${a}`);
  }
  if (opts.release === undefined) {
    const debug = process.env.TAURI_ENV_DEBUG;
    opts.release = debug !== undefined && debug !== "true";
  }
  return opts;
}

function hostTriple() {
  const out = spawnSync("rustc", ["-vV"], { encoding: "utf8" });
  if (out.status !== 0) fail(`rustc -vV failed: ${out.stderr || out.error}`);
  const line = out.stdout.split(/\r?\n/).find((l) => l.startsWith("host:"));
  if (!line) fail("no host line in rustc -vV");
  return line.slice("host:".length).trim();
}

function sameFile(a, b) {
  if (!existsSync(b)) return false;
  if (statSync(a).size !== statSync(b).size) return false;
  return readFileSync(a).equals(readFileSync(b));
}

const opts = parseArgs(process.argv.slice(2));
const host = hostTriple();
const triple = opts.target || process.env.TAURI_ENV_TARGET_TRIPLE || host;
const exe = triple.includes("windows") ? ".exe" : "";
const profile = opts.release ? "release" : "debug";

// Only pass --target when cross-compiling: it changes the output path.
const cross = triple !== host;
const binaries = join(tauriDir, "binaries");
mkdirSync(binaries, { recursive: true });

for (const { bin, args: buildArgs } of SIDECARS) {
  const args = ["build", ...buildArgs, "--target-dir", targetDir];
  if (opts.release) args.push("--release");
  if (cross) args.push("--target", triple);

  console.error(`[build-worker] cargo ${args.join(" ")}`);
  const built = spawnSync("cargo", args, { cwd: workspace, stdio: "inherit" });
  if (built.error) fail(`cannot run cargo: ${built.error.message}`);
  if (built.status !== 0) fail(`cargo exited with ${built.status}`);

  const source = join(targetDir, ...(cross ? [triple] : []), profile, `${bin}${exe}`);
  if (!existsSync(source)) fail(`built ${bin} not found at ${source}`);

  const dest = join(binaries, `${bin}-${triple}${exe}`);
  // Unchanged binary: leave it (and its mtime) alone, so tauri-build does not rerun for nothing.
  if (sameFile(source, dest)) {
    console.error(`[build-worker] up to date: ${dest}`);
  } else {
    // Copy then rename: tauri-build never sees a half-written file.
    const tmp = `${dest}.tmp`;
    copyFileSync(source, tmp);
    renameSync(tmp, dest);
    console.error(`[build-worker] ${profile} ${bin} -> ${dest}`);
  }
}
