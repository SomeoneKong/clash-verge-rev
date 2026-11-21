"""Decide whether a fork release can replace the current updater feed."""

import json
import re
import sys
from pathlib import Path

NUMBER = r"(?:0|[1-9][0-9]*)"
IDENTIFIER = rf"(?:{NUMBER}|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
VERSION = re.compile(
    rf"({NUMBER})\.({NUMBER})\.({NUMBER})(?:-({IDENTIFIER}(?:\.{IDENTIFIER})*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def precedence(path):
    version = json.loads(Path(path).read_text(encoding="utf-8"))["version"]
    match = VERSION.fullmatch(version)
    if not match:
        raise ValueError(f"{path} has an invalid version: {version!r}")
    major, minor, patch, pre = match.groups()
    identifiers = tuple(
        (0, int(part)) if part.isdigit() else (1, part)
        for part in pre.split(".")
    ) if pre else ()
    return int(major), int(minor), int(patch), pre is None, identifiers


if __name__ == "__main__":
    offered = precedence(sys.argv[1])
    print("true" if len(sys.argv) == 2 or offered >= precedence(sys.argv[2]) else "false")
