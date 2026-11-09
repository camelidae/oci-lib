//go:build linux

// SPDX-License-Identifier: Apache-2.0

package main

import (
	"encoding/json"
	"errors"
)

const (
	codeInvalid     = 1
	codeNotFound    = 3
	codePrereq      = 4
	codeRun         = 5
	codeUnsupported = 8
	codeInternal    = 9
	codeState       = 10
	codeTimeout     = 11
)

var errInput = errors.New("stdin input failed")

type shimError struct {
	code    int
	message string
	detail  string
}

func (e *shimError) Error() string { return e.message }

func fail(code int, message, detail string) *shimError {
	return &shimError{code: code, message: message, detail: detail}
}

// ioHooks carries the C callbacks of one rok_call into the engine. For
// "start" the emit hook stays registered in the handle until "wait".
type ioHooks struct {
	emit      func(stream int32, data []byte)
	readInput func(buf []byte) (int, error)
}

type authConfig struct {
	Username      string `json:"username"`
	Password      string `json:"password"`
	Auth          string `json:"auth"`
	ServerAddress string `json:"server_address"`
	IdentityToken string `json:"identity_token"`
}

// opRequest is the JSON vocabulary shared with src/engine.rs.
type opRequest struct {
	Rootfs  string           `json:"rootfs"`
	User    string           `json:"user"`
	Groups  []uint32         `json:"groups"`
	Strict  bool             `json:"strict"`
	Timeout int64            `json:"timeout"`
	FD      int              `json:"fd"`
	Header  []byte           `json:"header"`
	Session streamingSession `json:"session"`
	LogPath string           `json:"log_path"`

	Op        string          `json:"op"`
	StoreRoot string          `json:"store_root"`
	StateRoot string          `json:"state_root"`
	ID        string          `json:"id"`
	Image     string          `json:"image"`
	Platform  string          `json:"platform"`
	Filter    string          `json:"filter"`
	Spec      json.RawMessage `json:"spec"`
	Cmd       []string        `json:"cmd"`
	TTY       bool            `json:"tty"`
	Signal    int             `json:"signal"`
	Width     uint16          `json:"width"`
	Height    uint16          `json:"height"`
	Data      string          `json:"data"` // base64 stdin bytes
	Resources json.RawMessage `json:"resources"`
	Parent    string          `json:"parent"`
	Auth      *authConfig     `json:"auth"`
	Insecure  bool            `json:"insecure"`
}

func dispatch(req *opRequest, hooks *ioHooks) (string, error) {
	var (
		response any
		err      error
	)
	switch req.Op {
	case "resolve_user":
		response, err = opResolveUser(req)
	case "spdy":
		err = opSPDY(req)

	case "pull_image":
		response, err = opPullImage(req)
	case "list_images":
		response, err = opListImages(req)
	case "remove_image":
		err = opRemoveImage(req)
		response = map[string]any{}
	case "image_fs_info":
		response, err = opImageFsInfo(req)
	case "prepare_rootfs":
		response, err = opPrepareRootfs(req)
	case "release_rootfs":
		err = opReleaseRootfs(req)
		response = map[string]any{}
	case "create":
		if useExternalRuntime() {
			err = opCreateExternal(req)
		} else {
			err = opCreate(req)
		}
		response = map[string]any{}
	case "start":
		if useExternalRuntime() {
			err = opStartExternal(req, hooks)
		} else {
			err = opStart(req, hooks)
		}
		response = map[string]any{}
	case "wait":
		if useExternalRuntime() {
			response, err = opWaitExternal(req)
		} else {
			response, err = opWait(req)
		}
	case "kill":
		if useExternalRuntime() {
			err = opKillExternal(req)
		} else {
			err = opKill(req)
		}
		response = map[string]any{}
	case "delete":
		if useExternalRuntime() {
			err = opDeleteExternal(req)
		} else {
			err = opDelete(req)
		}
		response = map[string]any{}
	case "state":
		if useExternalRuntime() {
			response, err = opStateExternal(req)
		} else {
			response, err = opState(req)
		}
	case "stats":
		if useExternalRuntime() {
			response, err = opStatsExternal(req)
		} else {
			response, err = opStats(req)
		}
	case "update":
		if useExternalRuntime() {
			err = opUpdateExternal(req)
		} else {
			err = opUpdate(req)
		}
		response = map[string]any{}
	case "exec":
		if useExternalRuntime() {
			response, err = opExecExternal(req, hooks)
		} else {
			response, err = opExec(req, hooks)
		}
	case "write_stdin":
		if useExternalRuntime() {
			err = opWriteStdinExternal(req)
		} else {
			err = opWriteStdin(req)
		}
		response = map[string]any{}
	case "resize":
		if useExternalRuntime() {
			err = opResizeExternal(req)
		} else {
			err = opResize(req)
		}
		response = map[string]any{}
	case "checkpoint":
		if useExternalRuntime() {
			err = opCheckpointExternal(req)
		} else {
			err = opCheckpoint(req)
		}
		response = map[string]any{}
	default:
		return "", fail(codeInvalid, "unknown op "+req.Op, "")
	}
	if err != nil {
		return "", err
	}
	encoded, err := json.Marshal(response)
	if err != nil {
		return "", fail(codeInternal, "encoding response", err.Error())
	}
	return string(encoded), nil
}
