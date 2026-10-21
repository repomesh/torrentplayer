#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Builds the playback-only FFmpeg that TorrentStream runs to convert Dolby and DTS audio, which
# the built-in player cannot decode, to AAC (see docs/adr/0007-dolby-and-dts-audio.md).
#
# Usage: scripts/build-ffmpeg.sh <rust-target-triple> <output-dir>
#        scripts/build-ffmpeg.sh source <output-dir>
#
# Writes <output-dir>/torrentstream-ffmpeg-<triple>[.exe], the name Tauri expects for a sidecar
# (bundle.externalBin; see src-tauri/tauri.sidecar.conf.json), with FFmpeg's license files.
# `source` only downloads and verifies the source tarball, which releases publish for the LGPL.
#
# The build is LGPL-2.1-or-later: no --enable-gpl, no --enable-nonfree, and nothing detected from
# the build machine (--disable-autodetect). It contains only what playback needs: the Matroska
# and MP4 demuxers, the Dolby and DTS decoders, the AAC encoder, the MP4 muxer, and the loopback
# HTTP input (no file access). Supported targets: x86_64-unknown-linux-gnu (built on Linux),
# x86_64-pc-windows-msvc (cross-compiled on Linux with mingw-w64, or built with MinGW on
# Windows), aarch64-apple-darwin and x86_64-apple-darwin (built on macOS).
set -euo pipefail

FFMPEG_VERSION=9.0.2
# SHA-256 of ffmpeg-9.0.2.tar.xz, whose signature was checked against the FFmpeg release key
# (FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8) when it was pinned.
FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e

target=${1:?usage: build-ffmpeg.sh <rust-target-triple | source> <output-dir>}
out=${2:?usage: build-ffmpeg.sh <rust-target-triple | source> <output-dir>}
work=${FFMPEG_WORK_DIR:-$PWD/target/ffmpeg-build}
jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)

case "$target" in
  source) ;;
  x86_64-unknown-linux-gnu)
    platform=(--arch=x86_64 --target-os=linux)
    ext=
    ;;
  x86_64-pc-windows-msvc | x86_64-pc-windows-gnu)
    # A self-contained executable: MinGW's runtime is linked statically.
    platform=(--arch=x86_64 --target-os=mingw32 --extra-ldflags=-static)
    if [ "$(uname -s)" = Linux ]; then
      platform+=(--enable-cross-compile --cross-prefix=x86_64-w64-mingw32-)
    fi
    ext=.exe
    ;;
  aarch64-apple-darwin)
    platform=(--arch=arm64 --target-os=darwin --enable-cross-compile
      --cc="clang -arch arm64" --extra-ldflags="-arch arm64 -mmacosx-version-min=11.0"
      --extra-cflags=-mmacosx-version-min=11.0)
    ext=
    ;;
  x86_64-apple-darwin)
    platform=(--arch=x86_64 --target-os=darwin --enable-cross-compile
      --cc="clang -arch x86_64" --extra-ldflags="-arch x86_64 -mmacosx-version-min=10.15"
      --extra-cflags=-mmacosx-version-min=10.15)
    ext=
    ;;
  *)
    echo "unsupported target: $target" >&2
    exit 1
    ;;
esac

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

mkdir -p "$work" "$out"
out=$(cd "$out" && pwd)
work=$(cd "$work" && pwd)
tarball="$work/ffmpeg-$FFMPEG_VERSION.tar.xz"
if [ ! -f "$tarball" ] || [ "$(sha256 "$tarball")" != "$FFMPEG_SHA256" ]; then
  curl -sSfL --retry 3 -o "$tarball" "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
fi
actual=$(sha256 "$tarball")
if [ "$actual" != "$FFMPEG_SHA256" ]; then
  echo "checksum mismatch for $tarball: $actual" >&2
  exit 1
fi
if [ "$target" = source ]; then
  cp "$tarball" "$out/"
  echo "verified $out/ffmpeg-$FFMPEG_VERSION.tar.xz"
  exit 0
fi

src=$(mktemp -d "$work/ffmpeg-$FFMPEG_VERSION-$target.XXXXXX")
tar -xJf "$tarball" -C "$src" --strip-components=1

cd "$src"
./configure \
  "${platform[@]}" \
  --disable-everything \
  --disable-autodetect \
  --disable-programs \
  --enable-ffmpeg \
  --disable-ffplay \
  --disable-ffprobe \
  --disable-doc \
  --disable-debug \
  --disable-shared \
  --enable-static \
  --disable-x86asm \
  --disable-avdevice \
  --disable-swscale \
  --enable-swresample \
  --enable-avfilter \
  --enable-network \
  --enable-protocol=pipe,http,tcp \
  --enable-demuxer=matroska,mov \
  --enable-decoder=ac3,eac3,truehd,mlp,dca \
  --enable-encoder=aac \
  --enable-muxer=mp4 \
  --enable-parser=ac3,mlp,dca,aac,h264,hevc,av1,vp9,mpegaudio,opus,vorbis,flac \
  --enable-bsf=aac_adtstoasc,extract_extradata,vp9_superframe \
  --enable-filter=aresample,aformat,anull,null,abuffer,abuffersink,buffer,buffersink \
  --extra-version=torrentstream

"${MAKE:-make}" -j"$jobs" "ffmpeg$ext"

binary="$out/torrentstream-ffmpeg-$target$ext"
cp "ffmpeg$ext" "$binary"
strip_tool=strip
if [ "$ext" = .exe ] && [ "$(uname -s)" = Linux ]; then
  strip_tool=x86_64-w64-mingw32-strip
fi
"$strip_tool" "$binary"

# Its configuration must stay LGPL and include what playback needs; checked here so a change to
# this script (or to FFmpeg's dependencies between components) cannot slip by. This works for
# cross-compiled builds too, which cannot be run here.
if grep -Eq 'CONFIG_GPL 1|CONFIG_NONFREE 1' config.h; then
  echo "the build is not LGPL" >&2
  exit 1
fi
for component in AC3_DECODER EAC3_DECODER TRUEHD_DECODER MLP_DECODER DCA_DECODER AAC_ENCODER   MATROSKA_DEMUXER MOV_DEMUXER MP4_MUXER HTTP_PROTOCOL PIPE_PROTOCOL ARESAMPLE_FILTER; do
  if ! grep -q "^#define CONFIG_$component 1" config_components.h; then
    echo "the build lacks $component" >&2
    exit 1
  fi
done
cp LICENSE.md COPYING.LGPLv2.1 "$out/"
echo "built $binary ($(wc -c <"$binary") bytes)"
