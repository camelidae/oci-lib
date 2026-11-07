//go:build linux

// SPDX-License-Identifier: Apache-2.0

package main

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sync"
	"syscall"
	"time"

	"github.com/opencontainers/cgroups/fs"
	"github.com/opencontainers/cgroups/fs2"
	"github.com/opencontainers/runc/libcontainer"
	"github.com/opencontainers/runc/libcontainer/specconv"
	specs "github.com/opencontainers/runtime-spec/specs-go"
)

// Container lifecycle on libcontainer. Shapes mirror runc's create/start:
// libcontainer.Create lays down state, Start runs the init process, Wait
// reaps it. Handles live in a registry because start and wait are separate
// rok_calls; the state root also makes containers reloadable after a daemon
// restart via libcontainer.Load.

type handle struct {
	namespaces map[string]*os.File
	startMu    sync.Mutex
	startErr   error
	container  *libcontainer.Container
	process    *libcontainer.Process
	stdin      io.WriteCloser
	pumps      sync.WaitGroup
	// Result fields are published by closing waitDone. Only one goroutine
	// calls Process.Wait, including for sandbox init processes.
	waitDone  chan struct{}
	waitErr   error
	oomKilled bool
	exitCode  int
}

var (
	handlesMu sync.Mutex
	handles   = map[string]*handle{}
)

func stateRoot(root string) string {
	if root == "" {
		return "/run/oci-cri/state"
	}
	// Callers pass either the store root (state lives in <root>/state) or,
	// when the request carries an explicit state_root, that path via
	// stateRootOf. This helper resolves the store-root form.
	return filepath.Join(root, "state")
}

func stateRootOf(req *opRequest) string {
	if req.StateRoot != "" {
		return req.StateRoot
	}
	return stateRoot(req.StoreRoot)
}

func specDir(root string) string {
	return filepath.Join(root, "specs")
}

func loadContainer(req *opRequest, id string) (*libcontainer.Container, error) {
	container, err := libcontainer.Load(stateRootOf(req), id)
	if err != nil {
		return nil, fail(codeNotFound, "container not found", err.Error())
	}
	return container, nil
}

func savedSpec(root, id string) (*specs.Spec, error) {
	data, err := os.ReadFile(filepath.Join(specDir(root), id+".json"))
	if err != nil {
		return nil, fail(codeInternal, "reading saved spec", err.Error())
	}
	var spec specs.Spec
	if err := json.Unmarshal(data, &spec); err != nil {
		return nil, fail(codeInternal, "parsing saved spec", err.Error())
	}
	return &spec, nil
}

func getHandle(req *opRequest, id string) (*handle, error) {
	handlesMu.Lock()
	defer handlesMu.Unlock()
	if h, ok := handles[id]; ok {
		return h, nil
	}
	container, err := loadContainer(req, id)
	if err != nil {
		return nil, err
	}
	h := &handle{container: container, exitCode: -1}
	handles[id] = h
	return h, nil
}

func opCreate(req *opRequest) error {
	if req.ID == "" {
		return fail(codeInvalid, "container id is empty", "")
	}
	var spec specs.Spec
	if err := json.Unmarshal(req.Spec, &spec); err != nil {
		return fail(codeInvalid, "spec is not an OCI spec", err.Error())
	}
	if spec.Root == nil || spec.Root.Path == "" {
		return fail(codeInvalid, "spec has no root path", "")
	}
	if err := os.MkdirAll(stateRootOf(req), 0o700); err != nil {
		return fail(codeInternal, "state directory", err.Error())
	}
	if err := os.MkdirAll(specDir(req.StoreRoot), 0o700); err != nil {
		return fail(codeInternal, "spec directory", err.Error())
	}
	if err := applySecurity(&spec); err != nil {
		return err
	}
	rootless := os.Geteuid() != 0
	if rootless {
		specconv.ToRootless(&spec)
	}
	if !rootless {
		if err := prepareCgroupRoot(); err != nil {
			return fail(codeRun, "prepare delegated cgroup", err.Error())
		}
	}
	opts := &specconv.CreateOpts{
		CgroupName:      req.ID,
		Spec:            &spec,
		RootlessEUID:    rootless,
		RootlessCgroups: rootless,
	}
	config, err := specconv.CreateLibcontainerConfig(opts)
	if err != nil {
		return fail(codeRun, "container config", err.Error())
	}
	var st syscall.Statfs_t
	// An initramfs rootfs can be backed by either ramfs or tmpfs. Linux
	// cannot pivot_root away from rootfs; use libcontainer's move/chroot path.
	if syscall.Statfs("/", &st) == nil && (st.Type == 0x858458f6 || st.Type == 0x01021994) {
		config.NoPivotRoot = true
	}
	container, err := libcontainer.Create(stateRootOf(req), req.ID, config)
	if err != nil {
		return fail(codeRun, "create container", err.Error())
	}
	specJSON, err := json.Marshal(&spec)
	if err != nil {
		return fail(codeInternal, "encoding spec", err.Error())
	}
	if err := os.WriteFile(filepath.Join(specDir(req.StoreRoot), req.ID+".json"), specJSON, 0o600); err != nil {
		_ = container.Destroy()
		return fail(codeInternal, "saving spec", err.Error())
	}
	handlesMu.Lock()
	handles[req.ID] = &handle{container: container, exitCode: -1}
	handlesMu.Unlock()
	return nil
}

