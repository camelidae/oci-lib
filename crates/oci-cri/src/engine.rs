// SPDX-License-Identifier: Apache-2.0

//! The container/image engine behind the CRI service.
//!
//! On Linux this is the Go shim (one c-archive holding libcontainer and
//! containers/image+storage). On macOS the CRI connection is proxied into the
//! VM guest, which runs this same trait over its Linux engine. Tests use the
//! `test-support` fake. Every operation is a JSON-shaped value so the Rust
//! service, the FFI layer, and the fake all agree on one vocabulary.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Registry credentials from a CRI `AuthConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Auth {
    /// Registry username.
    pub username: String,
    /// Registry password.
    #[serde(default)]
    pub password: String,
    /// Base64 `username:password` auth blob.
    #[serde(default)]
    pub auth: String,
    /// Registry to authenticate against.
    #[serde(default)]
    pub server_address: String,
    /// Identity token.
    #[serde(default)]
    pub identity_token: String,
}

/// One image in the local store.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineImage {
    /// Storage image ID.
    pub id: String,
    /// Tagged names pointing at the image.
    #[serde(default, deserialize_with = "null_as_default")]
    pub repo_tags: Vec<String>,
    /// Canonical digested names pointing at the image.
    #[serde(default, deserialize_with = "null_as_default")]
    pub repo_digests: Vec<String>,
    /// Image size in bytes.
    #[serde(default)]
    pub size: u64,
    /// UID of the image's configured user, when numeric.
    #[serde(default)]
    pub uid: Option<i64>,
    /// Name of the image's configured user.
    #[serde(default)]
    pub username: String,
    /// Whether the manifest is pinned in the store.
    #[serde(default)]
    pub pinned: bool,
    /// Entrypoint + command of the image config, joined for rootfs creation.
    #[serde(default)]
    pub spec: Option<ImageSpec>,
}

impl EngineImage {
    /// Match the stored identity or a name using Kubernetes image defaults.
    pub(crate) fn matches_reference(&self, requested: &str) -> bool {
        if self.id == requested {
            return true;
        }
        let normalized = crate::image_reference::normalize(requested).ok();
        self.repo_tags.iter().chain(&self.repo_digests).any(|name| {
            name == requested
                || normalized.as_ref().is_some_and(|wanted| {
                    crate::image_reference::normalize(name).as_ref().ok() == Some(wanted)
                })
        })
    }
}

/// The parts of an image config the runtime needs without an engine round trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageSpec {
    /// Image-configured user string.
    pub user: String,
    /// Exposed ports (`<port>/<proto>`).
    #[serde(default, deserialize_with = "null_as_default")]
    pub exposed_ports: BTreeMap<String, String>,
    /// Image environment (`KEY=VALUE`).
    #[serde(default, deserialize_with = "null_as_default")]
    pub env: Vec<String>,
    /// Image entrypoint.
    #[serde(default, deserialize_with = "null_as_default")]
    pub entrypoint: Vec<String>,
    /// Image default command.
    #[serde(default, deserialize_with = "null_as_default")]
    pub cmd: Vec<String>,
    /// Image volumes.
    #[serde(default, deserialize_with = "null_as_default")]
    pub volumes: BTreeMap<String, String>,
    /// Image working directory.
    pub working_dir: String,
    /// Image labels.
    #[serde(default, deserialize_with = "null_as_default")]
    pub labels: BTreeMap<String, String>,
    /// Stop signal name.
    pub stop_signal: String,
}

/// Lifecycle state of one libcontainer container.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineState {
    /// `created`, `running`, `paused`, or `stopped`.
    pub status: String,
    /// Init PID, `0` when not running.
    #[serde(default)]
    pub pid: i32,
    /// Exit code once stopped, `-1` while unknown.
    #[serde(default = "minus_one")]
    pub exit_code: i32,
    /// Whether this container's memory cgroup recorded an OOM kill.
    #[serde(default)]
    pub oom_killed: bool,
    /// Stable runtime-owned namespace paths, pinned before init exec.
    #[serde(default)]
    pub namespaces: BTreeMap<String, String>,
}

