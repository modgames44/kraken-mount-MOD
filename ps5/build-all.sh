#!/usr/bin/env bash
# Builds ampr-emu-ng and kraken-mount together (Linux or WSL, Docker running):
#   1. strips Windows line endings from the build scripts of both trees (never from vendor/ or
#      any binary: a .sprx edited that way no longer matches SHA256SUMS);
#   2. builds ampr-emu-ng (release + debug .sprx) unless --skip-ng;
#   3. bundles both builds into vendor/ampr-emu-ng (ps5/vendor-ampr.sh);
#   4. builds kraken-mount and copies the results to ./salida/.
#
#   ./ps5/build-all.sh ../ampr-emu-ng-main            # everything
#   ./ps5/build-all.sh ../ampr-emu-ng-main --skip-ng  # reuse the .sprx already built
set -euo pipefail

km=$(cd "$(dirname "$0")/.." && pwd)
ng=${1:-}
skip_ng=${2:-}
if [[ -z "$ng" || ! -d "$ng" ]]; then
  echo "usage: $0 <ampr-emu-ng dir> [--skip-ng]" >&2
  exit 2
fi
ng=$(cd "$ng" && pwd)

fix_scripts() {
  local f
  while IFS= read -r -d '' f; do
    if grep -q $'\r' "$f"; then
      sed -i 's/\r$//' "$f"
      echo "build-all: fixed Windows line endings in $f"
    fi
    chmod +x "$f"
  done < <(find "$1" -path "$1/target" -prune -o -path "$1/out" -prune -o -name '*.sh' -type f -print0)
}
fix_scripts "$km"
fix_scripts "$ng"

if [[ "$skip_ng" != --skip-ng ]]; then
  echo "build-all: building ampr-emu-ng in $ng"
  (cd "$ng" && bash ./ps5/build-docker.sh)
fi

bash "$km/ps5/vendor-ampr.sh" "$ng"

echo "build-all: building kraken-mount in $km"
(cd "$km" && bash ./ps5/build.sh)

out=$km/salida
mkdir -p "$out"
cp "$km/target/ps5/kraken-mount.elf" "$ng/libSceAmpr.sprx" "$out/"
[[ -f "$ng/libSceAmpr-debug.sprx" ]] && cp "$ng/libSceAmpr-debug.sprx" "$out/"
(cd "$out" && rm -f SHA256SUMS && sha256sum kraken-mount.elf libSceAmpr*.sprx > SHA256SUMS)

if grep -q "ampr_emu_status.txt" "$out/kraken-mount.elf"; then
  echo "build-all: OK: kraken-mount.elf bundles ampr-emu-ng $(cat "$km/vendor/ampr-emu-ng/VERSION")"
else
  echo "build-all: WARNING: kraken-mount.elf does not bundle a 0.5.x mejoras ampr-emu-ng" >&2
fi
if grep -q "AMPR metrics: listening" "$out/kraken-mount.elf"; then
  echo "build-all: OK: the debug ampr-emu-ng is bundled (setting ampr_debug_titles)"
else
  echo "build-all: note: no debug ampr-emu-ng bundled; ampr_debug_titles will have no effect" >&2
fi
echo "build-all: results in $out"
ls -l "$out"
