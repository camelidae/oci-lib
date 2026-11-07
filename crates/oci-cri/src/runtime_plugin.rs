// SPDX-License-Identifier: Apache-2.0
//! Executable handlers run inside the same namespaces and cgroups as native workloads.

use crate::{Config, Error, ErrorCode, engine::EngineImage, error::Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const GUEST_EXECUTABLE: &str = "/run/oci-cri-runtime";

/// A standalone executable, not a containerd shim or an in-process library.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlugin {
    /// Absolute host path to an executable runner.
    pub runtime_path: PathBuf,
    /// OCI platform selected for this handler.
    pub platform: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    runtimes: BTreeMap<String, RuntimePlugin>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidArgument, message, "")
}

impl RuntimePlugin {
    /// Check that the supported platform and executable are available.
    pub fn validate(&self) -> Result<()> {
        if self.platform != "wasip3/wasm32" {
            return Err(invalid(
                "executable handlers currently support only wasip3/wasm32",
            ));
        }
        if !self.runtime_path.is_absolute() {
            return Err(invalid("runtime_path must be absolute"));
        }
        let metadata = self
            .runtime_path
            .metadata()
            .map_err(|e| invalid(format!("{}: {e}", self.runtime_path.display())))?;
        if !metadata.is_file() {
            return Err(invalid("runtime_path must be a file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(invalid("runtime_path must be executable"));
            }
        }
        Ok(())
    }

    /// Bind the runner read-only and prefix argv, preserving the workload's args,
    /// environment, credentials, filesystem, network, and resource limits.
    pub(crate) fn apply(&self, spec: &mut Value) -> Result<()> {
        self.validate()?;
        let mounts = spec["mounts"]
            .as_array_mut()
            .ok_or_else(|| invalid("OCI spec has no mounts"))?;
        if mounts.iter().any(|m| {
            m["destination"]
                .as_str()
                .is_some_and(|p| p == GUEST_EXECUTABLE || p.starts_with("/run/oci-cri-runtime/"))
        }) {
            return Err(invalid("mount collides with runtime executable"));
        }
        mounts.push(json!({"destination":GUEST_EXECUTABLE,"type":"bind","source":self.runtime_path,"options":["bind","ro","nosuid","nodev"]}));
        let args = spec["process"]["args"]
            .as_array_mut()
            .ok_or_else(|| invalid("OCI spec has no process args"))?;
        if args.is_empty() {
            return Err(invalid("component path is missing"));
        }
        args.insert(0, json!(GUEST_EXECUTABLE));
        Ok(())
    }
}

/// Load sorted TOML drop-ins, rejecting ambiguous handler definitions.
pub fn load(directory: &Path) -> Result<BTreeMap<String, RuntimePlugin>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(invalid(format!("{}: {e}", directory.display()))),
    };
    let mut paths = entries
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| invalid(e.to_string()))?;
    paths.sort();
    let mut runtimes = BTreeMap::new();
    for path in paths
        .into_iter()
        .filter(|p| p.extension().is_some_and(|s| s == "toml"))
    {
        let contents = std::fs::read_to_string(&path).map_err(|e| invalid(e.to_string()))?;
        let file: File =
            toml::from_str(&contents).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
        for (name, runtime) in file.runtimes {
            if name.is_empty() {
                return Err(invalid("native runtime cannot be replaced"));
            }
            runtime.validate()?;
            if runtimes.insert(name.clone(), runtime).is_some() {
                return Err(invalid(format!("duplicate runtime handler {name}")));
            }
        }
    }
    Ok(runtimes)
}

pub(crate) fn resolve<'a>(config: &'a Config, handler: &str) -> Result<Option<&'a RuntimePlugin>> {
    if handler.is_empty() {
        return Ok(None);
    }
    config
        .runtimes
        .get(handler)
        .map(Some)
        .ok_or_else(|| invalid(format!("unknown runtime handler {handler}")))
}

pub(crate) fn matches_image(plugin: Option<&RuntimePlugin>, image: &EngineImage) -> bool {
    match plugin {
        Some(p) => p.platform == format!("{}/{}", image.os, image.architecture),
        // Older fake/serialized records lacked platform fields; never accept WASI natively.
        None => image.os.is_empty() || image.os == "linux",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn drop_ins_require_an_installed_p3_runner_and_do_not_replace_native() {
        let dir = tempfile::tempdir().unwrap();
        let runner = dir.path().join("runner");
        std::fs::write(&runner, b"fixture").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!(
            "[runtimes.wasmtime]\nruntime_path = {:?}\nplatform = 'wasip3/wasm32'\n",
            runner
        );
        std::fs::write(dir.path().join("wasmtime.toml"), &config).unwrap();
        let runtimes = load(dir.path()).unwrap();
        let plugin = &runtimes["wasmtime"];
        let mut spec = json!({"mounts":[],"process":{"args":["/probe.wasm","hello"],"env":["KEY=value"]},"linux":{"resources":{"memory":{"limit":134217728}}}});
        plugin.apply(&mut spec).unwrap();
        assert_eq!(
            spec["process"]["args"],
            json!([GUEST_EXECUTABLE, "/probe.wasm", "hello"])
        );
        assert_eq!(spec["process"]["env"], json!(["KEY=value"]));
        assert_eq!(spec["linux"]["resources"]["memory"]["limit"], 134217728);
        assert_eq!(
            spec["mounts"][0]["options"],
            json!(["bind", "ro", "nosuid", "nodev"])
        );
        let wasm = EngineImage {
            os: "wasip3".into(),
            architecture: "wasm32".into(),
            ..Default::default()
        };
        assert!(matches_image(Some(plugin), &wasm));
        assert!(!matches_image(None, &wasm));
        assert!(!matches_image(Some(plugin), &EngineImage::default()));
        assert!(resolve(&Config::default(), "").unwrap().is_none());
        assert!(resolve(&Config::default(), "unknown").is_err());
        std::fs::write(dir.path().join("duplicate.toml"), &config).unwrap();
        assert!(load(dir.path()).is_err());
        std::fs::remove_file(dir.path().join("duplicate.toml")).unwrap();
        std::fs::remove_file(runner).unwrap();
        assert!(load(dir.path()).is_err());
    }
}
