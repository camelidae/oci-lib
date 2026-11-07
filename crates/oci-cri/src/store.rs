// SPDX-License-Identifier: Apache-2.0

//! Persistent sandbox/container records.
//!
//! Records are JSON files under `<root>/sandboxes` and `<root>/containers`,
//! named by ID. The daemon reloads them at startup so `List*` and `*Status`
//! stay consistent across restarts; containers found running on disk are
//! reconciled against the engine by the service layer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};

/// Pod identity mirrored from `PodSandboxMetadata` (prost types are not
/// serialisable, so the store keeps its own copy).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    /// Object name.
    pub name: String,
    /// Object UID.
    pub uid: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// Creation attempt counter.
    pub attempt: u32,
}

/// Sandbox lifecycle state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SandboxState {
    /// Namespaces and networking are up.
    Ready,
    /// Stopped or not yet fully created.
    NotReady,
}

/// One persisted pod sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRecord {
    /// CRI sandbox ID.
    pub id: String,
    /// Pod metadata.
    pub metadata: Metadata,
    /// Labels from the sandbox config.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Annotations from the sandbox config.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// Creation time, nanoseconds since the Unix epoch.
    pub created_at_ns: i64,
    /// Lifecycle state.
    pub state: SandboxState,
    /// Infra (pause) container ID backing this sandbox.
    pub infra_container_id: String,
    /// Primary sandbox IP, empty when networking is not configured.
    #[serde(default)]
    pub ip: String,
    /// Additional sandbox IPs.
    #[serde(default)]
    pub extra_ips: Vec<String>,
    /// Whether the sandbox uses the host network namespace.
    #[serde(default)]
    pub host_network: bool,
    /// Serialised `LinuxPodSandboxConfig` bits needed to rebuild status:
    /// namespace options and security context are kept as JSON.
    #[serde(default)]
    pub linux_json: String,
    /// Directory kubelet expects container logs under.
    #[serde(default)]
    pub log_directory: String,
}

/// Container lifecycle state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContainerState {
    /// Created but not started.
    Created,
    /// Running.
    Running,
    /// Exited (see `exit_code`).
    Exited,
    /// Unknown (engine unreachable).
    Unknown,
}

/// One persisted container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRecord {
    /// CRI container ID.
    pub id: String,
    /// Owning sandbox ID.
    pub sandbox_id: String,
    /// Container metadata (name/attempt).
    pub metadata: Metadata,
    /// Image reference as requested.
    pub image: String,
    /// Canonical image reference returned by the engine.
    #[serde(default)]
    pub image_ref: String,
    /// Labels from the container config.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Annotations from the container config.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// Log file path from the container config (CRI log format).
    #[serde(default)]
    pub log_path: String,
    /// Whether the container allocated a terminal.
    #[serde(default)]
    pub tty: bool,
    /// Whether stdin is attached.
    #[serde(default)]
    pub stdin: bool,
    /// Close stdin after one attach.
    #[serde(default)]
    pub stdin_once: bool,
    /// Lifecycle state.
    pub state: ContainerState,
    /// Creation time (ns since epoch).
    pub created_at_ns: i64,
    /// Start time (ns since epoch), `0` when never started.
    #[serde(default)]
    pub started_at_ns: i64,
    /// Finish time (ns since epoch), `0` while not finished.
    #[serde(default)]
    pub finished_at_ns: i64,
    /// Exit code once finished.
    #[serde(default = "minus_one")]
    pub exit_code: i32,
    /// Human-readable state reason (`Completed`, `Error`, ...).
    #[serde(default)]
    pub reason: String,
    /// Human-readable state message.
    #[serde(default)]
    pub message: String,
    /// OCI runtime spec JSON used at create time.
    #[serde(default)]
    pub spec_json: String,
    /// Original CRI ContainerConfig, encoded with protobuf for status reporting.
    #[serde(default)]
    pub config_bytes: Vec<u8>,
    /// Rootfs path returned by the engine.
    #[serde(default)]
    pub rootfs: String,
    /// Stop signal name from the image/container config.
    #[serde(default)]
    pub stop_signal: String,
}

fn minus_one() -> i32 {
    -1
}

struct Inner {
    sandboxes: BTreeMap<String, SandboxRecord>,
    containers: BTreeMap<String, ContainerRecord>,
}

