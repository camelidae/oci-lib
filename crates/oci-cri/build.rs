// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if let Err(err) = run() {
        eprintln!("build.rs: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    println!("cargo::rustc-check-cfg=cfg(rok_stub)");
    println!("cargo::rustc-check-cfg=cfg(rok_vm)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim");
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-env-changed=ROK_GO_TAGS");
    println!("cargo:rerun-if-env-changed=ROK_ALLOW_CROSS");
    println!("cargo:rerun-if-env-changed=ROK_DISABLE_OPTIONAL_LIBS");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=HOST_CC");
    println!("cargo:rerun-if-env-changed=CGO_ENABLED");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| e.to_string())?);
    let pointer_width = env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();
    if pointer_width != "64" {
        return Err(format!(
            "the rok ABI is LP64; target pointer width is {pointer_width}"
        ));
    }

    codegen()?;

    emit_abi(&manifest, &out_dir)?;

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" {
        // The macOS guest is opt-in via the `vm` feature (default). Without it
        // macOS links the stub. Embedding only works on a macOS build host.
        #[cfg(all(feature = "vm", target_os = "macos"))]
        if target_os == "macos" {
            let embed = oci_util::vm::EmbedConfig {
                package: None,
                kernel_env: "ROK_GUEST_KERNEL",
                initrd_env: "ROK_GUEST_INITRD",
                cache_env: "ROK_GUEST_CACHE",
                out_kernel_name: "rok-guest-vmlinuz.zst",
                out_initrd_name: "rok-guest-initramfs.zst",
                out_attestation_name: "rok-guest-attestation.json",
                protocol_version: 2,
            };
            oci_util::vm::embed_guest(&embed, &manifest, &out_dir)?;
            println!("cargo:rustc-cfg=rok_vm");
        }
        #[cfg(all(feature = "vm", not(target_os = "macos")))]
        if target_os == "macos" {
            println!(
                "cargo:warning=the macOS guest can only be embedded on a macOS host; linking the stub"
            );
        }
        compile_stub();
        return Ok(());
    }

    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    if host != target && env::var_os("ROK_ALLOW_CROSS").is_none() {
        return Err(format!(
            "refusing to cross-compile the engine c-archive from {host} to {target}. \
             Set CC and ROK_ALLOW_CROSS=1 to attempt it. Non-Linux hosts link a stub."
        ));
    }

    link_go_archive(&manifest, &out_dir, &host, &target)?;
    Ok(())
}

/// Generate the CRI v1 client/server from the vendored `api/runtime.proto`.
/// Uses the vendored protoc so builds do not depend on a system install.
fn codegen() -> Result<(), String> {
    println!("cargo:rerun-if-changed=api/runtime.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path().map_err(|err| err.to_string())?;
    // SAFETY: build scripts are single-threaded at this point.
    unsafe { env::set_var("PROTOC", &protoc) };
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["api/runtime.proto"], &["api"])
        .map_err(|err| format!("compiling api/runtime.proto: {err}"))
}

fn compile_stub() {
    let mut build = cc::Build::new();
    build.file("native/stub.c").include("shim/include");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && env::var_os("CARGO_FEATURE_VM").is_none()
    {
        build.define(
            "ROK_STUB_DETAIL",
            "\"This build has the `vm` feature off. macOS runs the engine in a Linux VM; rebuild with default features or `--features vm`.\"",
        );
    }
    build.compile("rokshim");
    println!("cargo:rustc-cfg=rok_stub");
}

