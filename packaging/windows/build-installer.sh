#!/usr/bin/env bash
# Cross-compiles linter for 64-bit Windows and builds the installer and a
# portable zip in target/windows/.
#
# Needs: the Rust target (rustup target add x86_64-pc-windows-gnu), the
# MinGW-w64 toolchain (x86_64-w64-mingw32-gcc, -windres, -strip) and NSIS
# (makensis). On Fedora: dnf install mingw64-gcc mingw32-nsis
set -euo pipefail

cd "$(dirname "$0")/../.."
target=x86_64-pc-windows-gnu
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
out="$PWD/target/windows"
stage="$out/stage"

for tool in x86_64-w64-mingw32-gcc x86_64-w64-mingw32-windres x86_64-w64-mingw32-strip makensis zip; do
    command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

cargo build --release --target "$target"

rm -rf "$stage"
mkdir -p "$stage/examples"
cp "target/$target/release/linter.exe" "$stage/"
x86_64-w64-mingw32-strip "$stage/linter.exe"
cp README.md "$stage/"
cp examples/sample.md examples/sample.png "$stage/examples/"

setup="$out/linter-$version-setup.exe"
makensis -V2 -DVERSION="$version" -DSTAGE="$stage" -DOUTFILE="$setup" -DICON="$PWD/assets/linter.ico" \
    packaging/windows/installer.nsi

portable="$out/linter-$version-windows-x64.zip"
rm -f "$portable"
(cd "$stage" && zip -qr9 "$portable" .)

echo "installer: $setup ($(du -h "$setup" | cut -f1))"
echo "portable:  $portable ($(du -h "$portable" | cut -f1))"