func opStart(req *opRequest, hooks *ioHooks) (retErr error) {
	h, err := getHandle(req, req.ID)
	if err != nil {
		return err
	}
	h.startMu.Lock()
	defer h.startMu.Unlock()
	if h.startErr != nil {
		return h.startErr
	}
	// Never tear down a previous successful start on a duplicate request.
	if h.process != nil {
		return fail(codeRun, "container has already been started", "")
	}
	status, err := h.container.Status()
	if err != nil {
		return fail(codeRun, "container status before start", err.Error())
	}
	if status != libcontainer.Stopped {
		return fail(codeRun, "container is not stopped", status.String())
	}
	spec, err := savedSpec(req.StoreRoot, req.ID)
	if err != nil {
		return err
	}
	if spec.Process == nil {
		return fail(codeInvalid, "spec has no process", "")
	}
	initStarted := false
	startAttempted := false
	defer func() {
		if retErr == nil || !startAttempted {
			return
		}
		// Runs after output writers have closed. Failed Start may leave
		// initProcessStartTime set even after libcontainer reaps its child.
		// Destroy and recreate the handle from its configuration before retry.
		if cleanupErr := resetFailedStart(req, h, initStarted); cleanupErr != nil {
			retErr = errors.Join(retErr, fmt.Errorf("reset failed start: %w", cleanupErr))
			h.startErr = retErr
		}
	}()
	process := &libcontainer.Process{
		Args:          spec.Process.Args,
		Env:           spec.Process.Env,
		Cwd:           spec.Process.Cwd,
		Init:          true,
		ConsoleWidth:  0,
		ConsoleHeight: 0,
		// specconv leaves AppArmor to the caller's per-process configuration.
		AppArmorProfile: spec.Process.ApparmorProfile,
	}
	process.UID = int(spec.Process.User.UID)
	process.GID = int(spec.Process.User.GID)
	for _, gid := range spec.Process.User.AdditionalGids {
		process.AdditionalGroups = append(process.AdditionalGroups, int(gid))
	}
	// stdin is a pipe so Attach can write to it later.
	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		return fail(codeInternal, "stdin pipe", err.Error())
	}
	defer stdinRead.Close()
	started := false
	defer func() {
		if !started {
			closeNamespaces(h.namespaces)
			h.namespaces = nil
			_ = stdinWrite.Close()
			h.stdin = nil
		}
	}()
	process.Stdin = stdinRead
	h.stdin = stdinWrite
	if hooks != nil && hooks.emit != nil {
		emit := hooks.emit
		if err := pipeTo(&h.pumps, &process.Stdout, emit, 1); err != nil {
			return err
		}
		// Start duplicates these descriptors into the child. Keeping the
		// parent writers open prevents EOF and deadlocks opWait on pumps.
		defer process.Stdout.(*os.File).Close()
		if spec.Process.Terminal {
			process.Stderr = process.Stdout
		} else {
			if err := pipeTo(&h.pumps, &process.Stderr, emit, 2); err != nil {
				return err
			}
			defer process.Stderr.(*os.File).Close()
		}
	} else {
		process.Stdout = os.Stdout
		process.Stderr = os.Stderr
	}
	startAttempted = true
	if err := h.container.Start(process); err != nil {
		_ = stdinRead.Close()
		return fail(codeRun, "start container", err.Error())
	}
	initStarted = true
	h.process = process
	pid, err := process.Pid()
	if err != nil {
		return fail(codeRun, "container pid before exec", err.Error())
	}
	h.namespaces, err = pinNamespaces(pid)
	if err != nil {
		return fail(codeRun, "pin container namespaces", err.Error())
	}
	if err := h.container.Exec(); err != nil {
		return fail(codeRun, "exec container init", err.Error())
	}
	_ = stdinRead.Close()
	h.process = process
	h.waitDone = make(chan struct{})
	go func() {
		state, err := process.Wait()
		if err != nil && state == nil {
			h.waitErr = fail(codeRun, "wait", err.Error())
		} else {
			h.exitCode = exitCode(state)
		}
		h.oomKilled = containerOOMKilled(h.container)
		close(h.waitDone)
	}()
	started = true
	return nil
}

