#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-slack.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-slack" ;;
    *) rel="x86_64-apple-darwin/gray-slack" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-slack" ;;
    *) rel="x86_64-unknown-linux-musl/gray-slack" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-slack.exe" "$here/target/release/gray-slack" "$here/target/debug/gray-slack.exe" "$here/target/debug/gray-slack"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-slack binary not found in $here" >&2
exit 1
