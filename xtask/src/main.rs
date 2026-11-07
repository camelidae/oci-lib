// SPDX-License-Identifier: Apache-2.0

//! Repository tasks, run with `cargo xtask <task>`.
//!
//! `cargo xtask guest [oci-builder|oci-runner|oci-cri]...` builds the Linux
//! guest the macOS host boots and writes
//! `crates/<package>/guest/out/{vmlinuz,initramfs,attestation.json}`.
//! Run it on Linux, on the same architecture as the Mac (arm64 for Apple
//! Silicon). With no package it builds all product guests.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

mod ca;
mod guest;

#[path = "../../crates/oci-builder/src/protocol_version.rs"]
mod protocol_version;

const PACKAGES: [&str; 3] = ["oci-builder", "oci-runner", "oci-cri"];
const KERNEL_RELEASE: &str = "https://github.com/arcboxlabs/kernel/releases/download/v0.0.25";
const BUSYBOX: &str = "busybox-1.36.1";
const BUSYBOX_SHA: &str = "b8cc24c9574d809e7279c3be349795c5d5ceb6fdf19ca709f80cde50e47de314";

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("guest") => guest(&args[1..]),
        _ => Err("usage: cargo xtask guest [oci-builder|oci-runner|oci-cri]...".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn guest(packages: &[String]) -> Result<()> {
    if env::consts::OS != "linux" {
        return Err(
            "`cargo xtask guest` runs on Linux. It produces the kernel and initramfs a Mac boots."
                .into(),
        );
    }
    let packages: Vec<&str> = if packages.is_empty() {
        PACKAGES.to_vec()
    } else {
        packages.iter().map(String::as_str).collect()
    };
    for package in &packages {
        if !PACKAGES.contains(package) {
            return Err(format!(
                "unknown package {package}; expected one of {PACKAGES:?}"
            ));
        }
    }

    let root = workspace_root();
    let package_version = workspace_version(&root)?;
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let cache = target_dir.join("guest-cache");
    fs::create_dir_all(&cache).map_err(|err| format!("creating {}: {err}", cache.display()))?;

    // Debian/Ubuntu and Alpine install their maintained public roots here.
    // Fail before building anything if the build environment lacks them.
    let ca_bundle = ca::load(&Path::new("/").join(ca::BUNDLE_PATH))?;
    let kernel = fetch_kernel(&cache)?;
    let busybox = build_busybox(&cache)?;
    for package in packages {
        let bin = build_package(&root, &target_dir, package)?;
        let crate_dir = root.join("crates").join(package);
        let out = crate_dir.join("guest/out");
        fs::create_dir_all(&out).map_err(|err| format!("creating {}: {err}", out.display()))?;

        let mut archive = Archive::default();
        archive.file(ca::BUNDLE_PATH, 0o644, ca_bundle.clone());
        archive.file("bin/busybox", 0o755, read(&busybox)?);
        let init = match package {
            "oci-builder" => guest::BUILDER_INIT,
            "oci-cri" => guest::CRI_INIT,
            _ => guest::RUNNER_INIT,
        };
        archive.file("init", 0o755, init.as_bytes().to_vec());
        archive.file(
            "udhcpc.script",
            0o755,
            guest::UDHCPC_SCRIPT.as_bytes().to_vec(),
        );
        archive.file(package, 0o755, read(&bin)?);
        add_shared_libraries(&mut archive, &bin, package)?;
        if package == "oci-cri" {
            run(Command::new("sh")
                .arg(root.join("scripts/build-wasip3.sh"))
                .current_dir(&root))?;
            let runner = target_dir.join("release/oci-wasip3");
            archive.file("usr/local/bin/oci-wasip3", 0o755, read(&runner)?);
            archive.file(
                "etc/oci-cri/config.d/wasmtime.toml",
                0o644,
                include_bytes!("../../crates/oci-cri/config.d/wasmtime.toml").to_vec(),
            );
        }

        let initramfs = out.join("initramfs");
        // Hash the gzip bytes just written, not the uncompressed cpio. That is
        // the file macOS embeds and the release publishes.
        let gz_bytes = {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            gz.write_all(&archive.to_newc())
                .and_then(|()| gz.finish())
                .map_err(|err| format!("compressing {}: {err}", initramfs.display()))?
        };
        fs::write(&initramfs, &gz_bytes)
            .map_err(|err| format!("writing {}: {err}", initramfs.display()))?;
        let vmlinuz = out.join("vmlinuz");
        fs::copy(&kernel, &vmlinuz)
            .map_err(|err| format!("copying {}: {err}", kernel.display()))?;
        let attestation = out.join("attestation.json");
        let json = attestation_json(
            package,
            &package_version,
            protocol_version::PROTOCOL_VERSION,
            &sha256(&vmlinuz)?,
            &sha256_bytes(&gz_bytes),
            &archive.entries,
        );
        fs::write(&attestation, json)
            .map_err(|err| format!("writing {}: {err}", attestation.display()))?;
        eprintln!(
            "wrote {}/vmlinuz, {}, and {}",
            out.display(),
            initramfs.display(),
            attestation.display()
        );
    }
    Ok(())
}

fn workspace_version(root: &Path) -> Result<String> {
    let cargo = fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|err| format!("reading workspace Cargo.toml: {err}"))?;
    cargo
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
        .map(str::to_string)
        .ok_or_else(|| "workspace Cargo.toml has no package version".into())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits inside the workspace")
        .to_path_buf()
}

