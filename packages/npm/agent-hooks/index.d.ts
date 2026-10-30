import type { SpawnOptions } from "node:child_process";

export interface InitOptions {
  /** Harness id or `all` (default). */
  harness?: string;
  /** Set false to only place the binary and leave harness configs alone. */
  hooks?: boolean;
  /** Fail unless the bundled binary is at least this X.Y.Z version. */
  minVersion?: string;
  /** Daemon socket path to embed in installed hook commands. */
  socket?: string;
}

export interface HarnessResult {
  id: string;
  status: "installed" | "current" | "skipped" | "failed";
  error?: string;
}

export interface InitResult {
  /** Stable path of the installed binary; use this for everything. */
  path: string;
  action: "installed" | "upgraded" | "kept" | "current";
  version: string;
  /** Per-harness hook registration; empty when hooks were skipped or no harness was detected. */
  harnesses: HarnessResult[];
  /** Whether the install dir is on the user's PATH. */
  on_path: boolean;
}

/** Install the binary at ~/.agent-hooks/bin and register hooks. Idempotent. */
export function init(options?: InitOptions): Promise<InitResult>;
/** Absolute path of the bundled binary (or `AGENT_HOOKS_BIN`). */
export function binaryPath(): string;
/** Run the bundled binary with args; resolves with its exit code. */
export function run(args: string[], options?: SpawnOptions): Promise<number>;
