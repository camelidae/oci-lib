// SPDX-License-Identifier: Apache-2.0

//! In-process fake engine behind the `test-support` feature.
//!
//! It executes containers as ordinary child processes (no namespaces or
//! chroot) so the CRI service layer, the gRPC wire, logs, and the streaming
//! server can be tested anywhere. Never use it in production.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::engine::{
    Auth, Engine, EngineImage, EngineState, EngineStats, ExecOutput, FsInfo, InputSource,
    OutputSink,
};
use crate::error::{Error, ErrorCode, Result};

struct FakeContainer {
    args: Vec<String>,
    env: Vec<String>,
    cwd: String,
    rootfs: PathBuf,
    child: Option<Child>,
    pid: u32,
    stdin: Option<std::process::ChildStdin>,
    exit_code: i32,
    status: String,
}

/// A fake [`Engine`] that runs commands on the host.
pub struct FakeEngine {
    root: PathBuf,
    images: Mutex<HashMap<String, EngineImage>>,
    containers: Mutex<HashMap<String, Arc<Mutex<FakeContainer>>>>,
}

impl FakeEngine {
    /// Create a fake engine storing rootfses under `root`.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            images: Mutex::new(HashMap::new()),
            containers: Mutex::new(HashMap::new()),
        }
    }

    /// Install explicit image metadata for runtime/platform tests.
    pub fn insert_image(&self, image: EngineImage) {
        self.images
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(image.id.clone(), image);
    }

    fn container(&self, id: &str) -> Result<Arc<Mutex<FakeContainer>>> {
        self.containers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotFound, format!("container {id} not found"), ""))
    }

    fn cwd_for(rootfs: &Path, cwd: &str) -> PathBuf {
        let joined = rootfs.join(cwd.trim_start_matches('/'));
        if joined.is_dir() {
            joined
        } else {
            rootfs.to_path_buf()
        }
    }
}

fn pump(mut reader: impl Read + Send + 'static, stream: u8, sink: OutputSink) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink(stream, &buf[..n]),
            }
        }
    });
}

impl Engine for FakeEngine {
    fn pull_image(&self, image: &str, _auth: Option<&Auth>, _insecure: bool) -> Result<String> {
        let mut images = self.images.lock().unwrap_or_else(|p| p.into_inner());
        let entry = images
            .entry(image.to_string())
            .or_insert_with(|| EngineImage {
                id: crate::store::new_id(),
                repo_tags: vec![image.to_string()],
                repo_digests: vec![format!(
                    "{}@sha256:{}",
                    image.split('@').next().unwrap_or(image),
                    crate::store::new_id()
                )],
                size: 1024,
                username: String::new(),
                // A long-running placeholder process so sandbox infra containers
                // (whose image entrypoint would be /pause) stay up under the fake.
                spec: Some(crate::engine::ImageSpec {
                    entrypoint: vec!["/bin/sleep".into()],
                    cmd: vec!["100000".into()],
                    ..crate::engine::ImageSpec::default()
                }),
                ..EngineImage::default()
            });
        Ok(entry
            .repo_digests
            .first()
            .cloned()
            .unwrap_or_else(|| image.to_string()))
    }

