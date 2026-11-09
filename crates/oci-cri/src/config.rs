// SPDX-License-Identifier: Apache-2.0

//! Runtime configuration for the CRI daemon.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Neutral default name kubelet sees when no product branding is applied.
pub const RUNTIME_NAME: &str = "oci-cri";
/// CRI API version implemented by this daemon.
pub const API_VERSION: &str = "v1";

/// Everything the daemon needs, from CLI flags or a config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Directory of runtime TOML drop-ins. Missing directories are allowed.
    pub runtime_config_dir: Option<PathBuf>,
    /// Explicit handlers; the empty handler always retains native execution.
    pub runtimes: std::collections::BTreeMap<String, crate::runtime_plugin::RuntimePlugin>,
    /// Name kubelet sees in `Version` and `Status` replies.
    pub runtime_name: String,
    /// Unix socket the CRI gRPC server listens on.
    pub socket: PathBuf,
    /// Root for the image store, sandbox/container state, and rootfses.
    pub root: PathBuf,
    /// Runtime state directory passed to libcontainer.
    pub state_root: PathBuf,
    /// Host directories exported into the macOS Linux guest at the same
    /// absolute paths. Ignored on Linux.
    pub host_shares: Vec<PathBuf>,
    /// Pause/infra image run as every pod sandbox.
    pub sandbox_image: String,
    /// Directory holding CNI plugin binaries. Empty disables CNI (sandboxes
    /// share the host network namespace).
    pub cni_bin_dir: Option<PathBuf>,
    /// Directory holding CNI configuration (`.conf`/`.conflist` files).
    pub cni_conf_dir: Option<PathBuf>,
    /// Extra CNI plugin search directories appended to `cni_bin_dir`.
    pub cni_plugin_dirs: Vec<PathBuf>,
    /// Address the Exec/Attach/PortForward streaming server binds.
    pub streaming_address: String,
    /// Port the streaming server binds. `0` picks a free port; the actual
    /// port only appears in the URLs returned to kubelet.
    pub streaming_port: u16,
    /// Pod CIDR reported back through `UpdateRuntimeConfig` handling. May be
    /// empty when the cluster tooling does not supply one.
    pub pod_cidr: String,
    /// Allow HTTP registries / skipped TLS verification when pulling.
    pub insecure_registries: bool,
}

/// Marker file under `Config::state_root` (guest `/mnt/runroot`) that enables
/// HTTP / skip-TLS image pulls. The macOS host writes this from
/// `Config::insecure_registries` before booting the guest.
pub const INSECURE_REGISTRIES_MARKER: &str = "insecure-registries";

/// Write or remove [`INSECURE_REGISTRIES_MARKER`] under `state_root`.
///
/// The macOS host calls this before booting the guest. The guest image pull
/// path still defaults insecure pulls on, because the in-cluster registry is
/// plain HTTP; the marker records the host's choice for that boot.
pub fn sync_insecure_registries_marker(state_root: &Path, enabled: bool) -> std::io::Result<()> {
    let path = state_root.join(INSECURE_REGISTRIES_MARKER);
    if enabled {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, b"1\n")
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            runtime_config_dir: Some(PathBuf::from("/etc/oci-cri/config.d")),
            runtimes: Default::default(),
            runtime_name: RUNTIME_NAME.to_string(),
            socket: PathBuf::from("/run/oci-cri/oci-cri.sock"),
            root: PathBuf::from("/var/lib/oci-cri"),
            state_root: PathBuf::from("/run/oci-cri/state"),
            host_shares: Vec::new(),
            sandbox_image: "registry.k8s.io/pause:3.10.1".to_string(),
            cni_bin_dir: None,
            cni_conf_dir: None,
            cni_plugin_dirs: Vec::new(),
            streaming_address: "127.0.0.1".to_string(),
            streaming_port: 0,
            pod_cidr: String::new(),
            insecure_registries: false,
        }
    }
}

impl Config {
    /// Directory holding persisted sandbox records.
    pub fn sandboxes_dir(&self) -> PathBuf {
        self.root.join("sandboxes")
    }

    /// Directory holding persisted container records.
    pub fn containers_dir(&self) -> PathBuf {
        self.root.join("containers")
    }

    /// Directory holding unpacked container rootfses.
    pub fn rootfs_dir(&self) -> PathBuf {
        self.root.join("rootfs")
    }

