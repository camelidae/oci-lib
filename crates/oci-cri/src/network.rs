// SPDX-License-Identifier: Apache-2.0

//! CNI networking for pod sandboxes.
//!
//! Implements the CNI plugin protocol directly: plugins are executables in
//! the configured bin directory, invoked with `CNI_COMMAND=ADD|DEL|CHECK`
//! environment and the network configuration on stdin. The sandbox's network
//! namespace is the infra container's (`/proc/<pid>/ns/net` once running).

use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::api::PortMapping;
use crate::config::Config;
use crate::error::{Error, ErrorCode, Result};

/// Result of a successful CNI ADD.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CniResult {
    /// IP addresses assigned to the sandbox (`10.0.0.2/24` form).
    pub ips: Vec<String>,
    /// Routes from the plugin result (kept for status/debugging).
    pub routes: Vec<String>,
}

/// Whether CNI is configured (a conf dir with at least one config file).
pub fn configured(config: &Config) -> bool {
    config.cni_conf_dir.is_some()
}

/// IPv4 network reserved for pod interfaces inside the macOS CRI guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestPodNetwork {
    /// Network address of the selected quarter of the guest uplink subnet.
    pub subnet: Ipv4Addr,
    /// Prefix length of the pod subnet.
    pub prefix: u8,
    /// Bridge gateway assigned to the first usable address.
    pub gateway: Ipv4Addr,
    /// First address host-local may assign to a pod.
    pub range_start: Ipv4Addr,
    /// Last address host-local may assign to a pod.
    pub range_end: Ipv4Addr,
}

/// Choose the highest quarter-subnet that does not contain the guest's own
/// uplink address. The quartering leaves the other three quarters available
/// to the host's NAT/DHCP network.
pub fn guest_pod_network(guest_ip: Ipv4Addr, guest_prefix: u8) -> Option<GuestPodNetwork> {
    if guest_prefix > 28 {
        return None;
    }

    let host_bits = 32 - guest_prefix;
    let quarter_bits = host_bits - 2;
    let quarter_size = 1_u32 << quarter_bits;
    let address = u32::from(guest_ip);
    let network = if guest_prefix == 0 {
        0
    } else {
        address & (u32::MAX << host_bits)
    };
    let guest_quarter = (address - network) / quarter_size;
    let quarter = (0..4).rev().find(|quarter| *quarter != guest_quarter)?;
    let subnet = network + quarter * quarter_size;
    let broadcast = subnet + quarter_size - 1;

    Some(GuestPodNetwork {
        subnet: Ipv4Addr::from(subnet),
        prefix: guest_prefix + 2,
        gateway: Ipv4Addr::from(subnet + 1),
        range_start: Ipv4Addr::from(subnet + 2),
        range_end: Ipv4Addr::from(broadcast - 1),
    })
}

/// Build the CNI bridge configuration for a pod subnet on the guest uplink.
pub fn guest_conflist(network: GuestPodNetwork, mtu: u32) -> Value {
    let subnet = format!("{}/{}", network.subnet, network.prefix);
    serde_json::json!({
        "cniVersion": "1.0.0",
        "name": "oci-cri-guest",
        "plugins": [
            { "type": "loopback" },
            {
                "type": "bridge",
                "bridge": "cni0",
                "isGateway": true,
                "ipMasq": false,
                "hairpinMode": true,
                "mtu": mtu,
                "ipam": {
                    "type": "host-local",
                    "ranges": [[{
                        "subnet": subnet,
                        "rangeStart": network.range_start.to_string(),
                        "rangeEnd": network.range_end.to_string()
                    }]],
                    "routes": [{ "dst": "0.0.0.0/0" }]
                }
            }
        ]
    })
}

