#!/bin/sh
# The runner is mounted into WASI root filesystems, which have no dynamic loader.
set -eu
case "$(uname -m)" in
    aarch64|arm64) target=aarch64-unknown-linux-musl ;;
    x86_64) target=x86_64-unknown-linux-musl ;;
    *) echo 'unsupported WASI host architecture' >&2; exit 1 ;;
esac
rustup target add "$target"
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
    cargo build --release --locked -p oci-wasip3 --target "$target"
bin="${CARGO_TARGET_DIR:-target}/$target/release/oci-wasip3"
if readelf -l "$bin" | grep -q INTERP; then
    echo 'oci-wasip3 must be statically linked' >&2
    exit 1
fi
mkdir -p "${CARGO_TARGET_DIR:-target}/release"
cp "$bin" "${CARGO_TARGET_DIR:-target}/release/oci-wasip3"
