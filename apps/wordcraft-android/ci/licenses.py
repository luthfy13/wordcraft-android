#!/usr/bin/env python3
"""Lists the Rust crates in Folio Writer's Android build with their licences, for the app's About › Licenses
page (licenses/crates.txt, compiled in). CI runs it and fails when the committed list is out of date.

Usage: licenses.py [OUT]   (from the repository root; default apps/wordcraft-android/licenses/crates.txt)
"""
import json
import subprocess
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "apps/wordcraft-android/licenses/crates.txt"
TARGET = "aarch64-linux-android"

meta = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
)
packages = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
root = next(p["id"] for p in meta["packages"] if p["name"] == "wordcraft-android")

# What ships: the normal (not build or dev) dependencies, transitively.
seen, todo = set(), [root]
while todo:
    pid = todo.pop()
    if pid in seen:
        continue
    seen.add(pid)
    for dep in nodes[pid]["deps"]:
        if any(k["kind"] is None for k in dep["dep_kinds"]):
            todo.append(dep["pkg"])

rows = set()
for pid in seen:
    p = packages[pid]
    if p["source"] is None:
        continue  # this workspace: MIT OR Apache-2.0, listed on its own
    licence = p.get("license") or ("see " + p["license_file"] if p.get("license_file") else "unknown")
    rows.add(f"{p['name']} {p['version']}: {licence}")

text = "".join(f"{r}\n" for r in sorted(rows, key=str.lower))
with open(OUT, "w", encoding="utf-8") as f:
    f.write(text)
print(f"{len(rows)} crates written to {OUT}")