    /// Socket address for the streaming server.
    pub fn streaming_socket_addr(&self) -> Result<SocketAddr, crate::Error> {
        format!("{}:{}", self.streaming_address, self.streaming_port)
            .parse()
            .map_err(|err| {
                crate::Error::new(
                    crate::ErrorCode::InvalidArgument,
                    format!("streaming address: {err}"),
                    "",
                )
            })
    }

    /// Validate fields that must be usable before the daemon starts.
    pub fn validate(&self) -> Result<(), crate::Error> {
        if self.runtime_name.is_empty() {
            return Err(crate::Error::new(
                crate::ErrorCode::InvalidArgument,
                "runtime_name is empty",
                "",
            ));
        }
        if self.socket.as_os_str().is_empty() {
            return Err(crate::Error::new(
                crate::ErrorCode::InvalidArgument,
                "socket path is empty",
                "",
            ));
        }
        if self.sandbox_image.is_empty() {
            return Err(crate::Error::new(
                crate::ErrorCode::InvalidArgument,
                "sandbox image is empty",
                "",
            ));
        }
        for path in &self.host_shares {
            let Some(text) = path.to_str() else {
                return Err(crate::Error::new(
                    crate::ErrorCode::InvalidArgument,
                    "host share path must be valid UTF-8",
                    "",
                ));
            };
            if !path.is_absolute()
                || text
                    .chars()
                    .any(|character| matches!(character, '\t' | '\n' | '\r'))
            {
                return Err(crate::Error::new(
                    crate::ErrorCode::InvalidArgument,
                    format!(
                        "host share path must be absolute and contain no tabs or newlines: {}",
                        path.display()
                    ),
                    "",
                ));
            }
        }
        self.streaming_socket_addr()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_neutral_layout() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.runtime_name, RUNTIME_NAME);
        assert_eq!(config.socket, PathBuf::from("/run/oci-cri/oci-cri.sock"));
        assert!(config.sandbox_image.contains("pause"));
    }

    #[test]
    fn rejects_an_empty_sandbox_image() {
        let config = Config {
            sandbox_image: String::new(),
            ..Config::default()
        };
        assert_eq!(
            config.validate().unwrap_err().code(),
            crate::ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn rejects_empty_runtime_name_and_socket() {
        let name = Config {
            runtime_name: String::new(),
            ..Config::default()
        };
        assert_eq!(
            name.validate().unwrap_err().code(),
            crate::ErrorCode::InvalidArgument
        );
        let socket = Config {
            socket: PathBuf::new(),
            ..Config::default()
        };
        assert_eq!(
            socket.validate().unwrap_err().code(),
            crate::ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn rejects_unparseable_streaming_address() {
        let config = Config {
            streaming_address: "not-a-host".into(),
            streaming_port: 0,
            ..Config::default()
        };
        assert_eq!(
            config.validate().unwrap_err().code(),
            crate::ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn host_shares_must_be_absolute_and_manifest_safe() {
        for path in [
            PathBuf::from("relative/path"),
            PathBuf::from("/tmp/share\tname"),
            PathBuf::from("/tmp/share\nname"),
            PathBuf::from("/tmp/share\rname"),
        ] {
            let config = Config {
                host_shares: vec![path],
                ..Config::default()
            };
            assert_eq!(
                config.validate().unwrap_err().code(),
                crate::ErrorCode::InvalidArgument
            );
        }

        let config = Config {
            host_shares: vec![PathBuf::from("/tmp/share with spaces")],
            ..Config::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn insecure_registries_marker_tracks_the_host_flag() {
        let dir = std::env::temp_dir().join(format!("oci-cri-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        sync_insecure_registries_marker(&dir, true).unwrap();
        let path = dir.join(INSECURE_REGISTRIES_MARKER);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\n");
        sync_insecure_registries_marker(&dir, false).unwrap();
        assert!(!path.exists());
        sync_insecure_registries_marker(&dir, false).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn layout_helpers_nest_under_root() {
        let config = Config {
            root: PathBuf::from("/var/lib/oci-cri"),
            ..Config::default()
        };
        assert_eq!(
            config.sandboxes_dir(),
            PathBuf::from("/var/lib/oci-cri/sandboxes")
        );
        assert_eq!(
            config.containers_dir(),
            PathBuf::from("/var/lib/oci-cri/containers")
        );
        assert_eq!(
            config.rootfs_dir(),
            PathBuf::from("/var/lib/oci-cri/rootfs")
        );
    }
}
