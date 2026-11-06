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
    /// Unpacked module on the host.
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
#[must_use]
pub fn module_path(rootfs: &str, argv: &[String]) -> Option<std::path::PathBuf> {
    let guest = argv.first()?;
    if !guest.ends_with(".wasm") {
        return None;
    }
    let relative = guest.trim_start_matches('/');
    Some(std::path::Path::new(rootfs).join(relative))
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
        let path = module_path("/var/root", &["/bin/app.wasm".into()]).unwrap();
        assert_eq!(path, std::path::PathBuf::from("/var/root/bin/app.wasm"));
    }

    #[test]
    fn module_path_ignores_a_later_wasm_argument() {
        assert!(module_path("/var/root", &["/bin/tool".into(), "x.wasm".into()]).is_none());
    }

    #[test]
    fn variant_helper_ignores_other_annotations() {
        let mut annotations = BTreeMap::new();
        annotations.insert(VARIANT_ANNOTATION.into(), "other".into());
        assert!(!variant_is_wasm(&annotations));
    }
}