// Keep the saved OCI spec and rootfs: CRI still owns a Created container.
func resetFailedStart(req *opRequest, h *handle, initStarted bool) error {
	config := h.container.Config()
	if initStarted {
		if err := h.container.Signal(syscall.SIGKILL); err != nil {
			status, statusErr := h.container.Status()
			if statusErr != nil || status != libcontainer.Stopped {
				return err
			}
		}
		_, _ = h.process.Wait()
	}
	if err := h.container.Destroy(); err != nil {
		return err
	}
	h.pumps.Wait()
	h.process = nil
	container, err := libcontainer.Create(stateRootOf(req), req.ID, &config)
	if err != nil {
		return err
	}
	h.container = container
	return nil
}

func opWait(req *opRequest) (any, error) {
	h, err := getHandle(req, req.ID)
	if err != nil {
		return nil, err
	}
	h.startMu.Lock()
	done := h.waitDone
	h.startMu.Unlock()
	if done == nil {
		// A reloaded container is not our child; its exit code is unknown.
		return map[string]any{"exit_code": -1}, nil
	}
	<-done
	h.pumps.Wait()
	if h.waitErr != nil {
		return nil, h.waitErr
	}
	return map[string]any{"exit_code": h.exitCode, "oom_killed": h.oomKilled}, nil
}

func opKill(req *opRequest) error {
	h, err := getHandle(req, req.ID)
	if err != nil {
		return err
	}
	signal := syscall.Signal(req.Signal)
	if signal == 0 {
		signal = syscall.SIGTERM
	}
	h.startMu.Lock()
	defer h.startMu.Unlock()
	if h.waitDone != nil {
		select {
		case <-h.waitDone:
			return h.waitErr
		default:
			if err := h.process.Signal(signal); err != nil && !errors.Is(err, os.ErrProcessDone) {
				return fail(codeRun, "signal container", err.Error())
			}
			return nil
		}
	}
	if err := h.container.Signal(signal); err != nil {
		// Killing an already-stopped container is not an error for CRI.
		if status, serr := h.container.Status(); serr == nil && status == libcontainer.Stopped {
			return nil
		}
		return fail(codeRun, "signal container", err.Error())
	}
	return nil
}

func opDelete(req *opRequest) error {
	h, err := getHandle(req, req.ID)
	if err != nil {
		return nil // already gone
	}
	h.startMu.Lock()
	defer h.startMu.Unlock()
	if h.waitDone != nil {
		select {
		case <-h.waitDone:
		default:
			if err := h.process.Signal(syscall.SIGKILL); err != nil && !errors.Is(err, os.ErrProcessDone) {
				return fail(codeRun, "kill before delete", err.Error())
			}
			<-h.waitDone
		}
	} else if status, serr := h.container.Status(); serr == nil && status != libcontainer.Stopped {
		_ = h.container.Signal(syscall.SIGKILL)
	}
	if h.stdin != nil {
		_ = h.stdin.Close()
	}
	if err := h.container.Destroy(); err != nil {
		return fail(codeRun, "destroy container", err.Error())
	}
	closeNamespaces(h.namespaces)
	h.namespaces = nil
	handlesMu.Lock()
	delete(handles, req.ID)
	handlesMu.Unlock()
	_ = os.Remove(filepath.Join(specDir(req.StoreRoot), req.ID+".json"))
	return nil
}

func containerOOMKilled(container *libcontainer.Container) bool {
	state, err := container.State()
	if err != nil {
		return false
	}
	var count uint64
	if path := state.CgroupPaths[""]; path != "" {
		count, err = fs2.OOMKillCount(path)
	} else if path := state.CgroupPaths["memory"]; path != "" {
		count, err = fs.OOMKillCount(path)
	}
	return err == nil && count > 0
}

