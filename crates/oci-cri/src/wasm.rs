// SPDX-License-Identifier: Apache-2.0

//! Decides which containers run as WASI modules.
//!
//! The pause container stays on libcontainer. A workload container is WASM
//! when the sandbox handler is `zwasm`, or when the image or the resolved
//! command says so. Builds without the `zwasm` feature still classify
//! containers, and refuse to create one.

use serde::{Deserialize, Serialize};

use crate::engine::ImageSpec;
use crate::error::{Error, ErrorCode, Result};

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

/// CRI runtime handler name advertised when the `zwasm` feature is enabled.
pub const HANDLER: &str = "zwasm";

/// Image-config annotation used by crun and containerd-shim-wasm.
pub const VARIANT_ANNOTATION: &str = "module.wasm.image/variant";

/// Persisted on [`crate::store::ContainerRecord::executor`].
pub const EXECUTOR_WASM: &str = "wasm";

/// True when a stored executor field names the WASM path.
#[must_use]
pub fn is_wasm_executor(executor: &str) -> bool {
    executor == EXECUTOR_WASM
}

/// Why a container took the WASM path. `component` is a wasip2/wasip3 image,
/// which this runtime can select but cannot instantiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmChoice {
    /// Image OS was `wasip2` or `wasip3`.
    pub component: bool,
}

/// Reject a sandbox handler this runtime does not implement.
///
/// An empty handler is the native default. `zwasm` is accepted only when the
/// crate is built with the `zwasm` feature.
pub fn check_handler(handler: &str) -> Result<()> {
    if handler.is_empty() {
        return Ok(());
    }
    if handler != HANDLER {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            format!("unknown runtime handler \"{handler}\""),
            "",
        ));
    }
    if !cfg!(feature = "zwasm") {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "runtime handler \"zwasm\" requires oci-cri built with the zwasm feature",
            "",
        ));
    }
    Ok(())
}

/// Classify a workload container. `command` is the resolved argv, image
/// entrypoint included.
pub fn classify(
    handler: &str,
    image: &ImageSpec,
    command: &[String],
) -> Result<Option<WasmChoice>> {
    check_handler(handler)?;
    let component = matches!(image.os.as_str(), "wasip2" | "wasip3");
    let selected = handler == HANDLER
        || variant_is_wasm(&image.annotations)
        || platform_is_wasm(image)
        || command_is_wasm(command);
    if !selected {
        return Ok(None);
    }
    if !cfg!(feature = "zwasm") {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "wasm workloads require oci-cri built with the zwasm feature",
            "",
        ));
    }
    Ok(Some(WasmChoice { component }))
}

fn variant_is_wasm(annotations: &std::collections::BTreeMap<String, String>) -> bool {
    matches!(
        annotations.get(VARIANT_ANNOTATION).map(String::as_str),
        Some("compat" | "compat-smart")
    )
}

fn platform_is_wasm(image: &ImageSpec) -> bool {
    matches!(
        image.os.as_str(),
        "wasip1" | "wasip2" | "wasip3" | "wasm" | "wasi"
    ) || image.architecture == "wasm32"
        || image.architecture == "wasi"
}

fn command_is_wasm(command: &[String]) -> bool {
    command
        .first()
        .is_some_and(|program| program.ends_with(".wasm"))
}

/// Argv the guest sees: an explicit command, otherwise the image entrypoint
/// and default command.
#[must_use]
pub fn resolved_command(command: &[String], args: &[String], image: &ImageSpec) -> Vec<String> {
    let mut argv = if command.is_empty() {
        let mut from_image = image.entrypoint.clone();
        from_image.extend(image.cmd.iter().cloned());
        from_image
    } else {
        command.to_vec()
    };
    argv.extend(args.iter().cloned());
    argv
}

/// Host path of the module inside an unpacked rootfs.
///
/// The program name must end in `.wasm`. The opened file, after resolving
/// `..` and symlinks, must stay inside `rootfs`.
#[must_use]
pub fn module_path(rootfs: &str, argv: &[String]) -> Option<std::path::PathBuf> {
    let guest = argv.first()?;
    if !guest.ends_with(".wasm") {
        return None;
    }
    let root = std::fs::canonicalize(rootfs).ok()?;
    let relative = guest.trim_start_matches('/');
    if relative.is_empty() || std::path::Path::new(relative).is_absolute() {
        return None;
    }
    let opened = std::fs::canonicalize(root.join(relative)).ok()?;
    opened.starts_with(&root).then_some(opened)
}