/// The record store: in-memory maps backed by one JSON file per record.
pub struct Store {
    sandboxes_dir: PathBuf,
    containers_dir: PathBuf,
    inner: RwLock<Inner>,
}

impl Store {
    /// Open (creating if needed) a store rooted at the config directories.
    pub fn open(sandboxes_dir: PathBuf, containers_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&sandboxes_dir).map_err(io_err)?;
        std::fs::create_dir_all(&containers_dir).map_err(io_err)?;
        let store = Self {
            sandboxes_dir,
            containers_dir,
            inner: RwLock::new(Inner {
                sandboxes: BTreeMap::new(),
                containers: BTreeMap::new(),
            }),
        };
        store.reload()?;
        Ok(store)
    }

    /// Reload every record from disk, replacing the in-memory view.
    pub fn reload(&self) -> Result<()> {
        let sandboxes = load_dir::<SandboxRecord>(&self.sandboxes_dir, |r| r.id.clone())?;
        let containers = load_dir::<ContainerRecord>(&self.containers_dir, |r| r.id.clone())?;
        let mut inner = self.inner.write().unwrap_or_else(|p| p.into_inner());
        inner.sandboxes = sandboxes;
        inner.containers = containers;
        Ok(())
    }

    /// Insert or replace a sandbox record and persist it.
    pub fn put_sandbox(&self, record: SandboxRecord) -> Result<()> {
        write_record(&self.sandboxes_dir, &record.id, &record)?;
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .sandboxes
            .insert(record.id.clone(), record);
        Ok(())
    }

    /// Fetch a sandbox by ID.
    pub fn sandbox(&self, id: &str) -> Option<SandboxRecord> {
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .sandboxes
            .get(id)
            .cloned()
    }

    /// All sandboxes.
    pub fn sandboxes(&self) -> Vec<SandboxRecord> {
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .sandboxes
            .values()
            .cloned()
            .collect()
    }

    /// Delete a sandbox record.
    pub fn remove_sandbox(&self, id: &str) -> Result<()> {
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .sandboxes
            .remove(id);
        remove_record(&self.sandboxes_dir, id)
    }

    /// Insert or replace a container record and persist it.
    pub fn put_container(&self, record: ContainerRecord) -> Result<()> {
        write_record(&self.containers_dir, &record.id, &record)?;
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .containers
            .insert(record.id.clone(), record);
        Ok(())
    }

    /// Fetch a container by ID.
    pub fn container(&self, id: &str) -> Option<ContainerRecord> {
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .containers
            .get(id)
            .cloned()
    }

    /// All containers.
    pub fn containers(&self) -> Vec<ContainerRecord> {
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .containers
            .values()
            .cloned()
            .collect()
    }

    /// Containers belonging to one sandbox.
    pub fn containers_in(&self, sandbox_id: &str) -> Vec<ContainerRecord> {
        self.containers()
            .into_iter()
            .filter(|c| c.sandbox_id == sandbox_id)
            .collect()
    }

    /// Delete a container record.
    pub fn remove_container(&self, id: &str) -> Result<()> {
        self.inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .containers
            .remove(id);
        remove_record(&self.containers_dir, id)
    }
}

fn load_dir<T: for<'de> Deserialize<'de>>(
    dir: &Path,
    id_of: impl Fn(&T) -> String,
) -> Result<BTreeMap<String, T>> {
    let mut out = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(io_err(err)),
    };
    for entry in entries {
        let entry = entry.map_err(io_err)?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(io_err)?;
        let record: T = serde_json::from_str(&text).map_err(|err| {
            Error::new(
                ErrorCode::Internal,
                format!("parsing {}: {err}", path.display()),
                "",
            )
        })?;
        out.insert(id_of(&record), record);
    }
    Ok(out)
}

fn write_record<T: Serialize>(dir: &Path, id: &str, record: &T) -> Result<()> {
    let path = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!(".{id}.json.partial"));
    let text = serde_json::to_string_pretty(record).map_err(|err| {
        Error::new(
            ErrorCode::Internal,
            format!("serialising record: {err}"),
            "",
        )
    })?;
    std::fs::write(&tmp, text).map_err(io_err)?;
    std::fs::rename(&tmp, &path).map_err(io_err)?;
    Ok(())
}