func opState(req *opRequest) (any, error) {
	handlesMu.Lock()
	h := handles[req.ID]
	handlesMu.Unlock()
	if h != nil {
		h.startMu.Lock()
		if h.waitDone != nil {
			defer h.startMu.Unlock()
			// During exec's de_thread, /proc can briefly report the init
			// leader as a zombie while another thread replaces it. Wait is
			// authoritative for a child we own; a procfs snapshot is not.
			select {
			case <-h.waitDone:
				if h.waitErr != nil {
					return nil, h.waitErr
				}
				return map[string]any{"status": "stopped", "pid": 0, "exit_code": h.exitCode, "oom_killed": h.oomKilled}, nil
			default:
				pid, err := h.process.Pid()
				if err != nil {
					return nil, fail(codeInternal, "container pid", err.Error())
				}
				return map[string]any{"status": "running", "pid": pid, "exit_code": -1, "namespaces": namespacePaths(h.namespaces)}, nil
			}
		}
		h.startMu.Unlock()
	}
	container, err := loadContainer(req, req.ID)
	if err != nil {
		// Fall back to the in-memory handle (e.g. state root cleaned up).
		handlesMu.Lock()
		h, ok := handles[req.ID]
		handlesMu.Unlock()
		if !ok {
			return nil, err
		}
		container = h.container
	}
	status, err := container.Status()
	if err != nil {
		return nil, fail(codeInternal, "container status", err.Error())
	}
	pid := 0
	exit := -1
	if state, err := container.State(); err == nil && state.InitProcessPid > 0 {
		pid = state.InitProcessPid
	}
	name := "stopped"
	switch status {
	case libcontainer.Running:
		name = "running"
	case libcontainer.Created:
		name = "created"
	case libcontainer.Paused:
		name = "paused"
	}
	return map[string]any{"status": name, "pid": pid, "exit_code": exit}, nil
}

func opStats(req *opRequest) (any, error) {
	container, err := loadContainer(req, req.ID)
	if err != nil {
		return nil, err
	}
	stats, err := container.Stats()
	if err != nil {
		return nil, fail(codeRun, "container stats", err.Error())
	}
	out := engineStats(stats)
	if spec, err := savedSpec(req.StoreRoot, req.ID); err == nil && spec.Root != nil {
		size, inodes := dirUsage(spec.Root.Path)
		out["writable_size"] = size
		out["inodes_used"] = inodes
	}
	return out, nil
}

// engineStats is the complete JSON contract consumed by Rust EngineStats.
func engineStats(stats *libcontainer.Stats) map[string]any {
	out := map[string]any{
		"cpu_total_ns":  uint64(0),
		"cpu_system_ns": uint64(0),
		"mem_usage":     uint64(0),
		"mem_limit":     uint64(1<<63 - 1),
		"mem_cache":     uint64(0),
		"mem_swap":      uint64(0),
		"writable_size": uint64(0),
		"inodes_used":   uint64(0),
	}
	if stats.CgroupStats != nil {
		out["cpu_total_ns"] = stats.CgroupStats.CpuStats.CpuUsage.TotalUsage
		out["cpu_system_ns"] = stats.CgroupStats.CpuStats.CpuUsage.UsageInKernelmode
		out["cpu_percpu_ns"] = stats.CgroupStats.CpuStats.CpuUsage.PercpuUsage
		out["mem_usage"] = stats.CgroupStats.MemoryStats.Usage.Usage
		if stats.CgroupStats.MemoryStats.Usage.Limit != 0 {
			out["mem_limit"] = stats.CgroupStats.MemoryStats.Usage.Limit
		}
		out["mem_cache"] = stats.CgroupStats.MemoryStats.Cache
		out["mem_swap"] = stats.CgroupStats.MemoryStats.SwapUsage.Usage
	}
	var rx, tx, rxErrors, txErrors uint64
	for _, iface := range stats.Interfaces {
		rx += iface.RxBytes
		tx += iface.TxBytes
		rxErrors += iface.RxErrors
		txErrors += iface.TxErrors
	}
	out["rx_bytes"] = rx
	out["tx_bytes"] = tx
	out["rx_errors"] = rxErrors
	out["tx_errors"] = txErrors
	return out
}