/// Copy the module selected by [`module_path`] into `dest`.
///
/// Later starts execute this copy. `dest` must sit outside `rootfs`. The
/// source is opened with `O_NOFOLLOW` after the confinement check, and it
/// must be a regular file, so a swap to a symlink between the check and the
/// read fails closed.
pub(crate) fn seal_module(
    rootfs: &str,
    argv: &[String],
    dest: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let source = module_path(rootfs, argv).ok_or_else(|| {
        Error::new(
            ErrorCode::InvalidArgument,
            "wasm container has no .wasm entrypoint",
            "",
        )
    })?;
    let root = std::fs::canonicalize(rootfs).map_err(|err| {
        Error::new(
            ErrorCode::InvalidArgument,
            "wasm rootfs is not available",
            err.to_string(),
        )
    })?;
    let placed = destination_outside(&root, dest)?;
    let parent = placed.parent().ok_or_else(|| {
        Error::new(
            ErrorCode::InvalidArgument,
            "wasm module destination has no parent",
            "",
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|err| {
        Error::new(
            ErrorCode::Internal,
            "create wasm module directory",
            err.to_string(),
        )
    })?;
    let mut input = open_regular(&source).map_err(|err| {
        Error::new(
            ErrorCode::InvalidArgument,
            "wasm module is not a regular file inside the rootfs",
            err.to_string(),
        )
    })?;
    let mut output = create_private(&placed).map_err(|err| {
        Error::new(
            ErrorCode::Internal,
            "create sealed wasm module",
            err.to_string(),
        )
    })?;
    if let Err(err) = std::io::copy(&mut input, &mut output) {
        let _ = std::fs::remove_file(&placed);
        return Err(Error::new(
            ErrorCode::Internal,
            "copy sealed wasm module",
            err.to_string(),
        ));
    }
    Ok(placed)
}

/// Read a sealed module. A final symlink is an error.
#[cfg_attr(not(feature = "zwasm"), allow(dead_code))]
pub(crate) fn read_sealed(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let mut file = open_regular(path)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(bytes)
}

fn destination_outside(
    root: &std::path::Path,
    dest: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let name = dest
        .file_name()
        .filter(|name| *name != "." && *name != "..")
        .ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidArgument,
                "wasm module destination has no file name",
                "",
            )
        })?;
    let mut suffix = Vec::new();
    let mut cursor = dest.parent().unwrap_or(dest);
    let existing = loop {
        if cursor.as_os_str().is_empty() || cursor.exists() {
            break cursor;
        }
        let Some(next) = cursor.parent().filter(|next| *next != cursor) else {
            break cursor;
        };
        if let Some(component) = cursor.file_name() {
            suffix.push(component.to_os_string());
        }
        cursor = next;
    };
    let mut placed = std::fs::canonicalize(existing).map_err(|err| {
        Error::new(
            ErrorCode::Internal,
            "wasm module directory is not available",
            err.to_string(),
        )
    })?;
    if placed.starts_with(root) {
        return Err(inside_rootfs());
    }
    for component in suffix.iter().rev() {
        placed.push(component);
        if placed.starts_with(root) {
            return Err(inside_rootfs());
        }
    }
    placed.push(name);
    if placed.starts_with(root) {
        Err(inside_rootfs())
    } else {
        Ok(placed)
    }
}

