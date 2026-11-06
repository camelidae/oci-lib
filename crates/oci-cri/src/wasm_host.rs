// SPDX-License-Identifier: Apache-2.0

//! Runs one preview1 module in a fresh process.
//!
//! zwasm's engine is one thread per process, and WASI stdio is the process
//! standard streams. The CRI daemon therefore re-execs itself with
//! `OCI_CRI_ZWASM_RUN` set. [`crate::startup`] dispatches that child before
//! the server starts. The child's stdout and stderr are pipes the parent
//! copies into the CRI log. On Linux the child joins the sandbox network
//! namespace before the engine exists, so WASI sockets follow CNI.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use crate::engine::OutputSink;
use crate::error::{Error, ErrorCode, Result};
use crate::wasm::WasmRun;

/// Environment variable that selects the guest entry in [`crate::startup`].
pub const RUN_ENV: &str = "OCI_CRI_ZWASM_RUN";

const PAGE_BYTES: u64 = 64 * 1024;

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

/// Re-exec the current binary as the guest. `on_output` receives stdout (`1`)
/// and stderr (`2`) until the pipes close.
pub fn spawn(spec: &WasmRun, on_output: OutputSink) -> Result<WasmProcess> {
    let payload = serde_json::to_vec(spec)
        .map_err(|err| Error::new(ErrorCode::Internal, "encode wasm spec", err.to_string()))?;
    let exe = std::env::current_exe().map_err(|err| {
        Error::new(
            ErrorCode::Internal,
            "locate current executable",
            err.to_string(),
        )
    })?;
    let mut child = Command::new(exe)
        .env(RUN_ENV, "1")
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

/// Guest entry. Reads one [`WasmRun`] from stdin and exits with the module's
/// status. Called from [`crate::startup`] when [`RUN_ENV`] is set.
pub fn run_from_env() -> i32 {
    let mut payload = Vec::new();
    if std::io::stdin().read_to_end(&mut payload).is_err() {
        eprintln!("zwasm guest: failed to read spec");
        return 1;
    }
    let spec: WasmRun = match serde_json::from_slice(&payload) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("zwasm guest: {err}");
            return 1;
        }
    };
    run_spec(&spec)
}

/// Instantiate `spec` in this process and return the guest exit code.
pub fn run_spec(spec: &WasmRun) -> i32 {
    if spec.component {
        eprintln!(
            "wasip2 and wasip3 modules need a component host; zwasm-sdk 0.4 runs preview1 _start only"
        );
        return 127;
    }
    if let Err(err) = enter_netns(&spec.netns) {
        eprintln!("zwasm guest: {err}");
        return 1;
    }
    match run_module(spec) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("zwasm guest: {err}");
            1
        }
    }
}

fn run_module(spec: &WasmRun) -> std::result::Result<i32, String> {
    let bytes = std::fs::read(&spec.module_path)
        .map_err(|err| format!("read {}: {err}", spec.module_path.display()))?;
    let engine = zwasm_sdk::Engine::new().map_err(|err| err.to_string())?;
    let mut store = zwasm_sdk::Store::new(&engine).map_err(|err| err.to_string())?;
    let mut wasi = zwasm_sdk::WasiConfig::new().map_err(|err| err.to_string())?;
    let arg_refs: Vec<&str> = spec.args.iter().map(String::as_str).collect();
    if !arg_refs.is_empty() {
        wasi.set_args(&arg_refs).map_err(|err| err.to_string())?;
    }
    let env_refs: Vec<(&str, &str)> = spec
        .envs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    if !env_refs.is_empty() {
        wasi.set_envs(&env_refs).map_err(|err| err.to_string())?;
    }
    for (host, guest) in &spec.preopens {
        wasi.preopen_dir(host, guest)
            .map_err(|err| err.to_string())?;
    }
    // Guest stdout and stderr are this process's standard streams, which the
    // parent replaced with the CRI log pipes.
    wasi.inherit_stdio();
    store.set_wasi(wasi);
    let module = zwasm_sdk::Module::new(&mut store, &bytes).map_err(|err| err.to_string())?;
    let instance =
        zwasm_sdk::Instance::new(&mut store, &module, &[]).map_err(|err| err.to_string())?;
    if spec.memory_limit_bytes > 0 {
        let pages = spec.memory_limit_bytes.div_ceil(PAGE_BYTES);
        instance.set_memory_pages_limit(&mut store, pages);
    }
    let start = instance
        .get_func(&mut store, "_start")
        .ok_or_else(|| "module does not export _start".to_string())?;
    let mut results = vec![zwasm_sdk::Val::I32(0); start.result_arity(&store)];
    match start.call(&mut store, &[], &mut results) {
        Ok(()) => Ok(0),
        Err(zwasm_sdk::Error::WasiExit { code }) => Ok(i32::try_from(code).unwrap_or(1)),
        Err(err) => Err(err.to_string()),
    }
}

fn enter_netns(path: &str) -> std::result::Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).map_err(|err| format!("open netns {path}: {err}"))?;
        let rc = unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) };
        if rc != 0 {
            return Err(format!("setns {path}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err("joining a network namespace requires Linux".to_string())
    }
}