fn fetch_kernel(cache: &Path) -> Result<PathBuf> {
    let (name, sha) = match env::consts::ARCH {
        "aarch64" => (
            "kernel-arm64",
            "a122ce7a69a77408eeef9423afd9f6915828ba74a767e3e1eace635f913e02c4",
        ),
        "x86_64" => (
            "kernel-x86_64",
            "04f18c7f3a9bc6a26c601b472a7b95fd5e69f3cbbdc06622f731d7529aecf165",
        ),
        other => return Err(format!("unsupported architecture {other}")),
    };
    let dest = cache.join(name);
    fetch(&format!("{KERNEL_RELEASE}/{name}"), &dest, sha)?;
    Ok(dest)
}

fn build_busybox(cache: &Path) -> Result<PathBuf> {
    let busybox = cache.join("busybox");
    if busybox.is_file() {
        return Ok(busybox);
    }
    let tarball = cache.join(format!("{BUSYBOX}.tar.bz2"));
    fetch(
        &format!("https://busybox.net/downloads/{BUSYBOX}.tar.bz2"),
        &tarball,
        BUSYBOX_SHA,
    )?;
    let src = cache.join(BUSYBOX);
    let _ = fs::remove_dir_all(&src);
    run(Command::new("tar")
        .arg("-xjf")
        .arg(&tarball)
        .arg("-C")
        .arg(cache))?;
    run(Command::new("make").arg("defconfig").current_dir(&src))?;

    // A static binary, and no tc: busybox 1.36.1's tc applet uses CBQ netlink
    // structs that linux-libc-dev on Ubuntu 24.04 no longer ships. The guest
    // does not configure traffic control.
    let config_path = src.join(".config");
    let config = fs::read_to_string(&config_path)
        .map_err(|err| format!("reading {}: {err}", config_path.display()))?;
    let mut lines: Vec<String> = config
        .lines()
        .map(|line| match line {
            "# CONFIG_STATIC is not set" => "CONFIG_STATIC=y".to_string(),
            "CONFIG_TC=y" => "# CONFIG_TC is not set".to_string(),
            other => other.to_string(),
        })
        .collect();
    if !lines.iter().any(|line| line == "CONFIG_STATIC=y") {
        lines.push("CONFIG_STATIC=y".into());
    }
    fs::write(&config_path, lines.join("\n") + "\n")
        .map_err(|err| format!("writing {}: {err}", config_path.display()))?;

    let jobs = std::thread::available_parallelism().map_or(1, usize::from);
    let mut make = Command::new("make");
    make.arg(format!("-j{jobs}")).current_dir(&src);
    if Command::new("musl-gcc").arg("--version").output().is_ok() {
        link_musl_kernel_headers()?;
        make.arg("CC=musl-gcc");
    } else {
        eprintln!("musl-gcc was not found; linking busybox statically against the system libc.");
        eprintln!("Install musl-tools if that link fails.");
    }
    run(&mut make)?;
    fs::copy(src.join("busybox"), &busybox).map_err(|err| format!("copying busybox: {err}"))?;
    Ok(busybox)
}

