#!/usr/bin/env python3
"""Add or remove a COSMIC panel or dock applet in the COSMIC config files.

The COSMIC panel stores its layout as a RON value of the shape

    Some(([ ...wing start... ], [ ...wing end... ]))

in ``~/.config/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings``.
This helper inserts or removes one applet id there and leaves the rest of the
layout untouched. A timestamped backup is written before every change.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import sys
import tempfile
import time
from pathlib import Path

CONFIG_ROOT = Path.home() / ".config" / "cosmic"
PANELS = (
    ("com.system76.CosmicPanel.Panel", "plugins_wings"),
    ("com.system76.CosmicPanel.Dock", "plugins_wings"),
)
BUCKETS = ("wing_start", "wing_end")


def config_path(panel: str, key: str) -> Path:
    return CONFIG_ROOT / panel / "v1" / key


def split_buckets(text: str) -> list[list[str]] | None:
    """Extracts the two applet lists from a `Some(([..], [..]))` value.

    Returns ``None`` for a panel that carries no layout at all (``None``).
    """
    start = text.find("Some((")
    if start == -1:
        if text.strip() in ("None", "Some(None)"):
            return None
        raise ValueError("unsupported config format: 'Some((' not found")
    body = text[start + len("Some((") :]

    groups: list[list[str]] = []
    depth = 0
    current = ""
    for char in body:
        if char == "[":
            depth += 1
            current = ""
        elif char == "]":
            depth -= 1
            groups.append(re.findall(r'"([^"]*)"', current))
            current = ""
        elif depth > 0:
            current += char
        if len(groups) == 2:
            break

    if len(groups) != 2:
        raise ValueError("unsupported config format: expected two applet lists")
    return groups


def render(buckets: list[list[str]]) -> str:
    lines = ["Some((["]
    for item in buckets[0]:
        lines.append(f'    "{item}",')
    lines.append("], [")
    for item in buckets[1]:
        lines.append(f'    "{item}",')
    lines.append("]))")
    return "\n".join(lines) + "\n"


def write_atomic(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        backup = path.with_suffix(path.suffix + f".bak-{int(time.time())}")
        shutil.copy2(path, backup)
        print(f"backup: {backup}")

    handle, temporary = tempfile.mkstemp(dir=str(path.parent), prefix=path.name)
    with os.fdopen(handle, "w", encoding="utf-8") as stream:
        stream.write(text)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def apply(applet: str, remove: bool, only: str | None) -> int:
    changed = 0
    for panel, key in PANELS:
        if only and panel != only:
            continue
        path = config_path(panel, key)
        if not path.exists():
            print(f"skip   {panel}: no config at {path}")
            continue

        buckets = split_buckets(path.read_text(encoding="utf-8"))
        if buckets is None:
            print(f"skip   {panel}: layout is empty (None)")
            continue
        flat = [item for bucket in buckets for item in bucket]

        if remove:
            if applet not in flat:
                print(f"absent {panel}")
                continue
            buckets = [[item for item in bucket if item != applet] for bucket in buckets]
            action = "removed"
        else:
            if applet in flat:
                print(f"present {panel}")
                continue
            # Place it directly in front of the status area, which keeps the
            # well known right hand side applets together.
            bucket = buckets[1]
            anchor = next(
                (i for i, item in enumerate(bucket) if "StatusArea" in item),
                len(bucket),
            )
            bucket.insert(anchor, applet)
            action = "added  "

        write_atomic(path, render(buckets))
        print(f"{action} {panel}: {applet}")
        changed += 1

    if changed == 0:
        print("nothing to do")
    else:
        print("The running panel reloads its configuration automatically.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("add", "remove", "status"))
    parser.add_argument("applet", help="applet id, e.g. com.example.CosmicAppletThing")
    parser.add_argument("--panel", help="limit to one panel config name")
    args = parser.parse_args()

    if args.action == "status":
        for panel, key in PANELS:
            path = config_path(panel, key)
            if not path.exists():
                continue
            buckets = split_buckets(path.read_text(encoding="utf-8"))
            if buckets is None:
                print(f"{panel}: empty layout")
                continue
            state = "present" if args.applet in sum(buckets, []) else "absent"
            print(f"{panel}: {state}")
        return 0

    try:
        return apply(args.applet, args.action == "remove", args.panel)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
