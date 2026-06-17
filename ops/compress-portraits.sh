#!/usr/bin/env bash
# Compress townsfolk portraits to small, web-friendly sepia JPEGs. Idempotent: converts any
# png/jpeg/webp to an optimised .jpg (max 640×800, quality 82, stripped), removes the original,
# and re-squeezes any oversized .jpg. Run after pulling a new batch into portraits/.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)/portraits}"
cd "$DIR"
shopt -s nullglob nocaseglob

for f in *.png *.jpeg *.webp; do
  base="${f%.*}"; out="$base.jpg"
  magick "$f" -auto-orient -strip -resize '640x800>' -quality 82 "$out.tmp"
  mv -f "$out.tmp" "$out"
  [ "$f" != "$out" ] && rm -f "$f" || true
done

for f in *.jpg; do
  sz=$(stat -c%s "$f" 2>/dev/null || echo 0)
  if [ "$sz" -gt 220000 ]; then
    magick "$f" -auto-orient -strip -resize '640x800>' -quality 82 "$f.tmp" && mv -f "$f.tmp" "$f"
  fi
done
echo "compressed $(ls -1 *.jpg 2>/dev/null | wc -l) portrait(s); total $(du -sh . | cut -f1)"