/// musl-gcc does not search the glibc header tree, so linux/kd.h and
/// mtd/mtd-user.h are invisible until those directories are linked into the
/// musl include root.
fn link_musl_kernel_headers() -> Result<()> {
    let arch = env::consts::ARCH;
    let dest = PathBuf::from(format!("/usr/include/{arch}-linux-musl"));
    if !dest.is_dir() {
        return Err(format!(
            "musl-gcc is installed but {} is missing; install musl-dev",
            dest.display()
        ));
    }
    let asm = format!("/usr/include/{arch}-linux-gnu/asm");
    for (name, src) in [
        ("linux", "/usr/include/linux"),
        ("asm-generic", "/usr/include/asm-generic"),
        ("mtd", "/usr/include/mtd"),
        ("asm", asm.as_str()),
    ] {
        let link = dest.join(name);
        if link.exists() {
            continue;
        }
        if !Path::new(src).is_dir() {
            return Err(format!(
                "missing kernel headers at {src}; install linux-libc-dev"
            ));
        }
        if std::os::unix::fs::symlink(src, &link).is_err() {
            run(Command::new("sudo").arg("ln").arg("-s").arg(src).arg(&link))?;
        }
    }
    Ok(())
}

fn build_package(root: &Path, target_dir: &Path, package: &str) -> Result<PathBuf> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "--release", "--locked", "-p", package])
        .current_dir(root);
    match package {
        "oci-builder" => {
            eprintln!("building oci-builder without seccomp or apparmor");
            cmd.env("ROB_DISABLE_OPTIONAL_LIBS", "1");
        }
        "oci-cri" => {
            eprintln!("building oci-cri without seccomp");
            cmd.env("ROK_DISABLE_OPTIONAL_LIBS", "1");
        }
        _ => eprintln!("building {package}"),
    }
    run(&mut cmd)?;
    Ok(target_dir.join("release").join(package))
}

/// Copy the binary's shared libraries, as `ldd` resolves them, plus
/// libnss_files so name lookups through /etc/passwd work.
fn add_shared_libraries(archive: &mut Archive, bin: &Path, package: &str) -> Result<()> {
    let output = Command::new("ldd")
        .arg(bin)
        .output()
        .map_err(|err| format!("running ldd: {err}"))?;
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut libc_dir = None;
    for line in listing.lines() {
        if line.contains("not found") {
            return Err(format!("{package} is missing a library: {}", line.trim()));
        }
        let path = match line.split_once("=>") {
            Some((_, rest)) => rest.split_whitespace().next(),
            None => line.split_whitespace().next(),
        };
        let Some(path) = path.filter(|p| p.starts_with('/')) else {
            continue;
        };
        if line.contains("libc.so") {
            libc_dir = Path::new(path).parent().map(Path::to_path_buf);
        }
        add_library(archive, Path::new(path))?;
    }
    if let Some(dir) = libc_dir {
        let entries =
            fs::read_dir(&dir).map_err(|err| format!("reading {}: {err}", dir.display()))?;
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("libnss_files.so")
            {
                add_library(archive, &entry.path())?;
            }
        }
    }
    Ok(())
}

/// ldd often names a symlink (/lib/ld-linux-*.so.1 -> aarch64-linux-gnu/...).
/// Archive the real file and recreate the link, so the target is not left out.
fn add_library(archive: &mut Archive, path: &Path) -> Result<()> {
    let real = fs::canonicalize(path)
        .map_err(|err| format!("missing shared library {}: {err}", path.display()))?;
    let mode = fs::metadata(&real)
        .map_err(|err| format!("reading {}: {err}", real.display()))?
        .permissions()
        .mode();
    let real_name = real.to_string_lossy().trim_start_matches('/').to_string();
    archive.file(&real_name, mode & 0o7777, read(&real)?);
    if path != real {
        let link_name = path.to_string_lossy().trim_start_matches('/').to_string();
        archive.symlink(&link_name, &real.to_string_lossy());
    }
    Ok(())
}

