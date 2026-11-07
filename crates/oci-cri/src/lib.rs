// SPDX-License-Identifier: Apache-2.0

//! A vendor-agnostic CRI v1 runtime backed by libcontainer.
//!
//! Callers supply branding via [`Config::runtime_name`] and socket/root
//! paths, then call [`server::serve`] or [`server::serve_with_engine`].
//! On Linux the Go shim (libcontainer + containers/image) is linked
//! in-process. On macOS (default `vm` feature) the same Linux engine runs
//! inside an Apple Virtualization guest and the host proxies the CRI unix
//! socket over virtio-vsock. Other platforms link a stub and return
//! [`ErrorCode::Unsupported`].
//!
//! Call [`startup`] before spawning threads or parsing arguments: on Linux
//! the libcontainer re-exec child is dispatched from the Go runtime before
//! `main` continues. On macOS it self-signs the virtualization entitlement
//! when needed.

#![warn(missing_docs)]
// The vendored CRI proto's generated doc comments trip this in tonic output.
#![allow(clippy::doc_lazy_continuation)]

#[allow(missing_docs)]
pub mod api;
pub mod config;
pub mod convert;
mod dns;
pub mod engine;
pub mod error;
mod image_reference;
mod image_service;
pub mod logs;
pub mod network;
pub mod proto;
pub mod server;
pub mod service;
pub mod store;
pub mod streaming;

mod ffi;
#[cfg(rok_vm)]
mod guest_config;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(rok_vm)]
mod macos;
#[cfg(any(not(any(target_os = "linux", rok_vm)), test))]
mod other;
mod protocol_version;
#[cfg(target_os = "linux")]
pub mod serve;

#[cfg(target_os = "linux")]
pub use linux::LinuxEngine;

#[cfg(feature = "test-support")]
pub mod test_support;

pub use config::Config;
pub use error::{Error, ErrorCode};
pub use service::CriService;

/// Check for a libcontainer re-exec, then return.
///
/// On Linux this calls into the shim. The `init` child never reaches this
/// function. On macOS this does not boot a guest, so `--help` stays local;
/// if the binary lacks the virtualization entitlement it signs itself ad hoc
/// (`ROK_NO_SELF_SIGN=1` turns that off). On other platforms this is a no-op
/// success (product layers may still refuse to serve).
pub fn startup() -> Result<(), Error> {
    #[cfg(target_os = "linux")]
    {
        linux::startup()
    }
    #[cfg(rok_vm)]
    {
        oci_util::vm::ensure_entitlement(guest_config::GUEST.no_self_sign_env);
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", rok_vm)))]
    {
        Ok(())
    }
}

/// Report whether this host can serve as a CRI runtime.
pub fn diagnose(config: &Config) -> Result<String, Error> {
    #[cfg(target_os = "linux")]
    {
        let _ = config;
        linux::diagnose()
    }
    #[cfg(rok_vm)]
    {
        macos::diagnose(config)
    }
    #[cfg(not(any(target_os = "linux", rok_vm)))]
    {
        let _ = config;
        Err(other::unsupported())
    }
}

/// Build the Linux engine rooted at the config's root directory, opening
/// the runtime if it is not open yet. Used by guest agents and tests.
#[cfg(target_os = "linux")]
pub fn linux_engine(config: &Config) -> linux::LinuxEngine {
    let _ = linux::open();
    linux::LinuxEngine::new(config.root.clone(), config.state_root.clone())
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn rok_nsexec_keep() -> i32;
}

#[cfg(target_os = "linux")]
#[used]
static ROK_NSEXEC_KEEP: unsafe extern "C" fn() -> i32 = rok_nsexec_keep;
