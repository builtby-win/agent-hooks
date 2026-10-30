#!/usr/bin/env node
// Builds publishable npm packages from the release assets.
//   node scripts/npm-pack.mjs <version> <assets-dir> <out-dir>
// Writes <out-dir>/agent-hooks (the wrapper) and one platform package per
// target, each containing bin/agent-hooks[.exe] extracted from the matching
// release archive. Publish every directory with `npm publish --access public`.
import { execFileSync } from "node:child_process";
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const [version, assetsArg, outArg] = process.argv.slice(2);
if (!version || !assetsArg || !outArg) {
  console.error("usage: npm-pack.mjs <version> <assets-dir> <out-dir>");
  process.exit(2);
}
const assets = resolve(assetsArg);
const out = resolve(outArg);
const base = "builtby.win-agent-hooks";
const root = resolve(import.meta.dirname, "..");

const targets = [
  { key: "darwin-arm64", os: "darwin", cpu: "arm64", triple: "aarch64-apple-darwin" },
  { key: "darwin-x64", os: "darwin", cpu: "x64", triple: "x86_64-apple-darwin" },
  { key: "linux-arm64", os: "linux", cpu: "arm64", triple: "aarch64-unknown-linux-musl" },
  { key: "linux-x64", os: "linux", cpu: "x64", triple: "x86_64-unknown-linux-musl" },
  { key: "win32-arm64", os: "win32", cpu: "arm64", triple: "aarch64-pc-windows-msvc" },
  { key: "win32-x64", os: "win32", cpu: "x64", triple: "x86_64-pc-windows-msvc" },
];

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });

for (const t of targets) {
  const win = t.os === "win32";
  const archive = join(assets, `agent-hooks-${t.triple}.${win ? "zip" : "tar.gz"}`);
  if (!existsSync(archive)) throw new Error(`missing release asset: ${archive}`);
  const scratch = mkdtempSync(join(tmpdir(), "ah-npm-"));
  execFileSync("tar", ["-xf", archive, "-C", scratch]); // bsdtar/GNU tar both read zip+tgz on CI runners
  const exe = win ? "agent-hooks.exe" : "agent-hooks";
  const dir = join(out, `agent-hooks-${t.key}`);
  mkdirSync(join(dir, "bin"), { recursive: true });
  cpSync(join(scratch, `agent-hooks-${t.triple}`, exe), join(dir, "bin", exe));
  if (!win) chmodSync(join(dir, "bin", exe), 0o755);
  rmSync(scratch, { recursive: true, force: true });
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify(
      {
        name: `${base}-${t.key}`,
        version,
        description: `agent-hooks native binary for ${t.key}`,
        license: "MIT OR Apache-2.0",
        repository: "github:builtby-win/agent-hooks",
        os: [t.os],
        cpu: [t.cpu],
        files: ["bin"],
      },
      null,
      2,
    ) + "\n",
  );
}

const wrapperDir = join(out, "agent-hooks");
cpSync(join(root, "packages/npm/agent-hooks"), wrapperDir, { recursive: true });
const pkgPath = join(wrapperDir, "package.json");
const pkg = JSON.parse(readFileSync(pkgPath, "utf8"));
pkg.version = version;
for (const name of Object.keys(pkg.optionalDependencies)) pkg.optionalDependencies[name] = version;
writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");
console.log(`packed ${targets.length} platform packages + wrapper @ ${version} -> ${out}`);
