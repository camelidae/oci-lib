// SPDX-License-Identifier: Apache-2.0

//! Starts one preview1 module in the `oci-cri-zwasm` process.
//!
//! zwasm's engine is one thread per process, and WASI stdio is the process
//! standard streams. The engine stays out of this daemon so the daemon binary
//! does not grow by the engine. The child's stdout and stderr are pipes the
//! parent copies into the CRI log. On Linux the child joins the sandbox
//! network namespace before the engine exists.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use oci_cri_zwasm::{BIN_ENV, HELPER_FILE};

use crate::engine::OutputSink;
use crate::error::{Error, ErrorCode, Result};
use crate::wasm::WasmRun;

/// A running guest process.
#[derive(Debug)]
pub struct WasmProcess {
    /// Operating-system pid, for stop.
    pub pid: u32,
    child: Child,
}

impl WasmProcess {
    /// Block until the guest exits and return its status.
    pub fn wait(mut self) -> i32 {
        match self.child.wait() {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        }
    }
}

/// Fail when the guest runner is not installed.
pub fn require_helper() -> Result<()> {
    resolve_helper(
        std::env::var_os(BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
    .map(|_| ())
}

/// Locate the guest runner.
///
/// `OCI_CRI_ZWASM_BIN` wins. Otherwise the file named `oci-cri-zwasm` beside
/// the current executable, then the same name on `PATH`.
pub fn resolve_helper(
    configured: Option<&OsStr>,
    current_exe: Option<&Path>,
    path_env: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(raw) = configured {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Ok(path);
        }
        return Err(missing_helper(&format!(
            "{BIN_ENV}={} is not a file",
            path.display()
        )));
    }
    if let Some(exe) = current_exe
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(HELPER_FILE);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if let Some(path_env) = path_env
        && let Some(found) = std::env::split_paths(path_env).find_map(|dir| {
            let candidate = dir.join(HELPER_FILE);
            candidate.is_file().then_some(candidate)
        })
    {
        return Ok(found);
    }
    Err(missing_helper(
        "install oci-cri-zwasm beside this executable or on PATH, or set OCI_CRI_ZWASM_BIN",
    ))
}

fn missing_helper(detail: &str) -> Error {
    Error::new(
        ErrorCode::Unsupported,
        "wasm guest runner oci-cri-zwasm was not found",
        detail,
    )
}

/// Run `spec` in the guest runner. `on_output` receives stdout (`1`) and
/// stderr (`2`) until the pipes close.
pub fn spawn(spec: &WasmRun, on_output: OutputSink) -> Result<WasmProcess> {
    let payload = serde_json::to_vec(spec)
        .map_err(|err| Error::new(ErrorCode::Internal, "encode wasm spec", err.to_string()))?;
    let exe = resolve_helper(
        std::env::var_os(BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )?;
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| Error::new(ErrorCode::Run, "spawn zwasm guest", err.to_string()))?;
    if let Err(err) = send_spec_and_collect_output(&mut child, &payload, &on_output) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    Ok(WasmProcess {
        pid: child.id(),
        child,
    })
}

fn send_spec_and_collect_output(
    child: &mut Child,
    payload: &[u8],
    on_output: &OutputSink,
) -> Result<()> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::new(ErrorCode::Internal, "zwasm guest stdin missing", ""))?;
    stdin
        .write_all(payload)
        .map_err(|err| Error::new(ErrorCode::Run, "send wasm spec", err.to_string()))?;
    drop(stdin);
    let pid = child.id();
    if let Some(stdout) = child.stdout.take() {
        let sink = Arc::clone(on_output);
        std::thread::Builder::new()
            .name(format!("zwasm-out-{pid}"))
            .spawn(move || copy_stream(stdout, 1, &sink))
            .map_err(|err| {
                Error::new(ErrorCode::Internal, "spawn stdout reader", err.to_string())
            })?;
    }
    if let Some(stderr) = child.stderr.take() {
        let sink = Arc::clone(on_output);
        std::thread::Builder::new()
            .name(format!("zwasm-err-{pid}"))
            .spawn(move || copy_stream(stderr, 2, &sink))
            .map_err(|err| {
                Error::new(ErrorCode::Internal, "spawn stderr reader", err.to_string())
            })?;
    }
    Ok(())
}

fn copy_stream(mut reader: impl Read, stream: u8, sink: &OutputSink) {
    let mut buf = [0_u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => sink(stream, &buf[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn configured_path_wins() {
        let dir = std::env::temp_dir().join(format!("zwasm-bin-{}", std::process::id()));
        let runner = dir.join("runner");
        touch(&runner);
        let found = resolve_helper(Some(runner.as_os_str()), None, None).unwrap();
        assert_eq!(found, runner);
        let err = resolve_helper(Some(dir.join("missing").as_os_str()), None, None).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Unsupported);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sibling_and_path_are_used_when_unset() {
        let dir = std::env::temp_dir().join(format!("zwasm-sib-{}", std::process::id()));
        let sibling = dir.join(HELPER_FILE);
        touch(&sibling);
        let exe = dir.join("oci-cri");
        let found = resolve_helper(None, Some(&exe), None).unwrap();
        assert_eq!(found, sibling);

        let path_dir = std::env::temp_dir().join(format!("zwasm-path-{}", std::process::id()));
        let on_path = path_dir.join(HELPER_FILE);
        touch(&on_path);
        let found = resolve_helper(
            None,
            Some(Path::new("/no/such/oci-cri")),
            Some(path_dir.as_os_str()),
        )
        .unwrap();
        assert_eq!(found, on_path);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(path_dir);
    }

    #[test]
    fn a_missing_runner_is_unsupported() {
        let err = resolve_helper(
            None,
            Some(Path::new("/no/such/oci-cri")),
            Some(OsStr::new("/no/such-path-zwasm")),
        )
        .unwrap_err();
        assert!(err.to_string().contains(HELPER_FILE));
    }
}