    fn list_images(&self, filter: &str) -> Result<Vec<EngineImage>> {
        Ok(self
            .images
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|img| {
                filter.is_empty() || img.id == filter || img.repo_tags.iter().any(|t| t == filter)
            })
            .cloned()
            .collect())
    }

    fn remove_image(&self, image: &str) -> Result<()> {
        let mut images = self.images.lock().unwrap_or_else(|p| p.into_inner());
        let id = images
            .iter()
            .find(|(_, img)| img.id == image || img.repo_tags.iter().any(|t| t == image))
            .map(|(key, _)| key.clone());
        if let Some(id) = id {
            images.remove(&id);
        }
        Ok(())
    }

    fn image_fs_info(&self) -> Result<Vec<FsInfo>> {
        Ok(vec![FsInfo {
            timestamp_ns: crate::store::now_ns(),
            mountpoint: self.root.to_string_lossy().into_owned(),
            used_bytes: 0,
            inodes_used: 0,
        }])
    }

    fn prepare_rootfs(&self, id: &str, _image: &str) -> Result<PathBuf> {
        let rootfs = self.root.join("rootfs").join(id);
        std::fs::create_dir_all(&rootfs)
            .map_err(|err| Error::new(ErrorCode::Internal, err.to_string(), ""))?;
        Ok(rootfs)
    }

    fn release_rootfs(&self, id: &str) -> Result<()> {
        let rootfs = self.root.join("rootfs").join(id);
        let _ = std::fs::remove_dir_all(rootfs);
        Ok(())
    }

    fn create_container(&self, id: &str, spec_json: &str) -> Result<()> {
        let spec: serde_json::Value = serde_json::from_str(spec_json)
            .map_err(|err| Error::new(ErrorCode::InvalidArgument, err.to_string(), ""))?;
        let args: Vec<String> = spec["process"]["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let env: Vec<String> = spec["process"]["env"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let container = FakeContainer {
            args,
            env,
            cwd: spec["process"]["cwd"].as_str().unwrap_or("/").to_string(),
            rootfs: PathBuf::from(spec["root"]["path"].as_str().unwrap_or("")),
            child: None,
            pid: 0,
            stdin: None,
            exit_code: -1,
            status: "created".to_string(),
        };
        self.containers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), Arc::new(Mutex::new(container)));
        Ok(())
    }

    fn start_container(&self, id: &str, on_output: OutputSink) -> Result<()> {
        let container = self.container(id)?;
        let mut guard = container.lock().unwrap_or_else(|p| p.into_inner());
        if guard.args.is_empty() {
            return Err(Error::new(ErrorCode::InvalidArgument, "no command", ""));
        }
        let mut command = Command::new(&guard.args[0]);
        command
            .args(&guard.args[1..])
            .current_dir(Self::cwd_for(&guard.rootfs, &guard.cwd))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for entry in &guard.env {
            if let Some((key, value)) = entry.split_once('=') {
                command.env(key, value);
            }
        }
        let mut child = command.spawn().map_err(|err| {
            Error::new(
                ErrorCode::Run,
                format!("spawning container process: {err}"),
                "",
            )
        })?;
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, 1, Arc::clone(&on_output));
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, 2, on_output);
        }
        guard.pid = child.id();
        guard.stdin = child.stdin.take();
        guard.child = Some(child);
        guard.status = "running".to_string();
        Ok(())
    }

    fn wait_container(&self, id: &str) -> Result<i32> {
        let container = self.container(id)?;
        let mut child = {
            let mut guard = container.lock().unwrap_or_else(|p| p.into_inner());
            guard.child.take()
        };
        let code = match child.as_mut() {
            Some(child) => child
                .wait()
                .map(|status| status.code().unwrap_or(-1))
                .unwrap_or(-1),
            None => {
                container
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .exit_code
            }
        };
        let mut guard = container.lock().unwrap_or_else(|p| p.into_inner());
        guard.exit_code = code;
        guard.status = "stopped".to_string();
        Ok(code)
    }

    fn signal_container(&self, id: &str, signal: i32) -> Result<()> {
        let container = self.container(id)?;
        let guard = container.lock().unwrap_or_else(|p| p.into_inner());
        #[cfg(unix)]
        {
            if guard.pid != 0 {
                unsafe { libc::kill(guard.pid as i32, signal) };
            }
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            if let Some(child) = guard.child.as_mut() {
                let _ = child.kill();
            }
        }
        Ok(())
    }

    fn remove_container(&self, id: &str) -> Result<()> {
        self.containers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        Ok(())
    }

    fn container_state(&self, id: &str) -> Result<EngineState> {
        let container = self.container(id)?;
        let mut guard = container.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(child) = guard.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                guard.exit_code = status.code().unwrap_or(-1);
                guard.status = "stopped".to_string();
            }
        }
        Ok(EngineState {
            status: guard.status.clone(),
            pid: guard.pid as i32,
            exit_code: guard.exit_code,
            oom_killed: false,
            namespaces: Default::default(),
        })
    }

    fn exec_sync(&self, id: &str, cmd: &[String], _timeout_secs: i64) -> Result<ExecOutput> {
        let container = self.container(id)?;
        let (rootfs, cwd, env) = {
            let guard = container.lock().unwrap_or_else(|p| p.into_inner());
            (guard.rootfs.clone(), guard.cwd.clone(), guard.env.clone())
        };
        if cmd.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "exec command is empty",
                "",
            ));
        }
        let mut command = Command::new(&cmd[0]);
        command
            .args(&cmd[1..])
            .current_dir(Self::cwd_for(&rootfs, &cwd));
        for entry in &env {
            if let Some((key, value)) = entry.split_once('=') {
                command.env(key, value);
            }
        }
        let output = command
            .output()
            .map_err(|err| Error::new(ErrorCode::Run, err.to_string(), ""))?;
        Ok(ExecOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn exec_stream(
        &self,
        id: &str,
        cmd: &[String],
        _tty: bool,
        next_input: InputSource,
        on_output: OutputSink,
    ) -> Result<i32> {
        let container = self.container(id)?;
        let (rootfs, cwd, env) = {
            let guard = container.lock().unwrap_or_else(|p| p.into_inner());
            (guard.rootfs.clone(), guard.cwd.clone(), guard.env.clone())
        };
        if cmd.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "exec command is empty",
                "",
            ));
        }
        let mut command = Command::new(&cmd[0]);
        command
            .args(&cmd[1..])
            .current_dir(Self::cwd_for(&rootfs, &cwd))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for entry in &env {
            if let Some((key, value)) = entry.split_once('=') {
                command.env(key, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|err| Error::new(ErrorCode::Run, err.to_string(), ""))?;
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, 1, Arc::clone(&on_output));
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, 2, on_output);
        }
        if let Some(mut stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                loop {
                    let data = next_input();
                    if data.is_empty() {
                        break;
                    }
                    if stdin.write_all(&data).is_err() {
                        break;
                    }
                    let _ = stdin.flush();
                }
            });
        }
        child
            .wait()
            .map(|status| status.code().unwrap_or(-1))
            .map_err(|err| Error::new(ErrorCode::Run, err.to_string(), ""))
    }

    fn write_stdin(&self, id: &str, data: &[u8]) -> Result<()> {
        let container = self.container(id)?;
        let mut guard = container.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(stdin) = guard.stdin.as_mut() {
            if data.is_empty() {
                let _ = stdin.flush();
            } else {
                stdin
                    .write_all(data)
                    .map_err(|err| Error::new(ErrorCode::Run, err.to_string(), ""))?;
            }
        }
        Ok(())
    }

    fn resize_tty(&self, _id: &str, _width: u16, _height: u16) -> Result<()> {
        Ok(())
    }

    fn container_stats(&self, id: &str) -> Result<EngineStats> {
        let _ = self.container(id)?;
        Ok(EngineStats {
            mem_limit: u64::MAX,
            ..EngineStats::default()
        })
    }

    fn update_resources(&self, id: &str, _resources_json: &str) -> Result<()> {
        let _ = self.container(id)?;
        Ok(())
    }

    fn checkpoint(&self, _id: &str, _image: &str, _parent: &str) -> Result<()> {
        Err(Error::new(
            ErrorCode::Unsupported,
            "checkpoint is not available in the fake engine",
            "",
        ))
    }

    fn diagnose(&self) -> Result<String> {
        Ok("status: ready\n[ok] fake engine (test-support)\n".to_string())
    }
}
