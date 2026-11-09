#!/usr/bin/env bash
# Build (optional) a Linux oci-cri agent and wire diorama-vm Image/runsc into
# crates/oci-cri/guest/out for the existing macOS embed path.
#
# Artifact sources (first match wins):
#   1. DIORAMA_VM/artifacts/{Image,runsc} when DIORAMA_VM points at a checkout
#   2. GitHub release camelidae/diorama-vm (DIORAMA_VM_RELEASE=vX.Y.Z or latest)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$ROOT/crates/oci-cri/guest/out}"
BUILD_AGENT="${BUILD_AGENT:-1}"
IMAGE="${IMAGE:-docker.io/library/rust:bookworm}"
GO_IMAGE="${GO_IMAGE:-docker.io/library/golang:1.27-bookworm}"
DIORAMA_VM_REPO="${DIORAMA_VM_REPO:-camelidae/diorama-vm}"

# Podman machine on macOS only mounts $HOME by default. Stage under ~/Library
# when the checkout lives on an external volume.
STAGE="${STAGE:-$HOME/Library/Caches/oci-lib-wire}"
mkdir -p "$STAGE"
FEED="$STAGE/diorama-vm-artifacts"
mkdir -p "$FEED"

resolve_diorama_vm_feed() {
  if [[ -n "${DIORAMA_VM:-}" ]]; then
    if [[ -f "$DIORAMA_VM/artifacts/Image" && -f "$DIORAMA_VM/artifacts/runsc" ]]; then
      echo "using local diorama-vm artifacts from $DIORAMA_VM/artifacts"
      cp -f "$DIORAMA_VM/artifacts/Image" "$FEED/Image"
      cp -f "$DIORAMA_VM/artifacts/runsc" "$FEED/runsc"
      if [[ -f "$DIORAMA_VM/artifacts/SHA256SUMS" ]]; then
        cp -f "$DIORAMA_VM/artifacts/SHA256SUMS" "$FEED/SHA256SUMS"
      fi
      return 0
    fi
    echo "DIORAMA_VM=$DIORAMA_VM set but artifacts/Image or artifacts/runsc missing" >&2
    echo "Build them with: make -C \"$DIORAMA_VM\" kernel runsc" >&2
    exit 1
  fi

  local tag="${DIORAMA_VM_RELEASE:-}"
  if [[ -z "$tag" || "$tag" == "latest" ]]; then
    tag="$(curl -fsSL "https://api.github.com/repos/${DIORAMA_VM_REPO}/releases/latest" \
      | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')"
  fi
  local base="https://github.com/${DIORAMA_VM_REPO}/releases/download/${tag}"
  echo "downloading diorama-vm ${tag} feed from ${DIORAMA_VM_REPO} ..."
  curl -fsSL -o "$FEED/Image" "${base}/Image"
  curl -fsSL -o "$FEED/runsc" "${base}/runsc"
  curl -fsSL -o "$FEED/SHA256SUMS" "${base}/SHA256SUMS"
  (
    cd "$FEED"
    # SHA256SUMS may list extra files; only verify the blobs we need.
    awk '/ (Image|runsc)$/ {print}' SHA256SUMS | sha256sum -c -
  )
  chmod +x "$FEED/runsc"
  echo "$tag" >"$FEED/RELEASE"
}

resolve_diorama_vm_feed

if [[ ! -f "$OUT/initramfs" || ! -f "$OUT/attestation.json" ]]; then
  echo "base oci-cri guest missing under $OUT (need initramfs + attestation.json)" >&2
  echo "Produce it with: cargo xtask guest oci-cri  (on Linux)" >&2
  exit 1
fi

echo "staging oci-lib sources into $STAGE ..."
rsync -a --delete \
  --exclude target \
  --exclude .git \
  --exclude 'crates/*/guest/out/*.zst' \
  "$ROOT/" "$STAGE/oci-lib/"
