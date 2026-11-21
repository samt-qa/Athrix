#!/usr/bin/env node
// `npm run tauri -- <args>`: the Tauri CLI, but `dev` and `build` first stage the extraction
// worker sidecar (scripts/build-worker.mjs), so they work even when `--config` replaces
// beforeDevCommand / beforeBuildCommand (which also stage it). `tauri build --debug` stages a
// debug worker; any other `build` a release one. `--target <triple>` is passed through.

import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const appDir = resolve(here, "..");
const args = process.argv.slice(2);
const command = args.find((a) => !a.startsWith("-"));

function run(script, scriptArgs) {
  const r = spawnSync(process.execPath, [script, ...scriptArgs], {
    cwd: appDir,
    stdio: "inherit",
  });
  if (r.error) {
    console.error(r.error.message);
    process.exit(1);
  }
  return r.status ?? 1;
}

if (command === "dev" || command === "build") {
  const workerArgs = [];
  const release = command === "build" && !args.includes("--debug") && !args.includes("-d");
  workerArgs.push(release ? "--release" : "--debug");
  const t = args.findIndex((a) => a === "--target" || a === "-t");
  if (t >= 0 && args[t + 1]) workerArgs.push("--target", args[t + 1]);
  const status = run(join(here, "build-worker.mjs"), workerArgs);
  if (status !== 0) process.exit(status);
}

process.exit(run(join(appDir, "node_modules", "@tauri-apps", "cli", "tauri.js"), args));
