// SPDX-License-Identifier: Apache-2.0

//! One preview1 module per process.
//!
//! zwasm's engine is one thread per process, and WASI stdio is this process's
//! standard streams. `oci-cri` writes a [`oci_cri_zwasm::WasmRun`] to stdin.
//! On Linux the process joins the sandbox network namespace before the engine
//! exists, so WASI sockets follow CNI.

use oci_cri_zwasm::WasmRun;

const PAGE_BYTES: u64 = 64 * 1024;

fn main() {
    let code = match read_spec() {
        Ok(spec) => run_spec(&spec),
        Err(err) => {
            eprintln!("zwasm guest: {err}");
            1
        }
    };
    std::process::exit(code);
}

fn read_spec() -> Result<WasmRun, String> {
    let mut payload = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin(), &mut payload)
        .map_err(|err| format!("failed to read spec: {err}"))?;
    oci_cri_zwasm::decode_spec(&payload).map_err(|err| err.to_string())
}

fn run_spec(spec: &WasmRun) -> i32 {
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

fn run_module(spec: &WasmRun) -> Result<i32, String> {
    let bytes = oci_cri_zwasm::read_sealed(&spec.module_path)
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

fn enter_netns(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).map_err(|err| format!("open netns {path}: {err}"))?;
        // SAFETY: `setns` only changes this process's network namespace to the
        // namespace `file` already refers to. The fd stays open until return.
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
