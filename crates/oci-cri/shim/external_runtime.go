//go:build linux

// SPDX-License-Identifier: Apache-2.0

package main

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	specs "github.com/opencontainers/runtime-spec/specs-go"
)

// External OCI runtime (for example runsc) selected with ROK_OCI_RUNTIME.
// Create only materializes a bundle; start runs `runtime run` as a child so
// Wait/Kill stay process-local. Libcontainer remains the default when the
// env var is unset or empty.

type extHandle struct {
	bundleDir string
	cmd       *exec.Cmd
	stdin     io.WriteCloser
	pumps     sync.WaitGroup
	startMu   sync.Mutex
	startErr  error
	waitDone  chan struct{}
	waitErr   error
	exitCode  int
	oomKilled bool
}

var (
	extHandlesMu sync.Mutex
	extHandles   = map[string]*extHandle{}
)

func useExternalRuntime() bool {
	return strings.TrimSpace(os.Getenv("ROK_OCI_RUNTIME")) != ""
}

func externalRuntimeBinary() (string, error) {
	bin := strings.TrimSpace(os.Getenv("ROK_OCI_RUNTIME"))
	if bin == "" {
		return "", fail(codePrereq, "ROK_OCI_RUNTIME is not set", "")
	}
	if path, err := exec.LookPath(bin); err == nil {
		return path, nil
	}
	if _, err := os.Stat(bin); err == nil {
		return bin, nil
	}
	return "", fail(codePrereq, "ROK_OCI_RUNTIME not found", bin)
}

func bundleDir(storeRoot, id string) string {
	return filepath.Join(storeRoot, "bundles", id)
}

func runtimeRoot(stateRoot string) string {
	return filepath.Join(stateRoot, "oci-runtime")
}

func runtimeFlags(stateRoot string) []string {
	flags := []string{
		"--root", runtimeRoot(stateRoot),
	}
	// Defaults match the diorama-vm guest runsc options that already boot
	// under Apple Virtualization (host net, ignore cgroups, memory overlay).
	extra := strings.TrimSpace(os.Getenv("ROK_OCI_RUNTIME_FLAGS"))
	if extra == "" {
		extra = "--network=host --ignore-cgroups --overlay2=root:memory"
	}
	for _, field := range strings.Fields(extra) {
		flags = append(flags, field)
	}
	return flags
}

func getExtHandle(id string) (*extHandle, error) {
	extHandlesMu.Lock()
	defer extHandlesMu.Unlock()
	h, ok := extHandles[id]
	if !ok {
		return nil, fail(codeNotFound, "container not found", id)
	}
	return h, nil
}

func opCreateExternal(req *opRequest) error {
	if req.ID == "" {
		return fail(codeInvalid, "container id is empty", "")
	}
	if _, err := externalRuntimeBinary(); err != nil {
		return err
	}
	var spec specs.Spec
	if err := json.Unmarshal(req.Spec, &spec); err != nil {
		return fail(codeInvalid, "spec is not an OCI spec", err.Error())
	}
	if spec.Root == nil || spec.Root.Path == "" {
		return fail(codeInvalid, "spec has no root path", "")
	}
	if err := applySecurity(&spec); err != nil {
		return err
	}
	if err := os.MkdirAll(specDir(req.StoreRoot), 0o700); err != nil {
		return fail(codeInternal, "spec directory", err.Error())
	}
	if err := os.MkdirAll(runtimeRoot(stateRootOf(req)), 0o700); err != nil {
		return fail(codeInternal, "runtime root", err.Error())
	}
	dir := bundleDir(req.StoreRoot, req.ID)
	if err := os.RemoveAll(dir); err != nil {
		return fail(codeInternal, "clearing bundle", err.Error())
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fail(codeInternal, "bundle directory", err.Error())
	}
	// runsc expects root.path relative to the bundle or absolute. Keep absolute.
	if !filepath.IsAbs(spec.Root.Path) {
		spec.Root.Path = filepath.Join(dir, spec.Root.Path)
	}
	specJSON, err := json.Marshal(&spec)
	if err != nil {
		return fail(codeInternal, "encoding spec", err.Error())
	}
	if err := os.WriteFile(filepath.Join(dir, "config.json"), specJSON, 0o600); err != nil {
		return fail(codeInternal, "writing bundle config", err.Error())
	}
	if err := os.WriteFile(filepath.Join(specDir(req.StoreRoot), req.ID+".json"), specJSON, 0o600); err != nil {
		_ = os.RemoveAll(dir)
		return fail(codeInternal, "saving spec", err.Error())
	}
	extHandlesMu.Lock()
	extHandles[req.ID] = &extHandle{bundleDir: dir, exitCode: -1}
	extHandlesMu.Unlock()
	return nil
}