/// Load the merged CNI configuration (first `.conflist`, else first `.conf`)
/// from the configured directory.
pub fn load_config(config: &Config) -> Result<Option<Value>> {
    let Some(dir) = &config.cni_conf_dir else {
        return Ok(None);
    };
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| {
            Error::new(
                ErrorCode::Prerequisite,
                format!("reading {}: {err}", dir.display()),
                "",
            )
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "conf" || e == "conflist")
                .unwrap_or(false)
        })
        .collect();
    names.sort();
    let Some(path) = names.into_iter().next() else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path).map_err(|err| {
        Error::new(
            ErrorCode::Prerequisite,
            format!("reading {}: {err}", path.display()),
            "",
        )
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|err| {
        Error::new(
            ErrorCode::Prerequisite,
            format!("parsing {}: {err}", path.display()),
            "",
        )
    })?;
    // A conflist already carries its plugin list; a single conf is wrapped.
    if value.get("plugins").is_some() {
        Ok(Some(value))
    } else {
        let name = value
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("oci-cri")
            .to_string();
        Ok(Some(serde_json::json!({
            "cniVersion": value.get("cniVersion").cloned().unwrap_or(serde_json::json!("1.0.0")),
            "name": name,
            "disableCheck": true,
            "plugins": [value],
        })))
    }
}

/// Run CNI ADD for a sandbox. `port_mappings` are embedded in the config the
/// plugins see (the `portmap` plugin consumes them).
pub fn setup(
    config: &Config,
    sandbox_id: &str,
    netns: &Path,
    port_mappings: &[PortMapping],
) -> Result<CniResult> {
    let Some(mut conf) = load_config(config)? else {
        return Ok(CniResult::default());
    };
    if !port_mappings.is_empty() {
        let mappings: Vec<Value> = port_mappings
            .iter()
            .filter(|m| m.host_port > 0)
            .map(|m| {
                serde_json::json!({
                    "hostPort": m.host_port,
                    "containerPort": m.container_port,
                    "protocol": protocol_name(m.protocol),
                    "hostIP": m.host_ip,
                })
            })
            .collect();
        if let Some(plugins) = conf["plugins"].as_array_mut() {
            for plugin in plugins.iter_mut() {
                let kind = plugin["type"].as_str().unwrap_or("");
                if kind == "portmap" {
                    plugin["runtimeConfig"] = serde_json::json!({"portMappings": mappings});
                }
            }
        }
    }
    let output = invoke(config, "ADD", sandbox_id, netns, &conf)?;
    parse_result(&output)
}

/// Run CNI DEL for a sandbox. Best effort: missing plugins or an already
/// removed namespace are not errors.
pub fn teardown(config: &Config, sandbox_id: &str, netns: &Path) {
    let Ok(Some(conf)) = load_config(config) else {
        return;
    };
    let _ = invoke(config, "DEL", sandbox_id, netns, &conf);
}

/// Run CNI CHECK (used by `Status` to report network readiness).
pub fn check(config: &Config, sandbox_id: &str, netns: &Path) -> bool {
    let Ok(Some(conf)) = load_config(config) else {
        return false;
    };
    invoke(config, "CHECK", sandbox_id, netns, &conf).is_ok()
}

