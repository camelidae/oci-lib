// SPDX-License-Identifier: Apache-2.0

//! Map a single-label registry host onto a Ready sandbox IP.
//!
//! The macOS guest resolver is the Apple NAT DNS server. It can resolve public
//! names and pod IPs are reachable, but it has no cluster search path, so
//! `oci-registry:5000/...` fails with `no such host`. CoreDNS answers that
//! name with a ClusterIP, and this guest has no working kube-proxy dataplane
//! to reach ClusterIPs. A Ready sandbox whose name (or app label) is the
//! registry host is already on the CNI bridge, so image pulls use that pod IP
//! via `/etc/hosts` and keep the original image reference.

use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;

use crate::store::{SandboxRecord, SandboxState};

static HOSTS_LOCK: Mutex<()> = Mutex::new(());

/// Registry hostname from a normalized image reference, when it is a single
/// DNS label we are willing to alias. Dotted names, IPs, and `localhost` stay
/// on ordinary DNS.
pub(crate) fn registry_host(image: &str) -> Option<&str> {
    let authority = image.split_once('/')?.0;
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    if host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host.contains('.')
        || host.parse::<IpAddr>().is_ok()
    {
        return None;
    }
    if !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    Some(host)
}

/// Pod IP for `host` when one Ready sandbox is that registry.
pub(crate) fn sandbox_ip_for_host(sandboxes: &[SandboxRecord], host: &str) -> Option<String> {
    let ready: Vec<_> = sandboxes
        .iter()
        .filter(|sandbox| sandbox.state == SandboxState::Ready && !sandbox_ip(sandbox).is_empty())
        .collect();
    if let Some(exact) = ready.iter().find(|sandbox| sandbox.metadata.name == host) {
        return Some(sandbox_ip(exact));
    }
    let labeled: Vec<_> = ready
        .iter()
        .filter(|sandbox| {
            sandbox.labels.get("app").map(String::as_str) == Some(host)
                || sandbox
                    .labels
                    .get("app.kubernetes.io/name")
                    .map(String::as_str)
                    == Some(host)
        })
        .collect();
    if labeled.len() == 1 {
        return Some(sandbox_ip(labeled[0]));
    }
    None
}

fn sandbox_ip(sandbox: &SandboxRecord) -> String {
    sandbox.ip.split('/').next().unwrap_or("").to_string()
}

/// Replace any existing alias for `host` and append `ip host`.
pub(crate) fn upsert_hosts(contents: &str, ip: &str, host: &str) -> String {
    let mut kept = Vec::new();
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(first) = parts.next() else {
            kept.push(line.to_string());
            continue;
        };
        if first.starts_with('#') {
            kept.push(line.to_string());
            continue;
        }
        let names: Vec<_> = parts.collect();
        if names.contains(&host) {
            continue;
        }
        kept.push(line.to_string());
    }
    kept.push(format!("{ip} {host}"));
    let mut out = kept.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub(crate) fn install_host_alias(path: &Path, host: &str, ip: &str) -> io::Result<()> {
    let _guard = HOSTS_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let current = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let updated = upsert_hosts(&current, ip, host);
    if updated == current {
        return Ok(());
    }
    fs::write(path, updated)
}

pub(crate) fn map_pull_host(sandboxes: &[SandboxRecord], image: &str) -> Result<(), crate::Error> {
    let Some(host) = registry_host(image) else {
        return Ok(());
    };
    let Some(ip) = sandbox_ip_for_host(sandboxes, host) else {
        return Ok(());
    };
    install_host_alias(Path::new("/etc/hosts"), host, &ip).map_err(|err| {
        crate::Error::new(
            crate::ErrorCode::Internal,
            "registry host alias",
            err.to_string(),
        )
    })?;
    eprintln!("oci-cri: registry host {host} -> {ip} (sandbox IP; guest DNS is not cluster DNS)");
    Ok(())
}

#[cfg(test)]
fn sandbox(name: &str, ip: &str, state: SandboxState, labels: &[(&str, &str)]) -> SandboxRecord {
    let mut label_map = std::collections::BTreeMap::new();
    for (key, value) in labels {
        label_map.insert((*key).to_string(), (*value).to_string());
    }
    SandboxRecord {
        runtime_handler: String::new(),
        id: format!("id-{name}"),
        metadata: crate::store::Metadata {
            name: name.to_string(),
            ..crate::store::Metadata::default()
        },
        labels: label_map,
        annotations: std::collections::BTreeMap::new(),
        created_at_ns: 0,
        state,
        infra_container_id: String::new(),
        ip: ip.to_string(),
        extra_ips: Vec::new(),
        host_network: false,
        linux_json: String::new(),
        log_directory: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_host_keeps_single_labels_only() {
        assert_eq!(
            registry_host("oci-registry:5000/probe:dev"),
            Some("oci-registry")
        );
        assert_eq!(registry_host("registry/app:v1"), Some("registry"));
        assert_eq!(registry_host("192.168.64.196:5000/probe:dev"), None);
        assert_eq!(registry_host("ghcr.io/geoffsee/app:dev"), None);
        assert_eq!(registry_host("localhost:5000/app:dev"), None);
        assert_eq!(registry_host("docker.io/library/busybox:latest"), None);
    }

    #[test]
    fn sandbox_ip_prefers_exact_ready_name() {
        let sandboxes = vec![
            sandbox(
                "other",
                "192.168.64.2",
                SandboxState::Ready,
                &[("app", "oci-registry")],
            ),
            sandbox(
                "oci-registry",
                "192.168.64.196/26",
                SandboxState::Ready,
                &[],
            ),
            sandbox(
                "oci-registry-old",
                "192.168.64.9",
                SandboxState::NotReady,
                &[],
            ),
        ];
        assert_eq!(
            sandbox_ip_for_host(&sandboxes, "oci-registry").as_deref(),
            Some("192.168.64.196")
        );
    }

    #[test]
    fn sandbox_ip_uses_one_labeled_pod_when_the_name_differs() {
        let sandboxes = vec![sandbox(
            "oci-registry-0",
            "192.168.64.20",
            SandboxState::Ready,
            &[("app.kubernetes.io/name", "oci-registry")],
        )];
        assert_eq!(
            sandbox_ip_for_host(&sandboxes, "oci-registry").as_deref(),
            Some("192.168.64.20")
        );
        let many = vec![
            sandbox(
                "a",
                "192.168.64.1",
                SandboxState::Ready,
                &[("app", "oci-registry")],
            ),
            sandbox(
                "b",
                "192.168.64.2",
                SandboxState::Ready,
                &[("app", "oci-registry")],
            ),
        ];
        assert_eq!(sandbox_ip_for_host(&many, "oci-registry"), None);
    }

    #[test]
    fn upsert_hosts_replaces_the_alias() {
        let first = upsert_hosts("127.0.0.1 localhost\n", "192.168.64.196", "oci-registry");
        assert_eq!(first, "127.0.0.1 localhost\n192.168.64.196 oci-registry\n");
        let second = upsert_hosts(&first, "192.168.64.200", "oci-registry");
        assert_eq!(second, "127.0.0.1 localhost\n192.168.64.200 oci-registry\n");
    }

    #[test]
    fn install_host_alias_writes_a_file() {
        let dir = std::env::temp_dir().join(format!("oci-cri-hosts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosts");
        install_host_alias(&path, "oci-registry", "192.168.64.196").unwrap();
        install_host_alias(&path, "oci-registry", "192.168.64.196").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("oci-registry").count(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
