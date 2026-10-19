#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-videogen.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-videogen" ;;
    *) rel="x86_64-apple-darwin/gray-videogen" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-videogen" ;;
    *) rel="x86_64-unknown-linux-musl/gray-videogen" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-videogen.exe" "$here/target/release/gray-videogen" "$here/target/debug/gray-videogen.exe" "$here/target/debug/gray-videogen"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-videogen binary not found in $here" >&2
exit 1