func opStartExternal(req *opRequest, hooks *ioHooks) (retErr error) {
	h, err := getExtHandle(req.ID)
	if err != nil {
		// Recover a create that survived a daemon restart.
		dir := bundleDir(req.StoreRoot, req.ID)
		if _, statErr := os.Stat(filepath.Join(dir, "config.json")); statErr != nil {
			return err
		}
		h = &extHandle{bundleDir: dir, exitCode: -1}
		extHandlesMu.Lock()
		extHandles[req.ID] = h
		extHandlesMu.Unlock()
	}
	h.startMu.Lock()
	defer h.startMu.Unlock()
	if h.startErr != nil {
		return h.startErr
	}
	if h.cmd != nil {
		return fail(codeRun, "container has already been started", "")
	}
	spec, err := savedSpec(req.StoreRoot, req.ID)
	if err != nil {
		return err
	}
	if spec.Process == nil {
		return fail(codeInvalid, "spec has no process", "")
	}

	bin, err := externalRuntimeBinary()
	if err != nil {
		return err
	}
	args := append(runtimeFlags(stateRootOf(req)), "run", "--bundle", h.bundleDir, req.ID)
	cmd := exec.Command(bin, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}

	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		return fail(codeInternal, "stdin pipe", err.Error())
	}
	defer stdinRead.Close()
	cmd.Stdin = stdinRead
	h.stdin = stdinWrite

	started := false
	defer func() {
		if !started {
			_ = stdinWrite.Close()
			h.stdin = nil
			if cmd.Process != nil {
				_ = cmd.Process.Kill()
				_, _ = cmd.Process.Wait()
			}
		}
	}()

	var stdoutWrite, stderrWrite *os.File
	if hooks != nil && hooks.emit != nil {
		emit := hooks.emit
		var stdoutSlot io.Writer
		if err := pipeTo(&h.pumps, &stdoutSlot, emit, 1); err != nil {
			return err
		}
		stdoutWrite = stdoutSlot.(*os.File)
		cmd.Stdout = stdoutWrite
		if spec.Process.Terminal {
			cmd.Stderr = stdoutWrite
		} else {
			var stderrSlot io.Writer
			if err := pipeTo(&h.pumps, &stderrSlot, emit, 2); err != nil {
				return err
			}
			stderrWrite = stderrSlot.(*os.File)
			cmd.Stderr = stderrWrite
		}
	} else {
		cmd.Stdout = os.Stdout
		cmd.Stderr = os.Stderr
	}

	if err := cmd.Start(); err != nil {
		return fail(codeRun, "start container", err.Error())
	}
	// Parent writers are only needed until Start duplicates them into the child.
	if stdoutWrite != nil {
		_ = stdoutWrite.Close()
	}
	if stderrWrite != nil {
		_ = stderrWrite.Close()
	}
	_ = stdinRead.Close()
	h.cmd = cmd
	h.waitDone = make(chan struct{})
	go func() {
		err := cmd.Wait()
		if err != nil {
			if exitErr, ok := err.(*exec.ExitError); ok {
				h.exitCode = exitCode(exitErr.ProcessState)
			} else {
				h.waitErr = fail(codeRun, "wait", err.Error())
				h.exitCode = 1
			}
		} else if cmd.ProcessState != nil {
			h.exitCode = exitCode(cmd.ProcessState)
		} else {
			h.exitCode = 0
		}
		close(h.waitDone)
	}()
	started = true
	return nil
}