fn emit_abi(manifest: &Path, out_dir: &Path) -> Result<(), String> {
    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let compiler = env::var("HOST_CC")
        .or_else(|_| {
            if host == target {
                env::var("CC")
            } else {
                Ok("cc".to_string())
            }
        })
        .unwrap_or_else(|_| "cc".to_string());
    let bin = out_dir.join("abi_size");
    let output = Command::new(&compiler)
        .arg("-I")
        .arg(manifest.join("shim/include"))
        .arg("-o")
        .arg(&bin)
        .arg(manifest.join("native/abi_size.c"))
        .output()
        .map_err(|err| format!("compiling abi_size.c with {compiler}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "abi_size.c failed to compile:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let printed = Command::new(&bin)
        .output()
        .map_err(|err| format!("running abi_size: {err}"))?;
    if !printed.status.success() {
        return Err(format!(
            "abi_size failed:\n{}",
            String::from_utf8_lossy(&printed.stderr)
        ));
    }
    std::fs::write(out_dir.join("abi_gen.rs"), &printed.stdout).map_err(|err| err.to_string())?;
    Ok(())
}

fn link_go_archive(
    manifest: &Path,
    out_dir: &Path,
    host: &str,
    target: &str,
) -> Result<(), String> {
    let go = require_go()?;
    let mut tags = vec![
        "exclude_graphdriver_btrfs".to_string(),
        "exclude_graphdriver_devicemapper".to_string(),
        "containers_image_openpgp".to_string(),
        "apparmor".to_string(),
    ];
    let skip_optional = env::var_os("ROK_DISABLE_OPTIONAL_LIBS").is_some();
    let seccomp = if skip_optional {
        false
    } else if pkg_config_exists("libseccomp") {
        true
    } else {
        println!("cargo:warning=libseccomp not found; building without the seccomp tag");
        false
    };
    if seccomp {
        tags.push("seccomp".to_string());
    }
    if let Ok(extra) = env::var("ROK_GO_TAGS") {
        tags.extend(
            extra
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }

    let archive = out_dir.join("librokshim.a");
    let mut cmd = Command::new(&go);
    cmd.current_dir(manifest.join("shim"))
        .env("CGO_ENABLED", "1")
        .env("GO111MODULE", "on")
        .arg("build")
        .arg("-buildmode=c-archive")
        .arg("-trimpath")
        .arg("-mod=readonly")
        .arg("-buildvcs=false");
    if !tags.is_empty() {
        cmd.arg("-tags").arg(tags.join(","));
    }
    cmd.arg("-o").arg(&archive).arg(".");
    if host != target {
        cmd.env("GOOS", "linux");
        cmd.env("GOARCH", goarch(target)?);
        if let Ok(cc) = env::var("CC") {
            cmd.env("CC", cc);
        }
    }
    let output = cmd
        .output()
        .map_err(|err| format!("starting `{go} build`: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "go build -buildmode=c-archive failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    cc::Build::new()
        .file("native/nsexec_early.c")
        .compile("rok_nsexec_early");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static:+whole-archive=rokshim");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-link-lib=resolv");
    if seccomp {
        link_pkg_config("libseccomp")?;
    }
    Ok(())
}

fn require_go() -> Result<String, String> {
    let go = env::var("GO").unwrap_or_else(|_| "go".to_string());
    let output = Command::new(&go)
        .arg("version")
        .output()
        .map_err(|err| format!("`{go}` not found ({err}). Linux builds need Go >= 1.26."))?;
    if !output.status.success() {
        return Err(format!("`{go} version` failed"));
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let token = text
        .split_whitespace()
        .find(|word| word.starts_with("go1.") || word.starts_with("go2."))
        .ok_or_else(|| format!("cannot parse Go version from `{text}`"))?;
    let numeric = token.trim_start_matches("go");
    let mut parts = numeric.split('.');
    let major: u32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let minor: u32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    if major < 1 || (major == 1 && minor < 26) {
        return Err(format!(
            "{token} is too old. runc v1.5.2 requires Go >= 1.26."
        ));
    }
    Ok(go)
}

fn goarch(target: &str) -> Result<&'static str, String> {
    let arch = target.split('-').next().unwrap_or("");
    match arch {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        "arm" => Ok("arm"),
        "riscv64" => Ok("riscv64"),
        "s390x" => Ok("s390x"),
        "powerpc64le" => Ok("ppc64le"),
        other => Err(format!("no GOARCH mapping for target arch {other}")),
    }
}

fn pkg_config_exists(package: &str) -> bool {
    Command::new("pkg-config")
        .args(["--exists", package])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn link_pkg_config(package: &str) -> Result<(), String> {
    let output = Command::new("pkg-config")
        .args(["--libs", package])
        .output()
        .map_err(|err| format!("pkg-config --libs {package}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "pkg-config --libs {package} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for token in text.split_whitespace() {
        if let Some(dir) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else if let Some(lib) = token.strip_prefix("-l") {
            println!("cargo:rustc-link-lib=dylib={lib}");
        }
    }
    Ok(())
}