mkdir -p "$STAGE/oci-lib/target/linux-guest"
# Keep the current guest/out as the repack base.
mkdir -p "$STAGE/oci-lib/crates/oci-cri/guest/out"
cp -f "$OUT/initramfs" "$OUT/attestation.json" "$OUT/vmlinuz" \
  "$STAGE/oci-lib/crates/oci-cri/guest/out/" 2>/dev/null || \
  cp -f "$OUT/initramfs" "$OUT/attestation.json" "$STAGE/oci-lib/crates/oci-cri/guest/out/"

AGENT_OUT="$STAGE/oci-lib/target/linux-guest/oci-cri"
AGENT_ARGS=()
if [[ "$BUILD_AGENT" == "1" ]]; then
  echo "building Linux oci-cri agent in podman ($IMAGE + $GO_IMAGE)..."
  GO_CACHE="$STAGE/go"
  if [[ ! -x "$GO_CACHE/bin/go" ]]; then
    mkdir -p "$GO_CACHE"
    cid="$(podman create --arch arm64 "$GO_IMAGE")"
    podman cp "$cid:/usr/local/go/." "$GO_CACHE/"
    podman rm "$cid" >/dev/null
  fi
  podman run --rm \
    --arch arm64 \
    -v "$STAGE/oci-lib:/src:z" \
    -v "$GO_CACHE:/usr/local/go:ro" \
    -w /src \
    -e CARGO_HOME=/src/target/podman-cargo \
    -e CARGO_TARGET_DIR=/src/target/podman-target \
    -e ROK_DISABLE_OPTIONAL_LIBS=1 \
    "$IMAGE" \
    bash -c '
      set -euo pipefail
      export PATH="/usr/local/go/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
      export ROK_DISABLE_OPTIONAL_LIBS=1
      apt-get update -qq
      DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        pkg-config protobuf-compiler gcc g++ git make
      go version
      # Skip libseccomp so the guest agent does not need libseccomp.so.2 in the
      # initramfs (diorama-vm kernels also omit it). runsc remains the runtime.
      cargo build -p oci-cri --release
      install -D /src/target/podman-target/release/oci-cri /src/target/linux-guest/oci-cri
      # Ensure we did not pick up a dynamic seccomp dependency.
      if ldd /src/target/linux-guest/oci-cri 2>/dev/null | grep -q libseccomp; then
        echo "oci-cri still links libseccomp; aborting" >&2
        exit 1
      fi
    '
  AGENT_ARGS=(--agent "$AGENT_OUT")
elif [[ -f "$AGENT_OUT" ]]; then
  AGENT_ARGS=(--agent "$AGENT_OUT")
  echo "reusing existing agent at $AGENT_OUT"
else
  echo "BUILD_AGENT=0 and no agent at $AGENT_OUT; wiring kernel/runsc/init only" >&2
  echo "(external runtime shim will be inactive until the agent is rebuilt)" >&2
fi

python3 "$STAGE/oci-lib/scripts/wire-diorama-vm-guest.py" \
  --source-dir "$STAGE/oci-lib/crates/oci-cri/guest/out" \
  --output-dir "$STAGE/oci-lib/crates/oci-cri/guest/out" \
  --diorama-vm "$STAGE" \
  --kernel "$FEED/Image" \
  --runsc "$FEED/runsc" \
  "${AGENT_ARGS[@]}"

mkdir -p "$OUT"
cp -f \
  "$STAGE/oci-lib/crates/oci-cri/guest/out/vmlinuz" \
  "$STAGE/oci-lib/crates/oci-cri/guest/out/initramfs" \
  "$STAGE/oci-lib/crates/oci-cri/guest/out/attestation.json" \
  "$OUT/"
if [[ -f "$AGENT_OUT" ]]; then
  mkdir -p "$ROOT/target/linux-guest"
  cp -f "$AGENT_OUT" "$ROOT/target/linux-guest/oci-cri"
fi

echo "guest ready at $OUT:"
ls -lh "$OUT/vmlinuz" "$OUT/initramfs" "$OUT/attestation.json"