/// Download `url` to `dest` unless a file with the expected checksum is
/// already there.
fn fetch(url: &str, dest: &Path, sha: &str) -> Result<()> {
    if dest.is_file() && sha256(dest)? == sha {
        return Ok(());
    }
    eprintln!("fetching {url}");
    let partial = dest.with_extension("partial");
    run(Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&partial)
        .arg(url))?;
    let got = sha256(&partial)?;
    if got != sha {
        let _ = fs::remove_file(&partial);
        return Err(format!(
            "checksum mismatch for {}: got {got} want {sha}",
            dest.display()
        ));
    }
    fs::rename(&partial, dest).map_err(|err| format!("renaming {}: {err}", partial.display()))
}

fn sha256(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).map_err(|err| format!("opening {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|err| format!("reading {}: {err}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher.finalize())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Bill of materials for one guest. `entries` is the archiver's member map,
/// already sorted by path, not a parse of the cpio that was just written.
/// `mode` keeps the file-type bits stored on the cpio entry.
fn attestation_json(
    package: &str,
    package_version: &str,
    protocol_version: u32,
    kernel_sha256: &str,
    initramfs_sha256: &str,
    entries: &BTreeMap<String, (u32, Vec<u8>)>,
) -> String {
    let mut out = String::from("{\n");
    out.push_str(&format!("  \"package\": {},\n", json_string(package)));
    out.push_str(&format!(
        "  \"package_version\": {},\n",
        json_string(package_version)
    ));
    out.push_str(&format!("  \"protocol_version\": {protocol_version},\n"));
    out.push_str(&format!(
        "  \"kernel_sha256\": {},\n",
        json_string(kernel_sha256)
    ));
    out.push_str(&format!(
        "  \"initramfs_sha256\": {},\n",
        json_string(initramfs_sha256)
    ));
    out.push_str("  \"members\": [\n");
    for (index, (path, (mode, data))) in entries.iter().enumerate() {
        if index != 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "    {{\"path\": {}, \"mode\": {mode}, \"size\": {}, \"sha256\": {}}}",
            json_string(path),
            data.len(),
            json_string(&sha256_bytes(data)),
        ));
    }
    if !entries.is_empty() {
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    out
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|err| format!("reading {}: {err}", path.display()))
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .map_err(|err| format!("starting {:?}: {err}", cmd.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} failed with {status}"))
    }
}

const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

/// An initramfs being assembled. Paths are relative to the guest root.
/// Parent directories are added on demand, and every entry is owned by root.
#[derive(Default)]
struct Archive {
    entries: BTreeMap<String, (u32, Vec<u8>)>,
}

impl Archive {
    fn file(&mut self, name: &str, mode: u32, data: Vec<u8>) {
        self.parents(name);
        self.entries
            .insert(name.to_string(), (S_IFREG | mode, data));
    }

    fn symlink(&mut self, name: &str, target: &str) {
        self.parents(name);
        self.entries
            .entry(name.to_string())
            .or_insert((S_IFLNK | 0o777, target.as_bytes().to_vec()));
    }

    fn parents(&mut self, name: &str) {
        let mut dir = Path::new(name).parent();
        while let Some(path) = dir.filter(|p| !p.as_os_str().is_empty()) {
            self.entries
                .entry(path.to_string_lossy().into_owned())
                .or_insert((S_IFDIR | 0o755, Vec::new()));
            dir = path.parent();
        }
    }

    /// Serialize as a newc ("070701") cpio archive, the format the kernel
    /// unpacks into the initial root filesystem. BTreeMap order puts every
    /// directory before its contents.
    fn to_newc(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (ino, (name, (mode, data))) in self.entries.iter().enumerate() {
            let nlink = if mode & S_IFDIR == S_IFDIR { 2 } else { 1 };
            newc_entry(&mut out, ino as u32 + 1, *mode, nlink, name, data);
        }
        newc_entry(&mut out, 0, 0, 1, "TRAILER!!!", &[]);
        out
    }
}