fn remove_record(dir: &Path, id: &str) -> Result<()> {
    let path = dir.join(format!("{id}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_err(err)),
    }
}

fn io_err(err: std::io::Error) -> Error {
    Error::new(ErrorCode::Internal, err.to_string(), "")
}

/// Current time in nanoseconds since the Unix epoch.
pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Generate an ID from time, pid, and a counter (hex, 64 chars like
/// containerd/CRI-O IDs so kubelet tooling accepts it).
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!(
        "{}-{}-{}-{:x}",
        now_ns(),
        std::process::id(),
        n,
        (now_ns() as u64) ^ (n.wrapping_mul(0x9e37_79b9_7f4a_7c15))
    );
    // FNV-1a expansion to 64 hex chars; uniqueness comes from the seed.
    let mut out = String::with_capacity(64);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let bytes = seed.as_bytes();
    for i in 0..32 {
        for &b in bytes {
            hash ^= (b as u64).wrapping_add(i);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        out.push_str(&format!("{hash:02x}")[..2]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(id: &str) -> SandboxRecord {
        SandboxRecord {
            id: id.into(),
            metadata: Metadata {
                name: "pod".into(),
                uid: "uid-1".into(),
                namespace: "default".into(),
                attempt: 1,
            },
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            created_at_ns: 42,
            state: SandboxState::Ready,
            infra_container_id: "infra".into(),
            ip: "10.0.0.2".into(),
            extra_ips: Vec::new(),
            host_network: false,
            linux_json: String::new(),
            log_directory: String::new(),
        }
    }

    #[test]
    fn records_roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("rok-store-{}", new_id()));
        let store = Store::open(dir.join("sb"), dir.join("ct")).unwrap();
        store.put_sandbox(sandbox("abc")).unwrap();
        assert_eq!(store.sandbox("abc").unwrap().metadata.uid, "uid-1");
        let reopened = Store::open(dir.join("sb"), dir.join("ct")).unwrap();
        assert_eq!(reopened.sandboxes().len(), 1);
        reopened.remove_sandbox("abc").unwrap();
        assert!(reopened.sandbox("abc").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_64_hex_chars_and_unique() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    fn container(id: &str, sandbox_id: &str) -> ContainerRecord {
        ContainerRecord {
            id: id.into(),
            sandbox_id: sandbox_id.into(),
            metadata: Metadata {
                name: "app".into(),
                uid: String::new(),
                namespace: String::new(),
                attempt: 1,
            },
            image: "fake/app:latest".into(),
            image_ref: String::new(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            log_path: String::new(),
            tty: false,
            stdin: false,
            stdin_once: false,
            state: ContainerState::Created,
            created_at_ns: 1,
            started_at_ns: 0,
            finished_at_ns: 0,
            exit_code: -1,
            reason: String::new(),
            message: String::new(),
            spec_json: String::new(),
            config_bytes: Vec::new(),
            rootfs: String::new(),
            stop_signal: String::new(),
        }
    }

    #[test]
    fn containers_in_filters_by_sandbox() {
        let dir = std::env::temp_dir().join(format!("rok-store-{}", new_id()));
        let store = Store::open(dir.join("sb"), dir.join("ct")).unwrap();
        store.put_sandbox(sandbox("sb-a")).unwrap();
        store.put_sandbox(sandbox("sb-b")).unwrap();
        store.put_container(container("c1", "sb-a")).unwrap();
        store.put_container(container("c2", "sb-a")).unwrap();
        store.put_container(container("c3", "sb-b")).unwrap();
        let in_a = store.containers_in("sb-a");
        assert_eq!(in_a.len(), 2);
        assert!(in_a.iter().all(|c| c.sandbox_id == "sb-a"));
        store.remove_container("c1").unwrap();
        assert!(store.container("c1").is_none());
        assert_eq!(store.containers_in("sb-a").len(), 1);
        // Removing an absent record is fine.
        store.remove_container("missing").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_non_json_and_survives_reload_after_delete() {
        let dir = std::env::temp_dir().join(format!("rok-store-{}", new_id()));
        let sb = dir.join("sb");
        let ct = dir.join("ct");
        let store = Store::open(sb.clone(), ct.clone()).unwrap();
        store.put_sandbox(sandbox("keep")).unwrap();
        std::fs::write(sb.join("notes.txt"), "ignore me").unwrap();
        store.reload().unwrap();
        assert_eq!(store.sandboxes().len(), 1);
        store.remove_sandbox("keep").unwrap();
        store.reload().unwrap();
        assert!(store.sandboxes().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
