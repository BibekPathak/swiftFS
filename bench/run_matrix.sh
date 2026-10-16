#!/usr/bin/env bash
# Run the SwiftFS chunk-size x access-pattern matrix and archive raw results.
# Bulk outputs go to bench-out/ (git-ignored). One image is built per cell and
# removed immediately to bound tmpfs usage.
set -u
export PATH="$PWD/target/release:$PATH"
DS="${DS:-/tmp/ml_large}"
OUT="${OUT:-$PWD/bench-out}"
mkdir -p "$OUT"

run_cell () {
  local cs="$1" acc="$2" rpf="$3" rb="$4" tag="$5"
  local od="/tmp/ml_cell_$tag"
  rm -rf "$od"
  echo "=== chunk=$cs access=$acc tag=$tag ==="
  timeout 1500 python3 bench/compare.py "$DS" --out "$od" \
      --iterations 5 --access "$acc" --reads-per-file "$rpf" --read-bytes "$rb" \
      --chunk-size "$cs" --metrics --validate 2>&1 | tail -6
  cp "$od/results.json" "$OUT/results_$tag.json" 2>/dev/null
  cp "$od/summary.csv"  "$OUT/summary_$tag.csv"  2>/dev/null
  rm -rf "$od"
}
