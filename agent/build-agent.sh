#!/usr/bin/env bash
# Build the Rust sysmon agent for the Spark nodes.
#
# On x86_64 the binary is cross-compiled in a container so that the result is
# linked against an older glibc than the target systems have (bookworm has 2.36,
# the Sparks run Ubuntu 24.04 with 2.39). On aarch64 hosts it builds natively.
#
# Usage: ./build-agent.sh
# Result: agent/rust/dist/megatron-sysmon-agent-aarch64 (static-ish, ~0.5 MB)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="${HERE}/rust"
DIST_DIR="${RUST_DIR}/dist"
TARGET="aarch64-unknown-linux-gnu"
X86_TARGET="x86_64-unknown-linux-gnu"
RUST_IMAGE="${RUST_IMAGE:-rust:1.90-bookworm}"
OUTPUT="${DIST_DIR}/megatron-sysmon-agent-aarch64"
X86_OUTPUT="${DIST_DIR}/megatron-sysmon-agent-x86_64"

mkdir -p "${DIST_DIR}"

if [ "$(uname -m)" = "aarch64" ]; then
    echo "building natively for aarch64"
    cargo build --release --manifest-path "${RUST_DIR}/Cargo.toml"
    cp "${RUST_DIR}/target/release/megatron-sysmon-agent" "${OUTPUT}"
else
    command -v docker >/dev/null || {
        echo "error: docker is required for the aarch64 cross build" >&2
        exit 1
    }
    echo "cross-compiling for ${TARGET} in ${RUST_IMAGE}"
    docker run --rm -v "${RUST_DIR}":/w -w /w "${RUST_IMAGE}" bash -c "
        set -e
        apt-get update -qq >/dev/null 2>&1
        apt-get install -y -qq gcc-aarch64-linux-gnu >/dev/null 2>&1
        rustup target add ${TARGET} >/dev/null 2>&1
        mkdir -p .cargo
        printf '[target.${TARGET}]\nlinker = \"aarch64-linux-gnu-gcc\"\n' > .cargo/config.toml
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
            cargo build --release --target ${TARGET}
        cp target/${TARGET}/release/megatron-sysmon-agent dist/megatron-sysmon-agent-aarch64

        # The same recipe builds the x86_64 variant for hosts like desktop-01.
        rustup target add ${X86_TARGET} >/dev/null 2>&1
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=cc \
            cargo build --release --target ${X86_TARGET}
        cp target/${X86_TARGET}/release/megatron-sysmon-agent dist/megatron-sysmon-agent-x86_64
    "
fi

# Remember the version that was just built: the applet compares it with the
# version each agent reports.
AGENT_VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "${RUST_DIR}/Cargo.toml" | head -1)"
if [ -n "${AGENT_VERSION}" ]; then
    printf '%s\n' "${AGENT_VERSION}" > "${DIST_DIR}/agent-version.txt"
    echo "agent version: ${AGENT_VERSION}"
fi

for candidate in "${OUTPUT}" "${X86_OUTPUT}"; do
    [ -f "${candidate}" ] || continue
    file "${candidate}"
    ls -la "${candidate}"
    sha256sum "${candidate}"
done
