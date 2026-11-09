# `oci-cri` — vendor-agnostic CRI v1 runtime

`oci-cri` is a [Container Runtime Interface](https://kubernetes.io/docs/concepts/architecture/cri/)
(CRI v1) runtime: kubelet talks CRI over a unix socket to this daemon instead
of containerd, dockerd, or CRI-O. Containers run with libcontainer (via an
in-process Go c-archive); images come from containers/image + containers/storage.

```
kubelet (--container-runtime-endpoint / --cri-socket)
  └── oci-cri          CRI RuntimeService + ImageService (this crate)
        └── Go shim    libcontainer + containers/image & storage
```

## Quick start (Linux)

```sh
cargo build -p oci-cri
sudo target/debug/oci-cri diagnose
sudo target/debug/oci-cri serve \
  --socket /run/oci-cri/oci-cri.sock \
  --root /var/lib/oci-cri \
  --sandbox-image registry.k8s.io/pause:3.10.1 \
  --cni-bin-dir /opt/cni/bin \
  --cni-conf-dir /etc/cni/net.d
```

On cgroup v2, provide a writable delegated domain hierarchy. In a private
cgroup namespace, the root must be empty or contain only the runtime process.
On first container creation the runtime moves itself into an
`oci-cri-runtime-<pid>` leaf, then enables controllers for sibling workload
cgroups. It rejects shared or already threaded roots before libcontainer can
convert the hierarchy to threaded mode. Run a containerized daemon directly as
PID 1; run diagnostic clients outside its cgroup. Existing diorama host setup
can supply an empty root with the runtime already in its `init` leaf.

Point any CRI-capable kubelet at the socket (see
[`examples/kubelet/`](examples/kubelet/)):

```sh
# kubeadm / vanilla
kubelet --container-runtime-endpoint=unix:///run/oci-cri/oci-cri.sock ...

# crictl
crictl --runtime-endpoint unix:///run/oci-cri/oci-cri.sock info
```

## Library usage

```rust,ignore
use oci_cri::{Config, startup};
use oci_cri::server;

fn main() -> Result<(), oci_cri::Error> {
    startup()?;
    let config = Config {
        runtime_name: "my-runtime".into(),
        socket: "/run/my-runtime/cri.sock".into(),
        root: "/var/lib/my-runtime".into(),
        state_root: "/run/my-runtime/state".into(),
        ..Config::default()
    };
    server::serve(config)
}
```

Defaults use the neutral `oci-cri` layout (`/run/oci-cri/...`). Override
`runtime_name` and paths for product branding.

For Linux embedding into an existing async executor, call
`server::serve_until(config, shutdown_future).await` after early `startup()`.
The shutdown future resolves to `()`. The server drains in-flight CRI RPCs,
closes its streaming listener, and removes its socket. It does not stop
workload containers; the orchestrator must apply its workload shutdown policy
first. `serve_with_engine_until` provides the same lifecycle for an explicit
engine, including the test engine. Streaming bind errors fail startup before
the CRI socket becomes visible. A supervisor should bound the shutdown wait
because outstanding RPCs may take time to drain.

## Surface

CRI v1 services: pod/container lifecycle, Exec/Attach/PortForward streaming,
CRI logs, stats, ImageService (pull/list/status/remove/`ImageFsInfo`), and
CNI networking via `--cni-bin-dir` / `--cni-conf-dir`.

Streaming accepts Kubernetes SPDY/3.1 and WebSocket upgrades with single-use
session tokens. Exec timeouts kill and reap the command before returning.
User namespaces remain unsupported and are advertised as such; requests for
mapped user namespaces fail explicitly. Terminal streams currently use pipes;
PTY allocation and terminal resizing remain incomplete.

Run the native regression and full upstream cri-tools suite in a disposable
Linux container with [`scripts/native-cri-conformance.sh`](../../scripts/native-cri-conformance.sh):

```sh
scripts/native-cri-conformance.sh target/debug/oci-cri /tmp/cri-native-evidence
```

Set `CONTAINER_ENGINE=podman` to use Podman. This fixture supplies CNI, shares
the required namespaces with its test client, preserves logs/JUnit results,
and returns a failure if any regression or conformance test fails. Tests for
unsupported optional features or unavailable host security modules may skip.
`cri_lifecycle` / `streaming_exec` exercise the protocol with a fake engine.

## Distro notes

| Distro | Typical wiring |
| --- | --- |
| kubeadm / vanilla | `--container-runtime-endpoint=unix://…` |
| k3s / RKE2 | Disable embedded containerd; point the agent at this socket |
| MicroK8s | Override snap paths / confinement |
| OpenShift | Needs extra SELinux/SCC work beyond this crate today |

Supply CNI plugins and a pause image on disk before the first pod.

Seccomp `RuntimeDefault` uses the maintained containers-common profile.
`Localhost` accepts an absolute node path to a valid seccomp JSON profile;
legacy `runtime/default`, `docker/default`, and `localhost/<absolute-path>`
selections are supported. Filters apply to both init and exec. Missing or
malformed profiles, and confined requests on builds without libseccomp, fail
explicitly. Unspecified and `Unconfined` profiles add no filter. Privileged
containers follow Kubernetes semantics and ignore seccomp and AppArmor profiles.

The native [seccomp regression](../../scripts/repro-seccomp.sh) denies a syscall
in both init and exec, checks unconfined/privileged controls and RuntimeDefault,
and rejects invalid profiles. The [AppArmor regression](../../scripts/repro-apparmor.sh)
checks denied writes in init and exec, with unconfined and privileged controls.
AppArmor requires host kernel support and `apparmor_parser`; requested profiles
fail explicitly when unavailable. The resolved profile is passed to each
libcontainer process because its OCI configuration converter omits that field.

## macOS

Without `vm`, macOS links the engine stub (`ErrorCode::Unsupported`).

With `--features vm`, macOS boots an embedded Linux guest via
[`oci_util::vm`](../oci-util) and proxies each host CRI unix connection over
virtio-vsock (port 5254, banner `ROKCRI1`) to `oci-cri --engine-serve` inside
the guest. Build the guest on Linux with `cargo xtask guest oci-cri` (or let
the release download cache supply images once published). Until those artifacts
exist, keep `vm` opt-in so plain workspace builds stay green.

To feed a [diorama-vm](https://github.com/camelidae/diorama-vm) kernel and
gVisor `runsc` into the existing embed path (no host protocol changes):

```sh
# Download Image + runsc from the latest camelidae/diorama-vm GitHub release:
./scripts/wire-diorama-vm-guest.sh

# Or pin a release / use a local checkout:
# DIORAMA_VM_RELEASE=v0.1.0 ./scripts/wire-diorama-vm-guest.sh
# DIORAMA_VM=/path/to/diorama-vm BUILD_AGENT=0 ./scripts/wire-diorama-vm-guest.sh
```

That script rebuilds the Linux `oci-cri` agent (unless `BUILD_AGENT=0`), embeds
`/bin/runsc`, swaps in the diorama-vm `Image`, refreshes `attestation.json`, and
writes `crates/oci-cri/guest/out/`. Prefer a local `DIORAMA_VM` checkout when
present; otherwise it downloads from `camelidae/diorama-vm` releases
(`DIORAMA_VM_RELEASE`, default `latest`). The oci-lib release workflow does the
same via `ROK_GUEST_KERNEL_SRC` / `ROK_RUNSC` during `cargo xtask guest`.
Guest init sets `ROK_OCI_RUNTIME=/bin/runsc` so the shim drives containers
through runsc instead of libcontainer. Kernels without `veth` skip guest CNI
(sandboxes share the guest netns).

MVP limits: guest CNI needs a kernel with `veth`/bridge; without it sandboxes
share the guest netns. Exec/Attach streaming URLs bind the guest NAT address.

## Container startup and DNS

Image names use Kubernetes defaults: `busybox:1.36` resolves to
`docker.io/library/busybox:1.36`, and an omitted tag means `latest`.
Pull, status, list filters, container creation, and removal use the same
normalization; digests take precedence over tags and local image IDs remain
unchanged. Explicit registries are preserved. Short names do not use Podman's
interactive aliases or search registries, so a node does not require
`/etc/containers/registries.conf` to pull them. Registry mirror and TLS policy
still apply to the resulting fully qualified reference.

Args-only CRI requests retain the image entrypoint; an explicit command replaces
it. Unprivileged workloads start with the containers-common default capabilities
(CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, NET_BIND_SERVICE, SETFCAP, SETGID,
SETPCAP, SETUID, SYS_CHROOT). CRI additions and drops accept names with or without
`CAP_`; `drop: ALL` clears defaults before explicit additions. Ambient requests
are handled separately. Defaults do not include SYS_ADMIN or NET_RAW.

Sandbox DNS settings are mounted from runtime-owned resolver files into each
workload, rather than shadowed by the host resolver. Failed initial starts clean
up libcontainer state before a retry, retaining the original error.

See [the CoreDNS regression guide](../../docs/repros/coredns-start.md) and
[`scripts/repro-coredns-start.sh`](../../scripts/repro-coredns-start.sh) for the
native regression gate. The script preserves diagnostics and exits nonzero on
startup, retry, or removal failures.
