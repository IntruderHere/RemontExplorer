#!/usr/bin/env python3
"""Собрать update.json для обновления вне GitHub.

Кладётся рядом с архивом в сетевой папке или на внутреннем сервере.
Приложение читает его, если в настройках указан «Источник обновлений».

Примеры:

    # архив и манифест лежат в сетевой папке
    python make_update_json.py --version 2.5.0 \
        --zip RemontExplorer-windows-x64.zip \
        --base '\\\\server\\share\\RemontExplorer' \
        --out '\\\\server\\share\\RemontExplorer\\update.json'

    # внутренний веб-сервер
    python make_update_json.py --version 2.5.0 \
        --zip RemontExplorer-windows-x64.zip \
        --base https://files.example.ru/remont \
        --out update.json
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys


def sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser(description="Собрать update.json для RemontExplorer")
    ap.add_argument("--version", required=True, help="версия, например 2.5.0")
    ap.add_argument("--zip", required=True, help="путь к архиву приложения")
    ap.add_argument(
        "--base",
        default="",
        help="папка или URL, где будет лежать архив (пусто — имя файла без пути)",
    )
    ap.add_argument("--notes", default="", help="описание изменений")
    ap.add_argument("--out", default="update.json", help="куда записать манифест")
    args = ap.parse_args()

    if not os.path.isfile(args.zip):
        print(f"нет файла: {args.zip}", file=sys.stderr)
        return 1

    version = args.version.strip().lstrip("vV")
    name = os.path.basename(args.zip)
    if args.base:
        sep = "\\" if "\\" in args.base or args.base.startswith("\\\\") else "/"
        url = args.base.rstrip("\\/") + sep + name
    else:
        url = name

    manifest = {
        "version": version,
        "url": url,
        "sha256": sha256(args.zip),
        "notes": args.notes,
    }
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
        f.write("\n")

    size = os.path.getsize(args.zip)
    print(f"update.json: {args.out}")
    print(f"  версия : {version}")
    print(f"  архив  : {url} ({size} байт)")
    print(f"  sha256 : {manifest['sha256']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
