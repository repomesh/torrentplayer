#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Installs the Linux packages in a clean container of a distribution and checks that the
# GStreamer elements WebKitGTK plays media with are there, and that the app finds every library
# it links. The AppImage is checked on Arch, which has no .deb or .rpm: the plugins it bundles
# must be there and resolve their libraries from the bundle and an Arch desktop's base libraries.
#
# Usage: scripts/test-linux-packages.sh <docker image> <folder with the .deb, .rpm, and AppImage>
#   e.g. scripts/test-linux-packages.sh fedora:41 build-assets
# Needs Docker. Used by .github/workflows/build.yml for each supported distribution.
set -euo pipefail

image=${1:?usage: test-linux-packages.sh <docker image> <assets folder>}
assets=$(cd "${2:?usage: test-linux-packages.sh <docker image> <assets folder>}" && pwd)

# What WebKitGTK needs, as alternatives separated by commas: an audio sink (good), the H.264 parser
# for Media Source Extensions (bad), the GL video sink (GL), and an H.264 and an AAC decoder
# (libav on Debian and Ubuntu; openh264 and the free libav on Fedora and openSUSE, whose libav
# leaves H.264 out).
elements='autoaudiosink h264parse glimagesink avdec_h264,openh264dec avdec_aac,fdkaacdec,faad'

# Runs in a package's container: each element must exist, and the app must find its libraries.
check_packages='
set -u
missing=0
for alternatives in '"$elements"'; do
  found=
  for element in $(echo "$alternatives" | tr "," " "); do
    if gst-inspect-1.0 --exists "$element"; then found=$element; break; fi
  done
  if [ -n "$found" ]; then echo "ok: $found"; else echo "MISSING: $alternatives"; missing=1; fi
done
if ldd /usr/bin/TorrentPlayer | grep "not found"; then
  echo "MISSING: libraries of the app"; missing=1
else
  echo "ok: the app finds its libraries"
fi
exit "$missing"
'

# Runs in an Arch container with the AppImage: its plugins and its app must resolve their
# libraries from the bundle plus what every Arch desktop has (GTK and its libraries, ALSA, Mesa),
# which the AppImage leaves out on purpose.
check_appimage='
set -u
pacman -Sy --noconfirm --quiet gtk3 alsa-lib mesa > /dev/null
cd /tmp && cp /assets/*.AppImage app.AppImage && chmod +x app.AppImage
./app.AppImage --appimage-extract > /dev/null
root=/tmp/squashfs-root
libs=$(find "$root" -name "*.so*" -printf "%h\n" | sort -u | tr "\n" ":")
missing=0
# The plugin files holding the elements above (avdec_* in libav, h264parse in videoparsersbad).
for plugin in libgstautodetect libgstvideoparsersbad libgstopengl libgstlibav; do
  file=$(find "$root" -name "$plugin.so" | head -n 1)
  if [ -z "$file" ]; then echo "MISSING: $plugin.so"; missing=1; continue; fi
  if LD_LIBRARY_PATH="$libs" ldd "$file" | grep "not found"; then
    echo "MISSING: libraries of $plugin.so"; missing=1
  else
    echo "ok: $plugin.so"
  fi
done
if LD_LIBRARY_PATH="$libs" ldd "$root/usr/bin/TorrentPlayer" | grep "not found"; then
  echo "MISSING: libraries of the app"; missing=1
else
  echo "ok: the app finds its libraries"
fi
exit "$missing"
'

case "$image" in
  debian:* | ubuntu:*)
    run='export DEBIAN_FRONTEND=noninteractive
      apt-get update -qq
      apt-get install -y -qq /assets/*.deb gstreamer1.0-tools > /dev/null || exit 1'"$check_packages" ;;
  fedora:*)
    # openh264 comes from Cisco's repository, which Fedora enables by default.
    run='dnf install -y -q --setopt=fedora-cisco-openh264.enabled=1 /assets/*.rpm gstreamer1 > /dev/null || exit 1'"$check_packages" ;;
  opensuse/*)
    run='zypper --non-interactive --quiet install --allow-unsigned-rpm /assets/*.rpm gstreamer-utils || exit 1'"$check_packages" ;;
  archlinux*)
    run=$check_appimage ;;
  *)
    echo "No install recipe for $image" >&2
    exit 2 ;;
esac

echo "== $image"
# Docker Desktop on Windows (Git Bash) wants a Windows path for the mount.
host=$assets
if command -v cygpath > /dev/null; then host=$(cygpath -m "$assets"); fi
MSYS_NO_PATHCONV=1 docker run --rm -v "$host:/assets:ro" "$image" bash -c "$run"