fn newc_entry(out: &mut Vec<u8>, ino: u32, mode: u32, nlink: u32, name: &str, data: &[u8]) {
    // magic, then ino mode uid gid nlink mtime filesize devmajor devminor
    // rdevmajor rdevminor namesize check, each as eight hex digits.
    let fields = [
        ino,
        mode,
        0,
        0,
        nlink,
        0,
        data.len() as u32,
        0,
        0,
        0,
        0,
        name.len() as u32 + 1,
        0,
    ];
    out.extend_from_slice(b"070701");
    for field in fields {
        out.extend_from_slice(format!("{field:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad4(out);
    out.extend_from_slice(data);
    pad4(out);
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newc_archive_round_trips_through_cpio() {
        let mut archive = Archive::default();
        archive.file("bin/tool", 0o755, b"#!/bin/sh\necho hi\n".to_vec());
        archive.file("lib/x/libz.so.1.2", 0o644, vec![7; 4099]);
        archive.symlink("lib/libz.so.1", "/lib/x/libz.so.1.2");
        let ca_bundle = ca::test_ca();
        archive.file(ca::BUNDLE_PATH, 0o644, ca_bundle.clone());

        let dir = env::temp_dir().join(format!("xtask-newc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new("cpio")
            .args(["-idm", "--quiet"])
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("cpio");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&archive.to_newc())
            .unwrap();
        assert!(child.wait().unwrap().success());

        let tool = dir.join("bin/tool");
        assert_eq!(fs::read(&tool).unwrap(), b"#!/bin/sh\necho hi\n");
        let bundle = dir.join(ca::BUNDLE_PATH);
        assert_eq!(ca::load(&bundle).unwrap(), ca_bundle);
        assert_eq!(
            fs::metadata(&bundle).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let attestation = attestation_json("oci-builder", "0.1.9", 2, "k", "i", &archive.entries);
        assert!(attestation.contains(ca::BUNDLE_PATH));
        assert!(attestation.contains(&sha256_bytes(&ca_bundle)));
        assert_eq!(
            fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read(dir.join("lib/x/libz.so.1.2")).unwrap(),
            vec![7; 4099]
        );
        assert_eq!(
            fs::read_link(dir.join("lib/libz.so.1")).unwrap(),
            Path::new("/lib/x/libz.so.1.2")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn attestation_lists_sorted_members_and_hashes_their_bytes() {
        let mut archive = Archive::default();
        archive.file("z", 0o644, b"z".to_vec());
        archive.symlink("a/link", "target");
        archive.file("a/b", 0o755, b"hello".to_vec());
        let json = attestation_json(
            "oci-runner",
            "0.1.9",
            2,
            "kernelhash",
            "initramfshash",
            &archive.entries,
        );
        assert!(json.contains("\"package\": \"oci-runner\""));
        assert!(json.contains("\"kernel_sha256\": \"kernelhash\""));
        assert!(json.contains("\"initramfs_sha256\": \"initramfshash\""));
        let dir = json.find("\"path\": \"a\"").expect("directory member");
        let file = json.find("\"path\": \"a/b\"").expect("file member");
        let link = json.find("\"path\": \"a/link\"").expect("symlink member");
        let last = json.find("\"path\": \"z\"").expect("z member");
        assert!(dir < file && file < link && link < last);
        assert!(json.contains(&format!("\"mode\": {}", S_IFDIR | 0o755)));
        assert!(json.contains(&format!("\"mode\": {}", S_IFREG | 0o755)));
        assert!(json.contains(&format!("\"mode\": {}", S_IFLNK | 0o777)));
        assert!(json.contains(&format!("\"mode\": {}", S_IFREG | 0o644)));
        assert!(json.contains("\"size\": 5"));
        assert!(json.contains("\"size\": 6"));
        let hello = sha256_bytes(b"hello");
        assert_eq!(
            hello,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(json.contains(&hello));
        assert!(json.contains(&sha256_bytes(b"target")));
        assert!(json.contains(&sha256_bytes(b"")));
    }

    #[test]
    fn attestation_escapes_member_paths() {
        let mut entries = BTreeMap::new();
        entries.insert("a\"b\\c".to_string(), (0o644, b"x".to_vec()));
        let json = attestation_json("oci-builder", "0.1.9", 2, "k", "i", &entries);
        let escaped = json_string("a\"b\\c");
        assert_eq!(escaped, "\"a\\\"b\\\\c\"");
        assert!(json.contains(&format!("\"path\": {escaped}")));
    }
}
