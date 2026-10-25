#!/usr/bin/env bash
# Build the Raspberry Pi 4 package on an x86-64 Linux machine.
#
#   ./scripts/build-pi4.sh
#
# Writes target/debian/lowlat-pi4.deb (under CARGO_TARGET_DIR when that is
# set). Needs Rust's aarch64 standard library
# (rustup target add aarch64-unknown-linux-gnu), the cross compiler
# (apt install gcc-aarch64-linux-gnu) and cargo-deb. CARGO names another
# cargo, such as a separate toolchain's, when the one on PATH has no aarch64
# library.
set -eu

cd "$(dirname "$0")/.."
target=aarch64-unknown-linux-gnu
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar

"${CARGO:-cargo}" build --release --target "$target" -p lowlatd
# The pi4 variant states the library versions the binary needs, rather than
# reading them from this machine's cross libraries, which are newer than the
# Pi's.
out="${CARGO_TARGET_DIR:-target}/debian/lowlat-pi4.deb"
cargo deb -p lowlatd --target "$target" --variant pi4 --no-build -o "$out"
echo "built $out"
