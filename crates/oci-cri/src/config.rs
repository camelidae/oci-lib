// SPDX-License-Identifier: Apache-2.0

//! Runtime configuration for the CRI daemon.

use std::net::SocketAddr;
use std::path::PathBuf;

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

impl Default for Config {
    fn default() -> Self {
        Self {
            runtime_config_dir: Some(PathBuf::from("/etc/oci-cri/config.d")),
            runtimes: Default::default(),
            runtime_name: RUNTIME_NAME.to_string(),
            socket: PathBuf::from("/run/oci-cri/oci-cri.sock"),
            root: PathBuf::from("/var/lib/oci-cri"),
            state_root: PathBuf::from("/run/oci-cri/state"),
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
