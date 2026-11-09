// SPDX-License-Identifier: Apache-2.0

//! Host directories exported into the Linux guest, and the path rewrite that
//! goes with them. Virtiofs devices are fixed before the VM starts, so every
//! directory a call needs is collected first.

use std::path::{Path, PathBuf};

/// Runner rootfs share tag.
pub const TAG_ROOTFS: &str = "rootfs";
/// Builder build-context share tag.
pub const TAG_CONTEXT: &str = "context";
/// Builder storage-root share tag.
pub const TAG_ROOT: &str = "root";
/// Builder run-root share tag.
pub const TAG_RUNROOT: &str = "runroot";
/// Builder signature-policy parent share tag.
pub const TAG_POLICY: &str = "policy";
/// Builder registries.conf parent share tag.
pub const TAG_REGISTRIES: &str = "registries";
/// Builder auth-file parent share tag.
pub const TAG_AUTH: &str = "auth";

/// Guest mount for [`TAG_ROOTFS`].
pub const GUEST_ROOTFS: &str = "/mnt/rootfs";
/// Guest mount for [`TAG_CONTEXT`].
pub const GUEST_CONTEXT: &str = "/mnt/context";
/// Guest mount for [`TAG_ROOT`].
pub const GUEST_ROOT: &str = "/mnt/root";
/// Guest mount for [`TAG_RUNROOT`].
pub const GUEST_RUNROOT: &str = "/mnt/runroot";
/// Guest mount for [`TAG_POLICY`].
pub const GUEST_POLICY: &str = "/mnt/policy";
/// Guest mount for [`TAG_REGISTRIES`].
pub const GUEST_REGISTRIES: &str = "/mnt/registries";
/// Guest mount for [`TAG_AUTH`].
pub const GUEST_AUTH: &str = "/mnt/auth";

/// One host directory mounted in the guest at `guest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    /// Virtiofs tag.
    pub tag: String,
    /// Canonical host directory.
    pub host: PathBuf,
    /// Absolute guest mount point.
    pub guest: String,
    /// Whether the guest sees the share read-only.
    pub read_only: bool,
}

/// Directories a builder boot has to export.
///
/// `context` is the build context. The dockerfile has to live inside it.
/// File settings contribute their parent directory.
pub struct ShareRequest<'a> {
    /// Build context directory.
    pub context: Option<&'a Path>,
    /// Dockerfile path (must be under `context` when both are set).
    pub dockerfile: Option<&'a Path>,
    /// Graph / storage root.
    pub storage_root: Option<&'a Path>,
    /// Run root.
    pub run_root: Option<&'a Path>,
    /// Signature policy file.
    pub signature_policy: Option<&'a Path>,
    /// registries.conf file.
    pub registries_conf: Option<&'a Path>,
    /// containers auth file.
    pub auth_file: Option<&'a Path>,
}

/// Plan virtiofs exports for a builder-style share request.
pub fn plan_shares(request: ShareRequest<'_>) -> Result<Vec<Export>, String> {
    if let (Some(dockerfile), Some(context)) = (request.dockerfile, request.context) {
        if !dockerfile.starts_with(context) {
            return Err(format!(
                "dockerfile {} is outside the build context {}",
                dockerfile.display(),
                context.display()
            ));
        }
    }
    let mut exports = Vec::new();
    if let Some(context) = request.context {
        add_dir(&mut exports, TAG_CONTEXT, GUEST_CONTEXT, context, true)?;
    }
    if let Some(root) = request.storage_root {
        add_dir(&mut exports, TAG_ROOT, GUEST_ROOT, root, false)?;
    }
    if let Some(run) = request.run_root {
        add_dir(&mut exports, TAG_RUNROOT, GUEST_RUNROOT, run, false)?;
    }
    if let Some(policy) = request.signature_policy {
        add_file_parent(&mut exports, TAG_POLICY, GUEST_POLICY, policy, false)?;
    }
    if let Some(registries) = request.registries_conf {
        add_file_parent(
            &mut exports,
            TAG_REGISTRIES,
            GUEST_REGISTRIES,
            registries,
            false,
        )?;
    }
    if let Some(auth) = request.auth_file {
        add_file_parent(&mut exports, TAG_AUTH, GUEST_AUTH, auth, false)?;
    }
    Ok(exports)
}

/// Export a single writable rootfs for the runner guest.
pub fn export_rootfs(rootfs: &Path) -> Result<Export, String> {
    let host = canonicalize(rootfs)?;
    if !host.is_dir() {
        return Err(format!("{} is not a directory", host.display()));
    }
    Ok(Export {
        tag: TAG_ROOTFS.to_string(),
        host,
        guest: GUEST_ROOTFS.to_string(),
        read_only: false,
    })
}

/// Rewrite a host path to the guest path under the longest matching export.
pub fn guest_path(path: &Path, exports: &[Export]) -> Result<String, String> {
    let path = canonicalize(path)?;
    let export = exports
        .iter()
        .filter(|export| path.starts_with(&export.host))
        .max_by_key(|export| export.host.as_os_str().len())
        .ok_or_else(|| {
            format!(
                "path {} is not inside a directory exported to the guest",
                path.display()
            )
        })?;
    let relative = path
        .strip_prefix(&export.host)
        .map_err(|err| err.to_string())?;
    let mut guest = export.guest.clone();
    for component in relative.components() {
        let text = component.as_os_str().to_string_lossy();
        if text.is_empty() || text == "." {
            continue;
        }
        if !guest.ends_with('/') {
            guest.push('/');
        }
        guest.push_str(&text);
    }
    Ok(guest)
}

/// Add a directory export unless that host path is already covered.
pub fn add_dir(
    exports: &mut Vec<Export>,
    tag: &'static str,
    guest: &'static str,
    dir: &Path,
    read_only: bool,
) -> Result<(), String> {
    let host = canonicalize(dir)?;
    if !host.is_dir() {
        return Err(format!("{} is not a directory", host.display()));
    }
    if exports.iter().any(|export| export.host == host) {
        return Ok(());
    }
    exports.push(Export {
        tag: tag.to_string(),
        host,
        guest: guest.to_string(),
        read_only,
    });
    Ok(())
}

/// Add the parent directory of a file as an export.
pub fn add_file_parent(
    exports: &mut Vec<Export>,
    tag: &'static str,
    guest: &'static str,
    file: &Path,
    read_only: bool,
) -> Result<(), String> {
    let file = canonicalize(file)?;
    if !file.is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    let parent = file
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", file.display()))?;
    add_dir(exports, tag, guest, parent, read_only)
}

fn canonicalize(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "oci-util-share-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn rewrites_rootfs_to_guest_path() {
        let root = scratch("rootfs");
        let export = export_rootfs(&root).unwrap();
        assert_eq!(guest_path(&root, &[export]).unwrap(), "/mnt/rootfs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrites_context_root_and_a_policy_file() {
        let root = scratch("tree");
        let context = root.join("ctx");
        let graph = root.join("graph");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::create_dir_all(&graph).unwrap();
        let dockerfile = context.join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\n").unwrap();
        let policy = root.join("policy.json");
        std::fs::write(&policy, "{}\n").unwrap();

        let exports = plan_shares(ShareRequest {
            context: Some(&context),
            dockerfile: Some(&dockerfile),
            storage_root: Some(&graph),
            run_root: None,
            signature_policy: Some(&policy),
            registries_conf: None,
            auth_file: None,
        })
        .unwrap();
        assert!(exports.iter().any(|export| export.tag == TAG_CONTEXT));
        assert_eq!(
            guest_path(&dockerfile, &exports).unwrap(),
            "/mnt/context/Dockerfile"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
