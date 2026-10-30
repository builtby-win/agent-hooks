#!/usr/bin/env python3
"""Build platform wheels: pypi-pack.py <version> <assets-dir> <out-dir>."""
import base64
import hashlib
import io
import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PKG = ROOT / "packages" / "python"
NAME, DIST = "builtby-win-agent-hooks", "builtby_win_agent_hooks"
LINUX = "manylinux_2_17_{a}.manylinux2014_{a}.musllinux_1_1_{a}"
TRIPLES = {
    "aarch64-apple-darwin": "macosx_11_0_arm64",
    "x86_64-apple-darwin": "macosx_10_12_x86_64",
    "aarch64-unknown-linux-musl": LINUX.format(a="aarch64"),
    "x86_64-unknown-linux-musl": LINUX.format(a="x86_64"),
    "aarch64-pc-windows-msvc": "win_arm64",
    "x86_64-pc-windows-msvc": "win_amd64",
}


def extract_binary(assets: Path, triple: str) -> bytes:
    windows = "windows" in triple
    exe = "agent-hooks.exe" if windows else "agent-hooks"
    archive = assets / f"agent-hooks-{triple}{'.zip' if windows else '.tar.gz'}"
    if not archive.is_file():
        sys.exit(f"missing asset: {archive}")
    member = f"agent-hooks-{triple}/{exe}"
    if windows:
        with zipfile.ZipFile(archive) as z:
            return z.read(member)
    with tarfile.open(archive) as t:
        f = t.extractfile(member)
        if f is None:
            sys.exit(f"{member} is not a file in {archive}")
        return f.read()


def build(version: str, triple: str, plat: str, binary: bytes, out: Path) -> Path:
    windows = "windows" in triple
    exe = "agent-hooks.exe" if windows else "agent-hooks"
    di = f"{DIST}-{version}.dist-info"
    files = []  # (arcname, bytes, mode)
    src = PKG / "src" / DIST
    for p in sorted(src.rglob("*")):
        if p.is_file() and "__pycache__" not in p.parts and p.suffix != ".pyc":
            files.append((f"{DIST}/{p.relative_to(src).as_posix()}", p.read_bytes(), 0o644))
    files.append((f"{DIST}/_bin/{exe}", binary, 0o755))
    readme = (PKG / "README.md").read_text()
    meta = (
        "Metadata-Version: 2.4\n"
        f"Name: {NAME}\nVersion: {version}\n"
        "Summary: Install and run the agent-hooks CLI (bundled binary) from Python\n"
        "License-Expression: MIT OR Apache-2.0\n"
        "Project-URL: Homepage, https://github.com/builtby-win/agent-hooks\n"
        "Requires-Python: >=3.9\n"
        "Description-Content-Type: text/markdown\n\n" + readme
    )
    tags = [f"Tag: py3-none-{t}\n" for t in plat.split(".")]
    wheel = (
        "Wheel-Version: 1.0\nGenerator: pypi-pack.py\nRoot-Is-Purelib: false\n"
        + "".join(tags)
    )
    files += [
        (f"{di}/METADATA", meta.encode(), 0o644),
        (f"{di}/WHEEL", wheel.encode(), 0o644),
        (f"{di}/entry_points.txt",
         b"[console_scripts]\nagent-hooks = builtby_win_agent_hooks.__main__:main\n", 0o644),
    ]
    record = io.StringIO()
    for name, data, _ in files:
        h = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        record.write(f"{name},sha256={h},{len(data)}\n")
    record.write(f"{di}/RECORD,,\n")
    files.append((f"{di}/RECORD", record.getvalue().encode(), 0o644))
    path = out / f"{DIST}-{version}-py3-none-{plat}.whl"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data, mode in files:
            zi = zipfile.ZipInfo(name, (2020, 1, 1, 0, 0, 0))
            zi.compress_type = zipfile.ZIP_DEFLATED
            zi.create_system = 3
            zi.external_attr = (0o100000 | mode) << 16
            z.writestr(zi, data)
    return path


def main() -> None:
    if len(sys.argv) != 4:
        sys.exit("usage: pypi-pack.py <version> <assets-dir> <out-dir>")
    version, assets, out = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
    out.mkdir(parents=True, exist_ok=True)
    bins = {t: extract_binary(assets, t) for t in TRIPLES}
    for triple, plat in TRIPLES.items():
        print(build(version, triple, plat, bins[triple], out))


if __name__ == "__main__":
    main()
