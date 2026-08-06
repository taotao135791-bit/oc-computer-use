#!/usr/bin/env bash
# Seed the initial state for finder tasks.
set -euo pipefail
mkdir -p "$SCRATCH/data"
printf 'rename me\n' > "$SCRATCH/old.txt"
printf 'move me\n' > "$SCRATCH/data/item.txt"