fn invoke(
    config: &Config,
    command: &str,
    sandbox_id: &str,
    netns: &Path,
    conf: &Value,
) -> Result<Vec<u8>> {
    let name = conf
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or(config.runtime_name.as_str())
        .to_string();
    let plugins = conf["plugins"].as_array().cloned().unwrap_or_default();
    if plugins.is_empty() {
        return Ok(Vec::new());
    }
    let mut path_search: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &config.cni_bin_dir {
        path_search.push(dir.clone());
    }
    path_search.extend(config.cni_plugin_dirs.iter().cloned());
    let cni_path = path_search
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let mut result = Vec::new();
    let list: Vec<&Value> = if command == "DEL" {
        plugins.iter().rev().collect()
    } else {
        plugins.iter().collect()
    };
    for plugin in list {
        let kind = plugin["type"].as_str().unwrap_or_default();
        let bin = path_search
            .iter()
            .map(|dir| dir.join(kind))
            .find(|p| p.is_file());
        let Some(bin) = bin else {
            if command == "DEL" {
                continue;
            }
            return Err(Error::new(
                ErrorCode::Prerequisite,
                format!("CNI plugin {kind} not found in {cni_path}"),
                "",
            ));
        };
        // Each plugin gets the previous result chained as prevResult.
        let mut input = plugin.clone();
        input["name"] = Value::String(name.clone());
        input["cniVersion"] = conf
            .get("cniVersion")
            .cloned()
            .unwrap_or_else(|| serde_json::json!("1.0.0"));
        if !result.is_empty() {
            if let Ok(prev) = serde_json::from_slice::<Value>(&result) {
                input["prevResult"] = prev;
            }
        }
        let mut child = Command::new(&bin)
            .env("CNI_COMMAND", command)
            .env("CNI_CONTAINERID", sandbox_id)
            .env("CNI_NETNS", netns.to_string_lossy().into_owned())
            .env("CNI_IFNAME", "eth0")
            .env("CNI_PATH", &cni_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                Error::new(
                    ErrorCode::Prerequisite,
                    format!("running CNI plugin {kind}: {err}"),
                    "",
                )
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.to_string().as_bytes());
        }
        let output = child.wait_with_output().map_err(|err| {
            Error::new(
                ErrorCode::Internal,
                format!("waiting for CNI plugin {kind}: {err}"),
                "",
            )
        })?;
        if !output.status.success() {
            if command == "DEL" {
                continue;
            }
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            return Err(Error::new(
                ErrorCode::Run,
                format!("CNI {command} plugin {kind} failed"),
                if stderr.is_empty() { stdout } else { stderr },
            ));
        }
        result = output.stdout;
    }
    Ok(result)
}

