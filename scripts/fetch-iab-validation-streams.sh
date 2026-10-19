#!/usr/bin/env bash
# usage: fetch-iab-validation-streams.sh <directory>
set -euo pipefail

directory=${1:?usage: fetch-iab-validation-streams.sh <directory>}
mkdir -p "$directory"
commit=694b3782672aece877a3d5f79996f379e74299e9
media=https://media.githubusercontent.com/media/DTSProAudio/iab-validator/$commit/test/bitstreams
reference=https://raw.githubusercontent.com/DTSProAudio/iab-validator/$commit/test/reference/cA

for number in $(seq 1 148); do
  stream="$directory/IABValidationStream_$number.iab.zip"
  verdict="$directory/Stream${number}_cA.json"
  if [ ! -s "$stream" ]; then
    curl -fsSL --retry 5 --retry-all-errors -o "$stream.part" "$media/IABValidationStream_$number.iab.zip"
    mv "$stream.part" "$stream"
  fi
  if [ ! -s "$verdict" ]; then
    curl -fsSL --retry 5 --retry-all-errors -o "$verdict.part" "$reference/Stream${number}_cA.json"
    mv "$verdict.part" "$verdict"
  fi
done
