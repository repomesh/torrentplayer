#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Downloads the openly licensed Blender films (CC BY) from download.blender.org into media/
# (next to this script, git-ignored) and builds the test variants benchmarks.txt uses. About
# 2.8 GB (plus 6.7 GB with --4k).
# Needs curl, unzip and an FFmpeg with encoders: set FFMPEG=/path/to/ffmpeg, or have it on PATH.
# Usage: bash crates/torrentplayer-bench/lists/prepare-media.sh [--4k]
set -euo pipefail
mkdir -p "$(dirname "$0")/media"
cd "$(dirname "$0")/media"
FFMPEG="${FFMPEG:-ffmpeg}"
BASE=https://download.blender.org/demo/movies

fetch() { # url, output: download once, resume if interrupted
  [ -s "$2" ] && { echo "have $2"; return; }
  echo "downloading $2"; curl -fL -C - -o "$2.part" "$1" && mv "$2.part" "$2"
}
unzip_one() { # zip, wanted file
  [ -s "$2" ] || unzip -o -j "$1" "*$2" -d .
}

# Big Buck Bunny 4K 60 fps, and a copy with the MP4 index (moov) at the end.
fetch "$BASE/BBB/bbb_sunflower_2160p_60fps_normal.mp4.zip" bbb_2160p60.zip
unzip_one bbb_2160p60.zip bbb_sunflower_2160p_60fps_normal.mp4
[ -s bbb_sunflower_2160p_60fps_moov-at-end.mp4 ] ||
  "$FFMPEG" -hide_banner -loglevel error -i bbb_sunflower_2160p_60fps_normal.mp4 -map 0 -c copy \
    -movflags -faststart bbb_sunflower_2160p_60fps_moov-at-end.mp4

# Sintel 1080p (MKV).
fetch "$BASE/Sintel.2010.1080p.mkv" Sintel.2010.1080p.mkv

# Tears of Steel 1080p with its official Dolby Digital 5.1 (AC-3) mix, as MKV.
fetch "$BASE/ToS/tears_of_steel_1080p.mov.zip" tos_1080p.zip
unzip_one tos_1080p.zip tears_of_steel_1080p.mov
fetch "$BASE/ToS/Surround-TOS_DVDSURROUND-Dolby%205.1.ac3" tos_surround_5.1.ac3
[ -s tears_of_steel_1080p_ac3-5.1.mkv ] ||
  "$FFMPEG" -hide_banner -loglevel error -i tears_of_steel_1080p.mov -i tos_surround_5.1.ac3 \
    -map 0:v:0 -map 1:a:0 -c copy -shortest tears_of_steel_1080p_ac3-5.1.mkv

# Optional: Tears of Steel 4K (6.7 GB download).
if [ "${1:-}" = --4k ]; then
  fetch "$BASE/ToS/tearsofsteel_4k.mov.zip" tos_4k.zip
  unzip_one tos_4k.zip tearsofsteel_4k.mov
fi

echo "ready:"; ls -la *.mp4 *.mkv *.mov 2>/dev/null