func opWaitExternal(req *opRequest) (any, error) {
	h, err := getExtHandle(req.ID)
	if err != nil {
		return nil, err
	}
	h.startMu.Lock()
	done := h.waitDone
	h.startMu.Unlock()
	if done == nil {
		return map[string]any{"exit_code": -1}, nil
	}
	<-done
	h.pumps.Wait()
	if h.waitErr != nil {
		return nil, h.waitErr
	}
	return map[string]any{"exit_code": h.exitCode, "oom_killed": h.oomKilled}, nil
}

func opKillExternal(req *opRequest) error {
	h, err := getExtHandle(req.ID)
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
		}
	}
	// Prefer the runtime kill so the sandbox tears down cleanly.
	bin, binErr := externalRuntimeBinary()
	if binErr != nil {
		return binErr
	}
	args := append(runtimeFlags(stateRootOf(req)), "kill", req.ID, fmt.Sprintf("%d", signal))
	if out, err := exec.Command(bin, args...).CombinedOutput(); err != nil {
		if h.cmd != nil && h.cmd.Process != nil {
			if killErr := h.cmd.Process.Signal(signal); killErr != nil && !isFinished(killErr) {
				return fail(codeRun, "signal container", fmt.Sprintf("%v; runtime kill: %s", killErr, out))
			}
			return nil
		}
		// Already gone is success for CRI.
		if isFinished(err) || strings.Contains(string(out), "does not exist") {
			return nil
		}
		return fail(codeRun, "signal container", string(out))
	}
	return nil
}

func opDeleteExternal(req *opRequest) error {
	extHandlesMu.Lock()
	h := extHandles[req.ID]
	extHandlesMu.Unlock()
	if h != nil {
		h.startMu.Lock()
		if h.waitDone != nil {
			select {
			case <-h.waitDone:
			default:
				_ = opKillExternal(&opRequest{ID: req.ID, Signal: int(syscall.SIGKILL), StoreRoot: req.StoreRoot, StateRoot: req.StateRoot})
				<-h.waitDone
			}
		}
		if h.stdin != nil {
			_ = h.stdin.Close()
		}
		h.startMu.Unlock()
	}
	if bin, err := externalRuntimeBinary(); err == nil {
		args := append(runtimeFlags(stateRootOf(req)), "delete", "--force", req.ID)
		_ = exec.Command(bin, args...).Run()
	}
	_ = os.RemoveAll(bundleDir(req.StoreRoot, req.ID))
	_ = os.Remove(filepath.Join(specDir(req.StoreRoot), req.ID+".json"))
	extHandlesMu.Lock()
	delete(extHandles, req.ID)
	extHandlesMu.Unlock()
	return nil
}

func opStateExternal(req *opRequest) (any, error) {
	extHandlesMu.Lock()
	h := extHandles[req.ID]
	extHandlesMu.Unlock()
	if h != nil {
		h.startMu.Lock()
		if h.waitDone != nil {
			defer h.startMu.Unlock()
			select {
			case <-h.waitDone:
				if h.waitErr != nil {
					return nil, h.waitErr
				}
				return map[string]any{"status": "stopped", "pid": 0, "exit_code": h.exitCode, "oom_killed": h.oomKilled}, nil
			default:
				pid := 0
				if h.cmd != nil && h.cmd.Process != nil {
					pid = h.cmd.Process.Pid
				}
				return map[string]any{"status": "running", "pid": pid, "exit_code": -1}, nil
			}
		}
		h.startMu.Unlock()
		return map[string]any{"status": "created", "pid": 0, "exit_code": -1}, nil
	}
	if _, err := os.Stat(filepath.Join(bundleDir(req.StoreRoot, req.ID), "config.json")); err == nil {
		return map[string]any{"status": "created", "pid": 0, "exit_code": -1}, nil
	}
	return nil, fail(codeNotFound, "container not found", req.ID)
}