fn inside_rootfs() -> Error {
    Error::new(
        ErrorCode::InvalidArgument,
        "wasm module destination is inside the rootfs",
        "",
    )
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

fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn image() -> ImageSpec {
        ImageSpec::default()
    }

    #[test]
    fn empty_handler_and_native_command_stay_native() {
        let choice = classify("", &image(), &["/bin/sh".into()]).unwrap();
        assert!(choice.is_none());
    }

    #[test]
    fn unknown_handler_is_rejected() {
        let err = classify("wasmedge", &image(), &[]).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_wasm_argument_does_not_select_a_native_command() {
        let choice =
            classify("", &image(), &["cp".into(), "a.wasm".into(), "/dst".into()]).unwrap();
        assert!(choice.is_none());
    }

    #[test]
    fn dot_wasm_command_selects_a_module() {
        let choice = classify("", &image(), &["/hello.wasm".into()]);
        if cfg!(feature = "zwasm") {
            assert_eq!(choice.unwrap(), Some(WasmChoice { component: false }));
        } else {
            assert_eq!(choice.unwrap_err().code(), ErrorCode::Unsupported);
        }
    }

    #[test]
    fn annotation_and_platform_select_wasm() {
        let mut annotated = image();
        annotated
            .annotations
            .insert(VARIANT_ANNOTATION.into(), "compat".into());
        assert!(classify("", &annotated, &[]).is_err() || cfg!(feature = "zwasm"));

        let mut platform = image();
        platform.architecture = "wasm32".into();
        assert!(matches!(classify("", &platform, &[]), Err(_) | Ok(Some(_))));

        let mut component = image();
        component.os = "wasip2".into();
        match classify("", &component, &[]) {
            Ok(Some(choice)) => assert!(choice.component),
            Err(err) => assert_eq!(err.code(), ErrorCode::Unsupported),
            Ok(None) => panic!("wasip2 must select wasm"),
        }
    }

    #[test]
    fn resolved_command_prefers_an_explicit_command() {
        let mut spec = image();
        spec.entrypoint = vec!["/image.wasm".into()];
        spec.cmd = vec!["from-image".into()];
        let argv = resolved_command(&["/override.wasm".into()], &["--flag".into()], &spec);
        assert_eq!(
            argv,
            vec!["/override.wasm".to_string(), "--flag".to_string()]
        );
        let from_image = resolved_command(&[], &["--flag".into()], &spec);
        assert_eq!(
            from_image,
            vec![
                "/image.wasm".to_string(),
                "from-image".to_string(),
                "--flag".to_string()
            ]
        );
    }

    #[test]
    fn module_path_joins_the_rootfs() {
        let dir = std::env::temp_dir().join(format!("wasm-mod-{}", std::process::id()));
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/app.wasm"), b"\0asm").unwrap();
        let path = module_path(&root.to_string_lossy(), &["/bin/app.wasm".into()]).unwrap();
        assert_eq!(
            path,
            std::fs::canonicalize(root.join("bin/app.wasm")).unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn module_path_ignores_a_later_wasm_argument() {
        assert!(module_path("/var/root", &["/bin/tool".into(), "x.wasm".into()]).is_none());
    }

    #[test]
    fn module_path_rejects_paths_outside_the_rootfs() {
        let dir = std::env::temp_dir().join(format!("wasm-escape-{}", std::process::id()));
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.join("outside.wasm");
        std::fs::write(&outside, b"\0asm").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link.wasm")).unwrap();
        assert!(module_path(&root.to_string_lossy(), &["/../outside.wasm".into()]).is_none());
        #[cfg(unix)]
        assert!(module_path(&root.to_string_lossy(), &["/link.wasm".into()]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn seal_module_keeps_the_bytes_after_the_rootfs_changes() {
        let dir = std::env::temp_dir().join(format!("wasm-seal-{}", std::process::id()));
        let root = dir.join("root");
        let store = dir.join("store");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/app.wasm"), b"\0asm-v1").unwrap();
        let dest = store.join("id.module");
        let sealed =
            seal_module(&root.to_string_lossy(), &["/bin/app.wasm".into()], &dest).unwrap();
        assert_eq!(sealed, std::fs::canonicalize(&dest).unwrap());
        std::fs::write(root.join("bin/app.wasm"), b"\0asm-v2").unwrap();
        assert_eq!(read_sealed(&sealed).unwrap(), b"\0asm-v1");
        let outside = dir.join("secret");
        std::fs::write(&outside, b"secret-bytes").unwrap();
        std::fs::remove_file(root.join("bin/app.wasm")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("bin/app.wasm")).unwrap();
        assert_eq!(read_sealed(&sealed).unwrap(), b"\0asm-v1");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn seal_module_rejects_a_destination_inside_the_rootfs() {
        let dir = std::env::temp_dir().join(format!("wasm-seal-in-{}", std::process::id()));
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/app.wasm"), b"\0asm").unwrap();
        let err = seal_module(
            &root.to_string_lossy(),
            &["/bin/app.wasm".into()],
            &root.join("bin/copy.module"),
        )
        .unwrap_err();
        assert_eq!(
            err.message(),
            "wasm module destination is inside the rootfs"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

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
    fn variant_helper_ignores_other_annotations() {
        let mut annotations = BTreeMap::new();
        annotations.insert(VARIANT_ANNOTATION.into(), "other".into());
        assert!(!variant_is_wasm(&annotations));
    }
}
