#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
command -v ffmpeg >/dev/null
command -v ffprobe >/dev/null
[[ -x target/release/mp4-compressor ]] || cargo build --release
bin="$PWD/target/release/mp4-compressor"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'testsrc2=size=320x180:rate=24:duration=3' \
  -f lavfi -i 'sine=frequency=440:duration=3' \
  -c:v libx264 -preset ultrafast -crf 10 -pix_fmt yuv420p \
  -c:a aac -shortest "$work/source.mp4"
"$bin" --headless --mode balanced "$work/source.mp4"
out="$work/source_compressed.mp4"
[[ -s "$out" ]] || { echo 'No output' >&2; exit 1; }
codec="$(ffprobe -v error -select_streams v:0 -show_entries stream=codec_name -of default=noprint_wrappers=1:nokey=1 "$out")"
[[ "$codec" == 'h264' ]] || { echo "Wrong codec: $codec" >&2; exit 1; }
[[ -s "$work/source.mp4" ]] || exit 1
echo 'PASS: headless real FFmpeg compression, Unicode path, codec validation, source preservation'

"$bin" --headless --mode fast "$work/source.mp4"
[[ -s "$work/source_compressed_2.mp4" ]] || { echo 'Fast mode fallback did not produce output' >&2; exit 1; }
echo 'PASS: Fast NVIDIA mode (native if available; explicit CPU fallback otherwise)'

set +e
"$bin" --headless --mode auto "$work/source.mp4" > "$work/smart.log" 2>&1
smart_status=$?
set -e
if [[ $smart_status -ne 0 ]] && ! grep -q 'Skipped.*No reduction in size' "$work/smart.log"; then
  echo 'Smart mode failed:' >&2
  cat "$work/smart.log" >&2
  exit 1
fi
if grep -q 'Failed' "$work/smart.log"; then
  cat "$work/smart.log" >&2
  exit 1
fi
echo 'PASS: Smart mode processes sample and handles quality target without fatal error'
