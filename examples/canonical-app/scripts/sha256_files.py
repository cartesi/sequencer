#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import sys
from pathlib import Path


def sha256_for_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as input_file:
        for chunk in iter(lambda: input_file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main(argv: list[str]) -> int:
    for raw_path in argv[1:]:
        path = Path(raw_path)
        print(f"{sha256_for_file(path)}  {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
