// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use oci_cri::{Config, Error, startup};

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => exit_code(code),
        Err(err) => {
            eprintln!("{err}");
            exit_code(err.code().as_exit())
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code)
        .map(ExitCode::from)
        .unwrap_or(ExitCode::from(1))
}

fn real_main() -> Result<i32, Error> {
    // The libcontainer re-exec child is dispatched before this returns, and
    // its argv must not be parsed as a CLI.
    startup()?;
    // Guest init invokes `--engine-serve` before clap; keep this ahead of parse.
    if std::env::args_os().any(|arg| arg == "--engine-serve") {
        #[cfg(target_os = "linux")]
        {
            return oci_cri::serve::engine_serve();
        }
        #[cfg(not(target_os = "linux"))]
        {
            return Err(Error::new(
                oci_cri::ErrorCode::Unsupported,
                "--engine-serve is only available inside the Linux guest",
                "",
            ));
        }
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => {
            oci_cri::server::serve(args.into_config())?;
            Ok(0)
        }
        Command::Diagnose(args) => {
            let report = oci_cri::diagnose(&args.into_config())?;
            print!("{report}");
            Ok(0)
        }
        Command::Version => {
            println!(
                "oci-cri {} (CRI {}, libcontainer engine)",
                env!("CARGO_PKG_VERSION"),
                oci_cri::config::API_VERSION
            );
            Ok(0)
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "oci-cri",
    about = "Vendor-agnostic CRI v1 runtime backed by libcontainer",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve CRI gRPC on the unix socket (point kubelet at it).
    Serve(RuntimeArgs),
    /// Print whether this host can run containers.
    Diagnose(RuntimeArgs),
    /// Print version information.
    Version,
}

/// Flags shared by `serve` and `diagnose`.
#[derive(Debug, Clone, Args)]
struct RuntimeArgs {
    /// Directory of executable runtime handler TOML files.
    #[arg(long, default_value = "/etc/oci-cri/config.d")]
    runtime_config_dir: PathBuf,
    /// CRI unix socket path.
    #[arg(long, default_value = "/run/oci-cri/oci-cri.sock")]
    socket: PathBuf,
    /// Image store, state, and rootfs root.
    #[arg(long, default_value = "/var/lib/oci-cri")]
    root: PathBuf,
    /// libcontainer state directory.
    #[arg(long, default_value = "/run/oci-cri/state")]
    state_root: PathBuf,
    /// Pause image used for pod sandboxes.
    #[arg(long, default_value = "registry.k8s.io/pause:3.10.1")]
    sandbox_image: String,
    /// CNI plugin binary directory.
    #[arg(long)]
    cni_bin_dir: Option<PathBuf>,
    /// CNI configuration directory.
    #[arg(long)]
    cni_conf_dir: Option<PathBuf>,
    /// Streaming server bind address (Exec/Attach/PortForward).
    #[arg(long, default_value = "127.0.0.1")]
    streaming_address: String,
    /// Streaming server port (0 = ephemeral).
    #[arg(long, default_value_t = 0)]
    streaming_port: u16,
    /// Pod CIDR, when known.
    #[arg(long, default_value = "")]
    pod_cidr: String,
    /// Allow insecure (HTTP / unverified TLS) registries when pulling.
    #[arg(long)]
    insecure_registries: bool,
}

impl RuntimeArgs {
    fn into_config(self) -> Config {
        Config {
            runtime_config_dir: Some(self.runtime_config_dir),
            runtimes: Default::default(),
            runtime_name: oci_cri::config::RUNTIME_NAME.to_string(),
            socket: self.socket,
            root: self.root,
            state_root: self.state_root,
            sandbox_image: self.sandbox_image,
            cni_bin_dir: self.cni_bin_dir,
            cni_conf_dir: self.cni_conf_dir,
            cni_plugin_dirs: Vec::new(),
            streaming_address: self.streaming_address,
            streaming_port: self.streaming_port,
            pod_cidr: self.pod_cidr,
            insecure_registries: self.insecure_registries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_serve_flags() {
        let cli = Cli::try_parse_from([
            "oci-cri",
            "serve",
            "--socket",
            "/tmp/rok.sock",
            "--root",
            "/tmp/rok",
            "--sandbox-image",
            "registry.k8s.io/pause:3.10.1",
            "--cni-conf-dir",
            "/etc/cni/net.d",
            "--insecure-registries",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("serve");
        };
        let config = args.into_config();
        assert_eq!(config.socket, PathBuf::from("/tmp/rok.sock"));
        assert!(config.insecure_registries);
        assert_eq!(config.cni_conf_dir, Some(PathBuf::from("/etc/cni/net.d")));
        assert_eq!(config.runtime_name, "oci-cri");
    }

    #[test]
    fn diagnose_needs_no_extra_args() {
        let cli = Cli::try_parse_from(["oci-cri", "diagnose"]).unwrap();
        assert!(matches!(cli.command, Command::Diagnose(_)));
    }
}
