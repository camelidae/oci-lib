// SPDX-License-Identifier: Apache-2.0
//! Runs only WASI 0.3 commands. The parent OCI runtime supplies isolation.
use anyhow::Result;
use wasmtime::error::Context as _;
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

struct State {
    ctx: WasiCtx,
    table: ResourceTable,
}
impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

async fn run(args: Vec<String>) -> Result<i32> {
    let path = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: oci-wasip3 COMPONENT [ARG ...]"))?;
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, path)
        .context("expected a WASI 0.3 component; core modules and Preview 1 are unsupported")?;
    let mut wasi = WasiCtxBuilder::new();
    wasi.args(&args)
        .envs(&std::env::vars().collect::<Vec<_>>())
        .inherit_stdio()
        .inherit_network()
        .allow_tcp(true)
        .allow_udp(true)
        .allow_ip_name_lookup(true)
        .preopened_dir("/", "/", wasmtime_wasi::FsPerms::ReadWrite)?;
    let mut store = Store::new(
        &engine,
        State {
            ctx: wasi.build(),
            table: ResourceTable::new(),
        },
    );
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let pre = linker
        .instantiate_pre(&component)
        .context("only WASI 0.3 imports are supported")?;
    let command = wasmtime_wasi::p3::bindings::CommandPre::new(pre)
        .context("expected wasi:cli/run@0.3.x; Preview 2 is unsupported")?
        .instantiate_async(&mut store)
        .await?;
    let result = store
        .run_concurrent(async |store| command.wasi_cli_run().call_run(store).await)
        .await?;
    match result {
        Ok(result) => Ok(if result.is_ok() { 0 } else { 1 }),
        Err(err) => match err.downcast_ref::<wasmtime_wasi::I32Exit>() {
            Some(exit) => Ok(exit.0),
            None => Err(err.into()),
        },
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(std::env::args().skip(1).collect()).await {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
                return std::process::ExitCode::from(u8::try_from(exit.0).unwrap_or(1));
            }
            eprintln!("oci-wasip3: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
