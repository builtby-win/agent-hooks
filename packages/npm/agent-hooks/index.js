"use strict";
// Resolves the platform binary that npm installed alongside this package
// (esbuild-style optionalDependencies), and drives `agent-hooks self-install`.

const { execFile, spawn } = require("node:child_process");
const { promisify } = require("node:util");

const execFileAsync = promisify(execFile);
const SCOPE = "builtby.win-agent-hooks";
const PLATFORMS = new Set([
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "win32-arm64",
  "win32-x64",
]);

/** Absolute path of the bundled binary, or `AGENT_HOOKS_BIN` if set. */
function binaryPath() {
  if (process.env.AGENT_HOOKS_BIN) return process.env.AGENT_HOOKS_BIN;
  const key = `${process.platform}-${process.arch}`;
  if (!PLATFORMS.has(key)) {
    throw new Error(`agent-hooks: unsupported platform ${key}`);
  }
  const exe = process.platform === "win32" ? "agent-hooks.exe" : "agent-hooks";
  let resolved;
  try {
    resolved = require.resolve(`${SCOPE}-${key}/bin/${exe}`);
  } catch {
    throw new Error(
      `agent-hooks: platform package ${SCOPE}-${key} is not installed. ` +
        "Reinstall without --omit=optional / --no-optional.",
    );
  }
  // Electron: executables cannot run from inside app.asar. Requires
  // `asarUnpack: ["**/node_modules/builtby.win-agent-hooks-*/**"]`.
  return resolved.replace(/app\.asar([\\/])/, "app.asar.unpacked$1");
}

/**
 * Install the binary at ~/.agent-hooks/bin (newest version wins) and register
 * hooks in detected harnesses. Idempotent: safe to call on every launch.
 *
 * @param {{harness?: string, hooks?: boolean, minVersion?: string, socket?: string}} [options]
 */
async function init(options = {}) {
  const args = ["self-install", "--json", "--harness", options.harness ?? "all"];
  if (options.hooks === false) args.push("--no-hooks");
  if (options.minVersion) args.push("--min-version", options.minVersion);
  if (options.socket) args.push("--socket", options.socket);
  let stdout;
  try {
    ({ stdout } = await execFileAsync(binaryPath(), args, { maxBuffer: 8 << 20 }));
  } catch (error) {
    const detail = error.stderr ? `: ${String(error.stderr).trim()}` : "";
    throw new Error(`agent-hooks self-install failed${detail}`, { cause: error });
  }
  // The JSON result is the final stdout line; the hook installer prints above it.
  const lines = stdout.trim().split("\n");
  return JSON.parse(lines[lines.length - 1]);
}

/** Run the bundled binary; resolves with its exit code. stdio is inherited by default. */
function run(args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(binaryPath(), args, { stdio: "inherit", ...options });
    child.on("error", reject);
    child.on("close", (code) => resolve(code ?? 1));
  });
}

module.exports = { init, binaryPath, run };