func dirUsage(root string) (uint64, uint64) {
	var size, inodes uint64
	_ = filepath.Walk(root, func(_ string, info os.FileInfo, err error) error {
		if err != nil {
			return nil
		}
		inodes++
		if info.Mode().IsRegular() {
			size += uint64(info.Size())
		}
		return nil
	})
	return size, inodes
}

func opUpdate(req *opRequest) error {
	container, err := loadContainer(req, req.ID)
	if err != nil {
		return err
	}
	var resources specs.LinuxResources
	if len(req.Resources) > 0 {
		if err := json.Unmarshal(req.Resources, &resources); err != nil {
			return fail(codeInvalid, "resources", err.Error())
		}
	}
	config := container.Config()
	if config.Cgroups != nil && config.Cgroups.Resources != nil {
		target := config.Cgroups.Resources
		if resources.CPU != nil {
			if resources.CPU.Shares != nil {
				target.CpuShares = *resources.CPU.Shares
			}
			if resources.CPU.Quota != nil {
				target.CpuQuota = *resources.CPU.Quota
			}
			if resources.CPU.Period != nil {
				target.CpuPeriod = *resources.CPU.Period
			}
			if resources.CPU.Cpus != "" {
				target.CpusetCpus = resources.CPU.Cpus
			}
			if resources.CPU.Mems != "" {
				target.CpusetMems = resources.CPU.Mems
			}
		}
		if resources.Memory != nil && resources.Memory.Limit != nil {
			target.Memory = *resources.Memory.Limit
		}
	}
	if err := container.Set(config); err != nil {
		return fail(codeRun, "update resources", err.Error())
	}
	return nil
}

type callbackReader struct {
	read func(buf []byte) (int, error)
	buf  []byte
}

func (r *callbackReader) Read(p []byte) (int, error) {
	if len(r.buf) == 0 {
		chunk := make([]byte, 4096)
		n, err := r.read(chunk)
		if err != nil {
			return 0, io.EOF
		}
		if n == 0 {
			return 0, io.EOF
		}
		r.buf = chunk[:n]
	}
	n := copy(p, r.buf)
	r.buf = r.buf[n:]
	return n, nil
}

type callbackWriter struct {
	emit   func(stream int32, data []byte)
	stream int32
}

func (w *callbackWriter) Write(p []byte) (int, error) {
	w.emit(w.stream, p)
	return len(p), nil
}

func opExec(req *opRequest, hooks *ioHooks) (any, error) {
	h, err := getHandle(req, req.ID)
	if err != nil {
		return nil, err
	}
	if len(req.Cmd) == 0 {
		return nil, fail(codeInvalid, "exec command is empty", "")
	}
	spec, err := savedSpec(req.StoreRoot, req.ID)
	if err != nil {
		return nil, err
	}
	process := &libcontainer.Process{
		Args: req.Cmd,
		Init: false,
	}
	if spec != nil && spec.Process != nil {
		process.Env = spec.Process.Env
		process.Cwd = spec.Process.Cwd
		process.AppArmorProfile = spec.Process.ApparmorProfile
		process.UID = int(spec.Process.User.UID)
		process.GID = int(spec.Process.User.GID)
		for _, gid := range spec.Process.User.AdditionalGids {
			process.AdditionalGroups = append(process.AdditionalGroups, int(gid))
		}
	}
	if hooks != nil && hooks.readInput != nil {
		if hooks.emit == nil {
			return nil, fail(codeInvalid, "interactive stdin requires a cancellation callback", "")
		}
		reader, writer, err := os.Pipe()
		if err != nil {
			return nil, fail(codeInternal, "exec stdin", err.Error())
		}
		defer reader.Close()
		process.Stdin = reader
		inputDone := make(chan struct{})
		go func() {
			defer close(inputDone)
			defer writer.Close()
			_, _ = io.Copy(writer, &callbackReader{read: hooks.readInput})
		}()
		defer func() {
			// Private control message: cancel the Rust-side blocking source
			// before joining this callback and releasing its FFI storage.
			if hooks.emit != nil {
				hooks.emit(0, []byte{0})
			}
			_ = writer.Close()
			<-inputDone
		}()
	}
	if hooks != nil && hooks.emit != nil {
		process.Stdout = &callbackWriter{emit: hooks.emit, stream: 1}
		if req.TTY {
			process.Stderr = process.Stdout
		} else {
			process.Stderr = &callbackWriter{emit: hooks.emit, stream: 2}
		}
	}
	if err := h.container.Run(process); err != nil {
		return nil, fail(codeRun, "exec", err.Error())
	}
	type waitResult struct {
		state *os.ProcessState
		err   error
	}
	waited := make(chan waitResult, 1)
	go func() { state, err := process.Wait(); waited <- waitResult{state, err} }()
	var result waitResult
	if req.Timeout > 0 {
		timer := time.NewTimer(time.Duration(req.Timeout) * time.Second)
		defer timer.Stop()
		select {
		case result = <-waited:
		case <-timer.C:
			_ = process.Signal(syscall.SIGKILL)
			<-waited
			return nil, fail(codeTimeout, "exec timed out", "command was killed and reaped")
		}
	} else {
		result = <-waited
	}
	state, err := result.state, result.err
	if err != nil && state == nil {
		return nil, fail(codeRun, "exec wait", err.Error())
	}
	return map[string]any{"exit_code": exitCode(state)}, nil
}

