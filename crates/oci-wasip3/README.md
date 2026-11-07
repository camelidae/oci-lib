# WASI 0.3 commands in oci-cri

`oci-wasip3` is a separate executable using Wasmtime 50.0.1. It accepts a component
path followed by arguments. It hosts `wasi:cli/run@0.3.0` only: core modules,
Preview 1, Preview 2 and incoming HTTP handlers are unsupported. Wasmtime is not
linked into oci-cri. No containerd daemon or Youki library is needed.

oci-cri supplies its existing libcontainer isolation, namespaces, mounts,
credentials, networking and cgroups. The runner inherits argv, environment and
standard streams and preopens the isolated root filesystem. It is not itself a
sandbox: do not execute untrusted components directly on the host.

## Build and install

On Linux amd64 or arm64, install Rust >=1.97, musl-gcc and binutils, then run:

```sh
sh scripts/build-wasip3.sh
install -Dm755 target/release/oci-wasip3 /usr/local/bin/oci-wasip3
install -Dm644 crates/oci-cri/config.d/wasmtime.toml /etc/oci-cri/config.d/wasmtime.toml
```

The build requires a static ELF executable because a WASI image does not contain
a Linux dynamic loader. The runner must match the host architecture; the component
image remains `wasip3/wasm32` on either host. Linux oci-cri release archives include
the runner and configuration alongside oci-cri; install them to the paths above.

`cargo xtask guest oci-cri` bundles the same static runner and TOML in the BusyBox
initramfs embedded in the macOS arm64 release. The guest builder copies oci-cri's
own shared libraries separately. The runtime is available without host Wasmtime.

## Handler configuration

The default drop-in directory is `/etc/oci-cri/config.d`, overridable with
`oci-cri serve --runtime-config-dir DIRECTORY`:

```toml
[runtimes.wasmtime]
runtime_path = "/usr/local/bin/oci-wasip3"
platform = "wasip3/wasm32"
```

These are executable handlers, not containerd shim-v2 plugins. The named handler
selects the OCI platform for pulls, and prefixes the component command with the
read-only mounted runner. Empty handlers use the native runtime. Unknown handlers,
missing executables and unsupported platforms fail explicitly. CRI Status only
advertises successfully configured handlers. Restart the daemon after changing
configuration.

Use `runtimeClassName: wasmtime` in Kubernetes. Diorama registers the RuntimeClass
when the connected CRI runtime advertises `wasmtime`. Single-manifest images and
indices (including mixed Linux/WASI indices) are supported. CRI clients must send
`ImageSpec.runtime_handler` for both PullImage and ImageStatus and use the returned
immutable image identity for CreateContainer. Older crictl pull commands omit this
field; they cannot select the WASI entry in a mixed index.

## Live verification

Publish the fixtures using Diorama's `container/wasip3/registry.py`, start a
privileged Linux oci-cri daemon with a delegated cgroup and the configuration above,
then run the test inside that Linux environment:

```sh
CRI_SOCKET=/run/oci-cri.sock P3_REPOSITORY=localhost:5000/p3-probe \
  cargo test -p oci-cri --test wasip3_live -- --ignored --nocapture
```

The test pulls/runs single, indexed, and mixed WASI tags, runs the native entry
from the mixed tag, then switches back to WASI. It checks exit statuses 0 and 7 and logs
with a 128 MiB memory limit. Its host-network sandboxes avoid requiring CNI for
this image-selection test. Run only against a disposable daemon.

The same live test also runs from macOS against the embedded guest. Set
`CRI_SOCKET` to the host proxy socket, `CRI_LOG_ROOT=/mnt/runroot` and
`CRI_HOST_LOG_ROOT` to the host `--state-root` directory. The guest registry must
use trusted TLS. Local verification used a disposable TLS registry with a test
CA installed in that guest's per-registry trust directory, without modifying
host trust. The arm64 guest passed all image-selection cases and native execution.

The guest's initramfs root may use tmpfs or ramfs. oci-cri selects libcontainer's
no-pivot-root path for either, since Linux cannot pivot away from rootfs.

## Verified locally

| Check | arm64 | amd64 |
| --- | --- | --- |
| `scripts/build-wasip3.sh` static ELF | verified (`readelf` shows no `INTERP`) | verified via `cargo zigbuild --target x86_64-unknown-linux-musl` when Podman could not pull a `linux/amd64` builder (disk full) |
| P3 probe, exit 7 | live `wasip3_live` + macOS guest | P3 + exit 7 under `qemu-x86_64-static` on an arm64 Debian test container |
| P1 rejection | verified | verified (same qemu setup) |

Published **v0.1.14** release archives do not include this crate yet. Ship a new
oci-lib tag after merging `feat/wasip3-runtime-plugin`, then bump Diorama's git pin
and npm `OCI_LIB_VERSION` together. Linux release tarballs and the macOS guest
build both expect `oci-wasip3` and `config.d/wasmtime.toml` beside `oci-cri`.
