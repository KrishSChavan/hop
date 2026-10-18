#!/bin/bash
# Stands in for MSVC's lib.exe when type-checking for Windows from macOS:
# cargo check never links, so ring's static library only has to exist.
out=""
objs=()
for arg in "$@"; do
  case "$arg" in
    -out:*|/OUT:*|/out:*) out="${arg#*:}" ;;
    -*|/*nologo*) ;;
    *) objs+=("$arg") ;;
  esac
done
[ -n "$out" ] || { echo "lib-shim: no -out: argument" >&2; exit 1; }
rm -f "$out"
exec /usr/bin/ar crs "$out" "${objs[@]}"
