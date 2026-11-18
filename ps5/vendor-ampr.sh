#!/bin/sh
# Bundles a local ampr-emu-ng build into kraken-mount: copies its
# libSceAmpr.sprx (and libSceAmpr-debug.sprx when present) into
# vendor/ampr-emu-ng/, writes VERSION from its crates/ampr-sys/Cargo.toml and
# regenerates SHA256SUMS, so ps5/build.sh's checksum check passes for that
# build. Run it after every ampr-emu-ng build, before ps5/build.sh.
#
#   ./ps5/vendor-ampr.sh ../ampr-emu-ng-main
#   ./ps5/build.sh
set -eu

ng=${1:-../ampr-emu-ng-main}
here=$(cd "$(dirname "$0")/.." && pwd)
dst=$here/vendor/ampr-emu-ng

if [ ! -f "$ng/libSceAmpr.sprx" ]; then
  echo "vendor-ampr: $ng/libSceAmpr.sprx not found: build ampr-emu-ng first (./ps5/build-docker.sh)" >&2
  exit 1
fi
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ng/crates/ampr-sys/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
  echo "vendor-ampr: no version in $ng/crates/ampr-sys/Cargo.toml" >&2
  exit 1
fi

cp "$ng/libSceAmpr.sprx" "$dst/libSceAmpr.sprx"
files=libSceAmpr.sprx
if [ -f "$ng/libSceAmpr-debug.sprx" ]; then
  cp "$ng/libSceAmpr-debug.sprx" "$dst/libSceAmpr-debug.sprx"
  files="$files libSceAmpr-debug.sprx"
else
  # An old debug copy would not match this release: drop it.
  rm -f "$dst/libSceAmpr-debug.sprx"
fi
printf 'v%s' "$version" > "$dst/VERSION"
# shellcheck disable=SC2086
(cd "$dst" && sha256sum $files > SHA256SUMS && sha256sum -c --strict SHA256SUMS)

if ! grep -q "ampr_emu_status.txt" "$dst/libSceAmpr.sprx"; then
  echo "vendor-ampr: warning: this libSceAmpr.sprx writes no status file (not a 0.5.x mejoras build)" >&2
fi
echo "vendor-ampr: bundled ampr-emu-ng v$version from $ng"
