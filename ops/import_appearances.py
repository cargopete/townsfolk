#!/usr/bin/env python3
"""Populate the `appearances` table from docs/portrait-prompts.md — each soul's body as they know
it, fed into their dialogue and reflections so they are embodied (aware of their own face, build,
dress and years). Idempotent: re-run after editing the portrait prompts or adding a sitter.

    python3 ops/import_appearances.py            # uses ../world.db
    python3 ops/import_appearances.py path/to.db
"""
import re, sqlite3, pathlib, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DB = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / "world.db")
DOC = ROOT / "docs" / "portrait-prompts.md"

db = sqlite3.connect(DB)
db.execute("CREATE TABLE IF NOT EXISTS appearances(name TEXT PRIMARY KEY, text TEXT NOT NULL)")
txt = DOC.read_text()
# lines like:  - ✅ **[18] Major Pringle** — a 60-year-old … ; (or without the ✅, or em/en dash)
pat = re.compile(r'^\s*-\s*(?:✅\s*)?\*\*\[\d+\]\s*(.+?)\*\*\s*[—–-]+\s*(.+)$', re.M)
n = 0
for m in pat.finditer(txt):
    name = re.sub(r'\s*\*?\(.*?\)\*?', '', m.group(1)).strip()   # drop "(the late … murdered)" asides
    desc = m.group(2).strip().rstrip('.')
    if not name or not desc:
        continue
    db.execute("INSERT INTO appearances(name,text) VALUES(?,?) ON CONFLICT(name) DO UPDATE SET text=?2",
               (name, desc + '.'))
    n += 1
db.commit()
db.close()
print(f"populated {n} appearances into {DB}")