func opWriteStdin(req *opRequest) error {
	h, err := getHandle(req, req.ID)
	if err != nil {
		return err
	}
	if h.stdin == nil {
		return fail(codeState, "container stdin is not open", "")
	}
	if req.Data == "" {
		return nil
	}
	data, err := base64.StdEncoding.DecodeString(req.Data)
	if err != nil {
		return fail(codeInvalid, "stdin data", err.Error())
	}
	if _, err := h.stdin.Write(data); err != nil {
		return fail(codeRun, "write stdin", err.Error())
	}
	return nil
}

func opResize(req *opRequest) error {
	// Piped (non-console) stdio has no winsize to set; terminal resize is a
	// no-op at this layer and reported as success so kubelet does not retry.
	_, err := getHandle(req, req.ID)
	return err
}

func opCheckpoint(req *opRequest) error {
	h, err := getHandle(req, req.ID)
	if err != nil {
		return err
	}
	if req.Image == "" {
		return fail(codeInvalid, "checkpoint location is empty", "")
	}
	if err := os.MkdirAll(req.Image, 0o755); err != nil {
		return fail(codeInternal, "checkpoint directory", err.Error())
	}
	opts := &libcontainer.CriuOpts{
		ImagesDirectory:   req.Image,
		WorkDirectory:     filepath.Join(req.Image, "work"),
		LeaveRunning:      false,
		TcpEstablished:    true,
		ShellJob:          false,
		FileLocks:         true,
		ManageCgroupsMode: "full",
	}
	if err := h.container.Checkpoint(opts); err != nil {
		return fail(codeRun, "checkpoint (needs CRIU)", err.Error())
	}
	return nil
}

func pipeTo(pumps *sync.WaitGroup, slot *io.Writer, emit func(stream int32, data []byte), stream int32) error {
	read, write, err := os.Pipe()
	if err != nil {
		return fail(codeInternal, "stdio pipe", err.Error())
	}
	*slot = write
	pumps.Add(1)
	go func() {
		defer pumps.Done()
		defer read.Close()
		buf := make([]byte, 32*1024)
		for {
			n, err := read.Read(buf)
			if n > 0 {
				emit(stream, buf[:n])
			}
			if err != nil {
				return
			}
		}
	}()
	return nil
}

func exitCode(state *os.ProcessState) int {
	if state == nil {
		return 1
	}
	code := state.ExitCode()
	if code >= 0 {
		return code
	}
	if ws, ok := state.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
		return 128 + int(ws.Signal())
	}
	return 1
}

// Pin namespaces while init is blocked on its exec FIFO. During execve,
// de_thread can temporarily hide /proc/<init-pid>/ns even though the process
// is alive. Runtime-owned descriptors keep CNI and subsequent joins stable.
func pinNamespaces(pid int) (map[string]*os.File, error) {
	result := map[string]*os.File{}
	for _, name := range []string{"net", "ipc", "uts", "pid"} {
		file, err := os.Open(fmt.Sprintf("/proc/%d/ns/%s", pid, name))
		if err != nil {
			closeNamespaces(result)
			return nil, err
		}
		result[name] = file
	}
	return result, nil
}

func namespacePaths(files map[string]*os.File) map[string]string {
	result := map[string]string{}
	for name, file := range files {
		result[name] = fmt.Sprintf("/proc/%d/fd/%d", os.Getpid(), file.Fd())
	}
	return result
}

func closeNamespaces(files map[string]*os.File) {
	for _, file := range files {
		_ = file.Close()
	}
}
