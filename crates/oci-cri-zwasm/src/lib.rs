// SPDX-License-Identifier: Apache-2.0

//! Spec and sealed-module reader shared by `oci-cri` and the guest runner.
//!
//! The runner binary links `zwasm-sdk`. The CRI daemon does not. Build it
//! with `cargo build -p oci-cri-zwasm --features zwasm`. Zig 0.16 has to be
//! on `PATH` for that build. The machine that runs the binary does not need
//! Zig.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// File name of the guest runner. `oci-cri` looks beside its own executable
/// and on `PATH`.
pub const HELPER_FILE: &str = "oci-cri-zwasm";

/// Path of the guest runner. When set, sibling and `PATH` lookup are skipped.
pub const BIN_ENV: &str = "OCI_CRI_ZWASM_BIN";

/// Frozen WASI setup for one container. The guest process reads this JSON
/// from stdin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmRun {
    /// Daemon-owned copy of the module, captured at create.
    /// Later starts read this file and do not reopen the rootfs.
    pub module_path: std::path::PathBuf,
    /// Guest argv. The first entry is the program name.
    pub args: Vec<String>,
    /// Guest environment pairs.
    pub envs: Vec<(String, String)>,
    /// Host directory, guest path.
    pub preopens: Vec<(String, String)>,
    /// Sandbox network namespace (`/proc/<pid>/ns/net`). Empty skips `setns`.
    pub netns: String,
    /// CRI memory limit in bytes. `0` leaves zwasm's default.
    pub memory_limit_bytes: u64,
    /// Image OS is wasip2 or wasip3. The guest exits before instantiation.
    pub component: bool,
}

/// Decode one guest spec from the JSON blob the daemon writes to stdin.
pub fn decode_spec(bytes: &[u8]) -> Result<WasmRun, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Read a sealed module. A final symlink is an error.
pub fn read_sealed(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let mut file = open_regular(path)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(bytes)
}

fn open_regular(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let file = open_nofollow(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

fn open_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_sealed_rejects_a_symlink() {
        let dir = std::env::temp_dir().join(format!("wasm-sealed-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("mod");
        std::fs::write(&real, b"\0asm").unwrap();
        assert_eq!(read_sealed(&real).unwrap(), b"\0asm");
        #[cfg(unix)]
        {
            let link = dir.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(read_sealed(&link).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spec_json_uses_the_daemon_field_names() {
        let spec = WasmRun {
            module_path: "/var/lib/oci-cri/wasm/abc.module".into(),
            args: vec!["/hello.wasm".into()],
            envs: vec![("A".into(), "b".into())],
            preopens: vec![("/host".into(), "/".into())],
            netns: String::new(),
            memory_limit_bytes: 0,
            component: false,
        };
        let encoded = serde_json::to_value(&spec).unwrap();
        for key in [
            "module_path",
            "args",
            "envs",
            "preopens",
            "netns",
            "memory_limit_bytes",
            "component",
        ] {
            assert!(encoded.get(key).is_some(), "{key}");
        }
        let decoded: WasmRun = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, spec);
    }
}