fn minus_one() -> i32 {
    -1
}

/// Captured output of a synchronous exec.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecOutput {
    /// Process exit code.
    pub exit_code: i32,
    /// Captured stdout.
    #[serde(with = "serde_bytes_b64", default)]
    pub stdout: Vec<u8>,
    /// Captured stderr.
    #[serde(with = "serde_bytes_b64", default)]
    pub stderr: Vec<u8>,
}

/// Raw counters the CRI stats replies are built from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineStats {
    /// Total CPU nanoseconds consumed by the container.
    pub cpu_total_ns: u64,
    /// System CPU nanoseconds at read time.
    pub cpu_system_ns: u64,
    /// Per-CPU nanoseconds, when available.
    #[serde(default, deserialize_with = "null_as_default")]
    pub cpu_percpu_ns: Vec<u64>,
    /// Memory usage in bytes.
    pub mem_usage: u64,
    /// Memory limit in bytes (`u64::MAX` when unlimited).
    pub mem_limit: u64,
    /// Page cache in bytes.
    pub mem_cache: u64,
    /// Swap usage in bytes.
    pub mem_swap: u64,
    /// Received bytes across interfaces.
    pub rx_bytes: u64,
    /// Transmitted bytes across interfaces.
    pub tx_bytes: u64,
    /// Received packet errors.
    pub rx_errors: u64,
    /// Transmitted packet errors.
    pub tx_errors: u64,
    /// Writable layer size in bytes.
    pub writable_size: u64,
    /// Inodes used by the writable layer.
    pub inodes_used: u64,
}

/// One filesystem in an `ImageFsInfo` reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsInfo {
    /// Nanosecond timestamp of the reading.
    pub timestamp_ns: i64,
    /// Mount point of the image store.
    pub mountpoint: String,
    /// Bytes used by the image store.
    pub used_bytes: u64,
    /// Inodes used by the image store.
    pub inodes_used: u64,
}

/// Receives container stdout/stderr chunks. `stream` is `1` or `2`.
pub type OutputSink = Arc<dyn Fn(u8, &[u8]) + Send + Sync>;

/// Supplies stdin bytes to an interactive exec. Returning an empty vec is EOF.
pub type InputSource = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// Identity resolved against the container image filesystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedUser {
    /// Effective user ID.
    pub uid: u32,
    /// Primary group ID.
    pub gid: u32,
    /// Supplementary groups, including the primary group.
    #[serde(default, rename = "additionalGids")]
    pub additional_gids: Vec<u32>,
}

/// Container and image operations. Implementations must be safe to call from
/// many threads; lifecycle calls for one container are serialised by the
/// service layer above.
pub trait Engine: Send + Sync {
    /// Pull `image` (with optional credentials) and return its canonical
    /// reference/digest string for status replies.
    fn pull_image(&self, image: &str, auth: Option<&Auth>, insecure: bool) -> Result<String>;

    /// List images, optionally filtered to one reference.
    fn list_images(&self, filter: &str) -> Result<Vec<EngineImage>>;

    /// Remove an image by ID or reference.
    fn remove_image(&self, image: &str) -> Result<()>;

    /// Filesystem usage of the image store.
    fn image_fs_info(&self) -> Result<Vec<FsInfo>>;

    /// Unpack `image` into a rootfs for container `id` and return its path.
    fn prepare_rootfs(&self, id: &str, image: &str) -> Result<PathBuf>;

    /// Release a rootfs prepared by [`Engine::prepare_rootfs`].
    fn release_rootfs(&self, id: &str) -> Result<()>;

    /// Resolve image account names and Merge/Strict supplemental groups.
    fn resolve_user(
        &self,
        _rootfs: &str,
        selected: &str,
        groups: &[u32],
        _strict: bool,
    ) -> Result<ResolvedUser> {
        let (uid, gid, _) = crate::convert::parse_user(selected);
        if selected
            .split(':')
            .any(|part| !part.is_empty() && part.parse::<u32>().is_err())
        {
            return Err(crate::Error::new(
                crate::ErrorCode::Unsupported,
                "named user resolution is unavailable",
                selected,
            ));
        }
        let mut additional_gids = groups.to_vec();
        additional_gids.push(gid);
        additional_gids.sort_unstable();
        additional_gids.dedup();
        Ok(ResolvedUser {
            uid,
            gid,
            additional_gids,
        })
    }