fn parse_result(output: &[u8]) -> Result<CniResult> {
    if output.is_empty() {
        return Ok(CniResult::default());
    }
    let value: Value = serde_json::from_slice(output).map_err(|err| {
        Error::new(
            ErrorCode::Internal,
            format!("parsing CNI result: {err}"),
            "",
        )
    })?;
    let ips = value["ips"]
        .as_array()
        .map(|ips| {
            ips.iter()
                .filter_map(|ip| ip["address"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let routes = value["routes"]
        .as_array()
        .map(|routes| {
            routes
                .iter()
                .filter_map(|r| r["dst"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(CniResult { ips, routes })
}

fn protocol_name(protocol: i32) -> &'static str {
    if protocol == crate::api::Protocol::Udp as i32 {
        "udp"
    } else if protocol == crate::api::Protocol::Sctp as i32 {
        "sctp"
    } else {
        "tcp"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_highest_quarter_outside_guest_address() {
        let network = guest_pod_network(Ipv4Addr::new(192, 168, 64, 2), 24).unwrap();
        assert_eq!(network.subnet, Ipv4Addr::new(192, 168, 64, 192));
        assert_eq!(network.prefix, 26);
        assert_eq!(network.gateway, Ipv4Addr::new(192, 168, 64, 193));
        assert_eq!(network.range_start, Ipv4Addr::new(192, 168, 64, 194));
        assert_eq!(network.range_end, Ipv4Addr::new(192, 168, 64, 254));
    }

    #[test]
    fn skips_highest_quarter_when_it_contains_guest_address() {
        let network = guest_pod_network(Ipv4Addr::new(192, 168, 64, 230), 24).unwrap();
        assert_eq!(network.subnet, Ipv4Addr::new(192, 168, 64, 128));
        assert_eq!(network.prefix, 26);
    }

    #[test]
    fn supports_larger_uplink_subnets() {
        let network = guest_pod_network(Ipv4Addr::new(10, 1, 2, 3), 16).unwrap();
        assert_eq!(network.subnet, Ipv4Addr::new(10, 1, 192, 0));
        assert_eq!(network.prefix, 18);
    }

    #[test]
    fn rejects_uplink_subnets_without_four_safe_quarters() {
        assert!(guest_pod_network(Ipv4Addr::new(192, 168, 1, 2), 29).is_none());
        assert!(guest_pod_network(Ipv4Addr::new(192, 168, 1, 2), 31).is_none());
    }

    #[test]
    fn conflist_chains_loopback_and_bridge_with_host_local_ipam() {
        let network = guest_pod_network(Ipv4Addr::new(192, 168, 64, 2), 24).unwrap();
        let config = guest_conflist(network, 1280);
        assert_eq!(config["cniVersion"], "1.0.0");
        let plugins = config["plugins"].as_array().unwrap();
        assert_eq!(plugins[0]["type"], "loopback");
        assert_eq!(plugins[1]["type"], "bridge");
        assert_eq!(plugins[1]["bridge"], "cni0");
        assert_eq!(plugins[1]["isGateway"], true);
        assert_eq!(plugins[1]["ipMasq"], false);
        assert_eq!(plugins[1]["hairpinMode"], true);
        assert_eq!(plugins[1]["mtu"], 1280);
        assert_eq!(plugins[1]["ipam"]["type"], "host-local");
        assert_eq!(
            plugins[1]["ipam"]["ranges"][0][0]["subnet"],
            "192.168.64.192/26"
        );
        assert_eq!(plugins[1]["ipam"]["routes"][0]["dst"], "0.0.0.0/0");
    }

    #[test]
    fn parses_ips_from_a_result() {
        let result = parse_result(
            br#"{"cniVersion":"1.0.0","ips":[{"address":"10.0.0.7/24"}],"routes":[{"dst":"0.0.0.0/0"}]}"#,
        )
        .unwrap();
        assert_eq!(result.ips, ["10.0.0.7/24"]);
        assert_eq!(result.routes, ["0.0.0.0/0"]);
    }

    #[test]
    fn an_unconfigured_runtime_reports_no_cni() {
        let config = Config::default();
        assert!(!configured(&config));
        assert!(load_config(&config).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn runs_a_fake_plugin_for_add() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rok-cni-{}", crate::store::new_id()));
        let bin_dir = dir.join("bin");
        let conf_dir = dir.join("conf");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&conf_dir).unwrap();
        let plugin = bin_dir.join("fakebridge");
        std::fs::write(
            &plugin,
            "#!/bin/sh\ngrep -q '\"cniVersion\":\"1.0.0\"' || exit 1\necho '{\"cniVersion\":\"1.0.0\",\"ips\":[{\"address\":\"10.1.0.3/24\"}]}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            conf_dir.join("10-test.conflist"),
            r#"{"cniVersion":"1.0.0","name":"testnet","plugins":[{"type":"fakebridge"}]}"#,
        )
        .unwrap();
        let config = Config {
            cni_bin_dir: Some(bin_dir),
            cni_conf_dir: Some(conf_dir),
            ..Config::default()
        };
        let result = setup(&config, "sandbox1", Path::new("/proc/self/ns/net"), &[]).unwrap();
        assert_eq!(result.ips, ["10.1.0.3/24"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[cfg(unix)]
    #[test]
    fn host_ports_are_passed_as_cni_runtime_capabilities() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("portmap");
        let captured = dir.path().join("request.json");
        std::fs::write(
            &plugin,
            format!(
                "#!/bin/sh\ncat > '{}'\necho '{{\"cniVersion\":\"1.0.0\"}}'\n",
                captured.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let configs = dir.path().join("conf");
        std::fs::create_dir(&configs).unwrap();
        std::fs::write(configs.join("net.conflist"), r#"{"cniVersion":"1.0.0","name":"test","plugins":[{"type":"portmap","capabilities":{"portMappings":true}}]}"#).unwrap();
        let config = Config {
            cni_bin_dir: Some(dir.path().into()),
            cni_conf_dir: Some(configs),
            ..Config::default()
        };
        setup(
            &config,
            "pod",
            Path::new("/proc/self/ns/net"),
            &[
                PortMapping {
                    host_port: 8080,
                    container_port: 80,
                    ..Default::default()
                },
                PortMapping {
                    container_port: 90,
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        let input: Value = serde_json::from_slice(&std::fs::read(captured).unwrap()).unwrap();
        assert_eq!(
            input["runtimeConfig"]["portMappings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(input["runtimeConfig"]["portMappings"][0]["hostPort"], 8080);
        assert!(input.get("portMappings").is_none());
    }
}
