#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

legacy_product="mp4""rust"
legacy_binary="mp4""rust-""native"
legacy_package="mp4-rust-""native"
legacy_working=".mp4""rust-working-"
legacy_samples="mp4""rust-samples-"
legacy_pattern="${legacy_product}|${legacy_binary}|${legacy_package}|${legacy_working}|${legacy_samples}"
legacy_path_pattern="(^|/)([^/]*(${legacy_product}|rust-""native|internal|staging|test)[^/]*)$"

markdown_files="$(git ls-files '*.md')"
if [[ "$markdown_files" != "README.md" ]]; then
  echo 'Preflight failed: only README.md may be public Markdown.' >&2
  printf '%s\n' "$markdown_files" >&2
  exit 1
fi

if git ls-files | rg -n -i "$legacy_path_pattern"; then
  echo 'Preflight failed: internal or test identifiers remain in public file names.' >&2
  exit 1
fi

if git ls-files -z | xargs -0 rg -n -I -i -e "$legacy_pattern"; then
  echo 'Preflight failed: legacy product identifiers remain in tracked files.' >&2
  exit 1
fi

if ! rg -n 'name = "mp4-compressor"' Cargo.toml Cargo.lock; then
  echo 'Preflight failed: canonical package name is missing.' >&2
  exit 1
fi

for binary in target/release/mp4-compressor target/release/mp4-compressor-linux-x86_64; do
  [[ -f "$binary" ]] || continue
  if strings -a "$binary" | rg -n -i -e "$legacy_pattern|/home/|/Users/|C:\\\\Users\\\\|[[:alnum:]._%+-]+@[[:alnum:].-]+\\.[A-Za-z]{2,}|BEGIN (RSA|OPENSSH|EC|DSA) PRIVATE KEY|gh[pousr]_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9]{20,}"; then
    echo "Preflight failed: identifying or legacy metadata found in $binary." >&2
    exit 1
  fi
done

echo 'Preflight passed: canonical names, public files, and release metadata checked.'
