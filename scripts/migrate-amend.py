#!/usr/bin/env python3
"""One-off migration for the Entry::Amend retirement.

Session files written before the change store a user's follow-up words as
`{"kind":"amend","target":N,"text":...}`. The new format keeps them as
standalone user messages instead. This rewrites every session file in the
directory, turning each amend into a user message of the same id.

Run `--dry-run` first to preview; the script only rewrites files that contain
amend entries, atomically (tmp + rename), and leaves everything else untouched.
"""

import argparse
import json
import sys
from pathlib import Path


def migrate_entry(e):
    """An amend becomes a standalone user message, keeping its id."""
    return {
        "kind": "message",
        "id": e["id"],
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": e["text"]}],
        },
    }


def migrate(entries):
    out = []
    changed = False
    for e in entries:
        if e.get("kind") == "amend":
            out.append(migrate_entry(e))
            changed = True
        else:
            out.append(e)
    return out, changed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "dir",
        nargs="?",
        default=str(Path.home() / ".local/state/pi/sessions"),
        help="directory of session files (default ~/.local/state/pi/sessions)",
    )
    ap.add_argument("--dry-run", action="store_true", help="preview without writing")
    args = ap.parse_args()

    files = sorted(Path(args.dir).glob("*.json"))
    total = 0
    for f in files:
        try:
            d = json.load(open(f))
        except (OSError, json.JSONDecodeError) as err:
            print(f"skip {f.name}: {err}", file=sys.stderr)
            continue
        entries = d.get("entries", [])
        amended = [e for e in entries if e.get("kind") == "amend"]
        if not amended:
            continue
        if args.dry_run:
            total += len(amended)
            print(
                f"{f.name}: {len(amended)} amend -> "
                f"{[(e['id'], e['text'][:20]) for e in amended]}"
            )
            continue
        migrated, _ = migrate(entries)
        d["entries"] = migrated
        tmp = f.with_suffix(".json.tmp")
        with open(tmp, "w") as fh:
            json.dump(d, fh, ensure_ascii=False, separators=(",", ":"))
        tmp.replace(f)
        total += len(amended)
        print(f"rewrote {f.name}")

    if args.dry_run:
        print(f"\n{total} amend entries in {sum(1 for f in files if f.exists())} files scanned")
    else:
        print(f"\nmigrated {total} amend entries")


if __name__ == "__main__":
    main()