    /// Create (but do not start) a container from an OCI runtime spec JSON.
    fn create_container(&self, id: &str, spec_json: &str) -> Result<()>;

    /// Start a created container. `on_output` receives stdout/stderr until
    /// the matching [`Engine::wait_container`] call returns.
    fn start_container(&self, id: &str, on_output: OutputSink) -> Result<()>;

    /// Block until the container exits and return its exit code.
    fn wait_container(&self, id: &str) -> Result<i32>;

    /// Send a signal (`9`, `15`, ...) to the container init.
    fn signal_container(&self, id: &str, signal: i32) -> Result<()>;

    /// Destroy a container and its runtime state.
    fn remove_container(&self, id: &str) -> Result<()>;

    /// Current lifecycle state of a container.
    fn container_state(&self, id: &str) -> Result<EngineState>;

    /// Run a command in a container and capture its output.
    fn exec_sync(&self, id: &str, cmd: &[String], timeout_secs: i64) -> Result<ExecOutput>;

    /// Run a command in a container interactively: output goes to
    /// `on_output`, stdin comes from `next_input` (empty means EOF).
    fn exec_stream(
        &self,
        id: &str,
        cmd: &[String],
        tty: bool,
        next_input: InputSource,
        on_output: OutputSink,
    ) -> Result<i32>;

    /// Write bytes (or EOF when empty) to a running container's stdin.
    fn write_stdin(&self, id: &str, data: &[u8]) -> Result<()>;

    /// Resize a container's terminal.
    fn resize_tty(&self, id: &str, width: u16, height: u16) -> Result<()>;

    /// Live resource counters for a container.
    fn container_stats(&self, id: &str) -> Result<EngineStats>;

    /// Apply updated cgroup resources (OCI Linux resources JSON fragment).
    fn update_resources(&self, id: &str, resources_json: &str) -> Result<()>;

    /// Checkpoint a container into an image.
    fn checkpoint(&self, id: &str, image: &str, parent: &str) -> Result<()>;

    /// Serve an authenticated, single-use SPDY session on an accepted socket.
    fn serve_spdy(
        &self,
        _socket: std::net::TcpStream,
        _header: &[u8],
        _session: &crate::streaming::SessionKind,
        _log_path: Option<&str>,
    ) -> Result<()> {
        Err(crate::error::Error::new(
            crate::error::ErrorCode::Unsupported,
            "SPDY streaming is unavailable for this engine",
            "",
        ))
    }

    /// Multi-line human-readable readiness report.
    fn diagnose(&self) -> Result<String>;
}

pub(crate) mod serde_bytes_b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(de)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

// Go encodes nil slices and maps as JSON null. Treat them like absent
// collections at this boundary, while retaining validation of non-null values.
fn null_as_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    #[test]
    fn image_config_accepts_go_nil_collections() {
        let image: EngineImage = serde_json::from_value(serde_json::json!({
            "id": "pause", "repo_tags": null, "repo_digests": null, "size": 10,
            "spec": {
                "user": "", "env": null, "entrypoint": ["/pause"], "cmd": null,
                "exposed_ports": null, "volumes": null, "labels": null,
                "working_dir": "", "stop_signal": ""
            }
        }))
        .unwrap();
        assert!(image.repo_tags.is_empty());
        let spec = image.spec.unwrap();
        assert_eq!(spec.entrypoint, ["/pause"]);
        assert!(spec.cmd.is_empty());
        assert!(spec.env.is_empty());
        assert!(spec.labels.is_empty());
    }

    #[test]
    fn image_config_rejects_wrong_collection_types() {
        let mut spec = serde_json::to_value(ImageSpec::default()).unwrap();
        spec["cmd"] = serde_json::json!("not an array");
        assert!(serde_json::from_value::<ImageSpec>(spec).is_err());
    }
}