func opStatsExternal(req *opRequest) (any, error) {
	out := map[string]any{
		"cpu_total_ns":  uint64(0),
		"cpu_system_ns": uint64(0),
		"mem_usage":     uint64(0),
		"mem_limit":     uint64(1<<63 - 1),
		"mem_cache":     uint64(0),
		"mem_swap":      uint64(0),
		"writable_size": uint64(0),
		"inodes_used":   uint64(0),
		"rx_bytes":      uint64(0),
		"tx_bytes":      uint64(0),
		"rx_errors":     uint64(0),
		"tx_errors":     uint64(0),
	}
	if spec, err := savedSpec(req.StoreRoot, req.ID); err == nil && spec.Root != nil {
		size, inodes := dirUsage(spec.Root.Path)
		out["writable_size"] = size
		out["inodes_used"] = inodes
	}
	return out, nil
}

func opUpdateExternal(req *opRequest) error {
	// runsc does not expose a full runc-compatible update for all resources
	// in this path; accept the call so CRI UpdateContainerResources is soft.
	_ = req
	return nil
}

func opExecExternal(req *opRequest, hooks *ioHooks) (any, error) {
	if _, err := getExtHandle(req.ID); err != nil {
		if _, statErr := os.Stat(filepath.Join(bundleDir(req.StoreRoot, req.ID), "config.json")); statErr != nil {
			return nil, err
		}
	}
	if len(req.Cmd) == 0 {
		return nil, fail(codeInvalid, "exec command is empty", "")
	}
	bin, err := externalRuntimeBinary()
	if err != nil {
		return nil, err
	}
	args := append(runtimeFlags(stateRootOf(req)), "exec", req.ID)
	args = append(args, req.Cmd...)
	cmd := exec.Command(bin, args...)
	if hooks != nil && hooks.readInput != nil {
		cmd.Stdin = &callbackReader{read: hooks.readInput}
	}
	if hooks != nil && hooks.emit != nil {
		cmd.Stdout = &callbackWriter{emit: hooks.emit, stream: 1}
		if req.TTY {
			cmd.Stderr = cmd.Stdout
		} else {
			cmd.Stderr = &callbackWriter{emit: hooks.emit, stream: 2}
		}
	}
	if err := cmd.Start(); err != nil {
		return nil, fail(codeRun, "exec", err.Error())
	}
	type waitResult struct {
		state *os.ProcessState
		err   error
	}
	waited := make(chan waitResult, 1)
	go func() {
		state, err := cmd.Process.Wait()
		waited <- waitResult{state, err}
	}()
	var result waitResult
	if req.Timeout > 0 {
		timer := time.NewTimer(time.Duration(req.Timeout) * time.Second)
		defer timer.Stop()
		select {
		case result = <-waited:
		case <-timer.C:
			_ = cmd.Process.Kill()
			<-waited
			return nil, fail(codeTimeout, "exec timed out", "command was killed and reaped")
		}
	} else {
		result = <-waited
	}
	if result.err != nil && result.state == nil {
		return nil, fail(codeRun, "exec wait", result.err.Error())
	}
	return map[string]any{"exit_code": exitCode(result.state)}, nil
}

func opWriteStdinExternal(req *opRequest) error {
	h, err := getExtHandle(req.ID)
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

func opResizeExternal(req *opRequest) error {
	// Piped stdio has no winsize; treat resize as success like libcontainer.
	if _, err := getExtHandle(req.ID); err != nil {
		if _, statErr := os.Stat(filepath.Join(bundleDir(req.StoreRoot, req.ID), "config.json")); statErr != nil {
			return err
		}
	}
	return nil
}

func opCheckpointExternal(req *opRequest) error {
	bin, _ := externalRuntimeBinary()
	return fail(codeUnsupported, "checkpoint is not supported with the external OCI runtime", bin)
}

func isFinished(err error) bool {
	if err == nil {
		return false
	}
	if err == os.ErrProcessDone || err == syscall.ESRCH {
		return true
	}
	msg := err.Error()
	return strings.Contains(msg, "process already finished") || strings.Contains(msg, "no such process")
}
