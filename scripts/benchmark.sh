#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

command -v ffmpeg >/dev/null
command -v ffprobe >/dev/null
binary="${1:-target/release/mp4-compressor}"
[[ -x "$binary" ]] || { echo "Executable not found: $binary" >&2; exit 1; }
[[ "$binary" = /* ]] || binary="$PWD/$binary"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
source="$work/render_source.mp4"

# Artificially over-encoded H.264 sample for a reproducible size-control test.
# This is NOT evidence that arbitrary real-world video can lose 90% losslessly.
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'testsrc2=size=1280x720:rate=30:duration=8' \
  -c:v libx264 -preset ultrafast -crf 4 -pix_fmt yuv420p "$source"

before="$(stat -c%s "$source")"
for mode in balanced auto; do
  "$binary" --headless --mode "$mode" --reduction 90 "$source" >"$work/$mode.log"
  if [[ "$mode" == balanced ]]; then
    output="$work/render_source_compressed.mp4"
  else
    output="$work/render_source_compressed_2.mp4"
  fi
  [[ -s "$output" ]] || { echo "$mode output missing" >&2; exit 1; }
  after="$(stat -c%s "$output")"
  reduction=$(( (before - after) * 100 / before ))
  [[ "$reduction" -ge 85 && "$reduction" -le 95 ]] || {
    echo "$mode unexpected synthetic-sample reduction: $reduction%" >&2
    exit 1
  }
  score="$(ffmpeg -hide_banner -i "$source" -i "$output" \
    -filter_complex '[0:v][1:v]ssim' -an -f null - 2>&1 \
    | rg -o 'All:[0-9.]+' | tail -n 1 | cut -d: -f2)"
  awk -v score="$score" 'BEGIN { if (score < 0.97) exit 1 }' || {
    echo "$mode failed synthetic-sample SSIM >= 0.97 (got $score)" >&2
    exit 1
  }
  echo "$mode: $before -> $after bytes, $reduction% saved, SSIM $score"
  rg 'Finished' "$work/$mode.log"
done

echo 'PASS: target-size accuracy and basic visual similarity on one compressible synthetic input'
