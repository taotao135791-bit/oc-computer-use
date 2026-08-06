#!/usr/bin/env bash
# Seed the initial state for textedit tasks. Runs with cwd = benchmarks/ and
# $SCRATCH set to the task's scratch dir.
set -euo pipefail
mkdir -p "$SCRATCH"
printf 'alpha beta gamma\n' > "$SCRATCH/textedit-02.txt"
printf 'alpha beta gamma\n' > "$SCRATCH/textedit-03.txt"
printf 'alpha beta gamma\n' > "$SCRATCH/textedit-04.txt"
