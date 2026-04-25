"""
作用
-
把 `data.txt` 和 `crates.txt` 的数据导入到 Postgres 的 `crates` 表，并使用 UPSERT（按 `id` 冲突更新）
实现可重复执行。`version_new` 会写入 `data.txt` 中同名 crate 的“最新版本号”（按 semver 规则比较）。

数据来源与字段映射
-
- crates.txt: 提供 `id/name/created_at/updated_at`
- data.txt: 提供 `crate_name/crate_version`，按 `crate_name == crates.name` 计算 `version_new`
- homepage: 按模板 `https://static.crates.io/crates/{crate_name}` 拼接
- 其他字段：`analyzed = false`，`download = false`

数据库连接
-
按以下优先级读取 `DATABASE_URL`：
1) 环境变量 `DATABASE_URL`
2) 本仓库根目录的 `.env` 文件

依赖
-
需要安装任意一个 Postgres 驱动：
- psycopg v3（推荐）：`pip install psycopg[binary]`
- psycopg2：`pip install psycopg2-binary`

使用方式 / 启动方式
-
在**仓库**根目录执行：(再根目录下运行以下命令)

1) 直接使用默认路径（脚本同目录下的 data.txt 和 crates.txt）：
   `python datahandle/data_import/import.py`

2) 指定输入文件与批大小：
   `python datahandle/data_import/import.py --data-txt <path> --crates-txt <path> --batch-size 2000`
"""

from __future__ import annotations

import argparse
import csv
import os
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable, Optional

STATIC_CRATES_HOMEPAGE_BASE = "https://static.crates.io/crates/"


def load_dotenv(dotenv_path: Path) -> dict[str, str]:
    if not dotenv_path.exists():
        return {}

    env: dict[str, str] = {}
    for raw_line in dotenv_path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        env[key.strip()] = value.strip()
    return env


def resolve_database_url() -> str:
    if os.environ.get("DATABASE_URL"):
        return os.environ["DATABASE_URL"]
    root_env = Path(__file__).resolve().parents[2] / ".env"
    env = load_dotenv(root_env)
    if env.get("DATABASE_URL"):
        return env["DATABASE_URL"]
    raise SystemExit(f"DATABASE_URL not found in environment or {root_env}")


def connect(database_url: str):
    try:
        import psycopg  # type: ignore

        return psycopg.connect(database_url)
    except Exception:
        pass

    try:
        import psycopg2  # type: ignore

        return psycopg2.connect(database_url)
    except Exception as exc:
        raise SystemExit(
            "Could not import/connect with psycopg (v3) or psycopg2.\n"
            "Install one of them, e.g.:\n"
            "  pip install psycopg[binary]\n"
            "or\n"
            "  pip install psycopg2-binary\n"
        ) from exc


_TZ_SUFFIX_RE = re.compile(r"([+-]\d{2})$")


def parse_crates_datetime(raw: str) -> Optional[datetime]:
    s = raw.strip().strip('"')
    if not s:
        return None
    s = s.replace(".-1f", "")
    s = _TZ_SUFFIX_RE.sub(r"\g<1>00", s)
    try:
        dt = datetime.strptime(s, "%d/%m/%Y %H:%M:%S%z")
        return dt.astimezone(timezone.utc).replace(tzinfo=None)
    except ValueError:
        return None


@dataclass(frozen=True)
class SemVer:
    major: int
    minor: int
    patch: int
    is_release: int
    prerelease: tuple[tuple[int, object], ...]
    original: str


def semver_key(version: str) -> tuple:
    original = version.strip()
    if not original:
        return (-1, -1, -1, -1, (), "")

    base, _, build = original.partition("+")
    core, _, prerelease_raw = base.partition("-")
    nums = core.split(".")
    try:
        major = int(nums[0])
        minor = int(nums[1]) if len(nums) > 1 else 0
        patch = int(nums[2]) if len(nums) > 2 else 0
    except Exception:
        return (-1, -1, -1, -1, (), original)

    prerelease: list[tuple[int, object]] = []
    if prerelease_raw:
        for part in prerelease_raw.split("."):
            if part.isdigit():
                prerelease.append((0, int(part)))
            else:
                prerelease.append((1, part))

    is_release = 1 if not prerelease_raw else 0
    return (major, minor, patch, is_release, tuple(prerelease), original)


