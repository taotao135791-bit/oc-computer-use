#!/usr/bin/env bash
# Seed the initial state for cross-app tasks.
set -euo pipefail
mkdir -p "$SCRATCH"
printf 'hello from the seed file\n' > "$SCRATCH/cross-03-input.txt"
