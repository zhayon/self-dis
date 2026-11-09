#!/usr/bin/env bash
# Собирает бинарь для виртуалки и кладёт его в dist/self-dis.
# PLATFORM=linux/arm64 scripts/build-linux.sh — если виртуалка ARM.
set -euo pipefail

cd "$(dirname "$0")/.."
platform="${PLATFORM:-linux/amd64}"
mkdir -p dist

docker buildx build \
    --platform "$platform" \
    --file deploy/Dockerfile \
    --output "type=local,dest=dist" \
    .

chmod 755 dist/self-dis
echo "готово: dist/self-dis ($platform)"
