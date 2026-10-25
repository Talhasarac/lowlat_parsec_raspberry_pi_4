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

# **The builder's folders do not ship.** Source paths are compiled into the
# binary for panic messages, absolute for every dependency, so the cargo home,
# the toolchain and this checkout are renamed to fixed stand-ins; without this
# the package carries the builder's user name and directory layout.
root=$(pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$root=/lowlat"
export CFLAGS_aarch64_unknown_linux_gnu="-ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$root=/lowlat"

"${CARGO:-cargo}" build --release --target "$target" -p lowlatd
# The pi4 variant states the library versions the binary needs, rather than
# reading them from this machine's cross libraries, which are newer than the
# Pi's.
out="${CARGO_TARGET_DIR:-target}/debian/lowlat-pi4.deb"
cargo deb -p lowlatd --target "$target" --variant pi4 --no-build -o "$out"
echo "built $out"