def load_latest_versions(data_txt: Path) -> dict[str, str]:
    versions_by_name: dict[str, str] = {}
    with data_txt.open("r", encoding="utf-8", newline="") as f:
        reader = csv.DictReader(f)
        for row in reader:
            name = (row.get("crate_name") or "").strip()
            version = (row.get("crate_version") or "").strip()
            if not name:
                continue
            if not version:
                continue
            prev = versions_by_name.get(name)
            if prev is None or semver_key(version) > semver_key(prev):
                versions_by_name[name] = version
    return versions_by_name


def iter_crates_rows(
    crates_txt: Path, latest_versions: dict[str, str]
) -> Iterable[tuple[int, str, str, bool, bool, datetime, datetime, str]]:
    with crates_txt.open("r", encoding="utf-8", newline="") as f:
        header = f.readline()
        if not header:
            return
        for line in f:
            line = line.strip()
            if not line:
                continue
            parts = [p.strip().strip('"') for p in line.split("\t")]
            if len(parts) < 4:
                continue
            raw_id, name, updated_raw, created_raw = parts[0], parts[1], parts[2], parts[3]
            try:
                crate_id = int(raw_id)
            except ValueError:
                continue

            created_at = parse_crates_datetime(created_raw) or datetime.utcnow()
            updated_at = parse_crates_datetime(updated_raw) or created_at
            version_new = latest_versions.get(name, "")
            homepage = f"{STATIC_CRATES_HOMEPAGE_BASE}{name}"

            yield (
                crate_id,
                name,
                homepage,
                False,
                False,
                created_at,
                updated_at,
                version_new,
            )


def upsert_crates(conn, rows: Iterable[tuple], batch_size: int) -> int:
    sql = """
        INSERT INTO crates
            (id, name, homepage, analyzed, download, created_at, updated_at, version_new)
        VALUES
            (%s, %s, %s, %s, %s, %s, %s, %s)
        ON CONFLICT (id) DO UPDATE SET
            name = EXCLUDED.name,
            homepage = EXCLUDED.homepage,
            analyzed = EXCLUDED.analyzed,
            download = EXCLUDED.download,
            created_at = EXCLUDED.created_at,
            updated_at = EXCLUDED.updated_at,
            version_new = EXCLUDED.version_new
    """

    total = 0
    buffer: list[tuple] = []
    with conn:
        with conn.cursor() as cur:
            for row in rows:
                buffer.append(row)
                if len(buffer) >= batch_size:
                    cur.executemany(sql, buffer)
                    total += len(buffer)
                    buffer.clear()
            if buffer:
                cur.executemany(sql, buffer)
                total += len(buffer)
    return total


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--data-txt",
        default=str(Path(__file__).resolve().parent / "data.txt"),
    )
    parser.add_argument(
        "--crates-txt",
        default=str(Path(__file__).resolve().parent / "crates.txt"),
    )
    parser.add_argument("--batch-size", type=int, default=2000)
    args = parser.parse_args()

    data_txt = Path(args.data_txt)
    crates_txt = Path(args.crates_txt)
    if not data_txt.exists():
        raise SystemExit(f"Missing file: {data_txt}")
    if not crates_txt.exists():
        raise SystemExit(f"Missing file: {crates_txt}")

    database_url = resolve_database_url()
    latest_versions = load_latest_versions(data_txt)
    rows = iter_crates_rows(crates_txt, latest_versions)

    conn = connect(database_url)
    try:
        total = upsert_crates(conn, rows, batch_size=args.batch_size)
        print(f"Upserted {total} rows into crates")
    finally:
        conn.close()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
