//go:build linux

// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"encoding/base64"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"

	"go.podman.io/common/libimage"
	"go.podman.io/common/pkg/config"
	"go.podman.io/image/v5/types"
	"go.podman.io/storage"
)

// Image operations on containers/image+storage, through libimage (the same
// stack Buildah uses). The store lives under <store_root>/storage; rootfses
// are storage containers named rok-<container-id> mounted for libcontainer.

var (
	runtimeMu     sync.Mutex
	runtimeByRoot = map[string]*libimage.Runtime{}
	storeByRoot   = map[string]storage.Store{}
)

func imageRuntime(root string, insecure bool, auth *authConfig) (*libimage.Runtime, storage.Store, error) {
	runtimeMu.Lock()
	defer runtimeMu.Unlock()
	if rt, ok := runtimeByRoot[root]; ok {
		return rt, storeByRoot[root], nil
	}
	if root == "" {
		root = "/var/lib/oci-cri"
	}
	graphRoot := filepath.Join(root, "storage")
	runRoot := filepath.Join(root, "runroot")
	for _, dir := range []string{graphRoot, runRoot} {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return nil, nil, fail(codeInternal, "image store directory", err.Error())
		}
	}
	driver := "overlay"
	store, err := storage.GetStore(storage.StoreOptions{
		GraphRoot:       graphRoot,
		RunRoot:         runRoot,
		GraphDriverName: driver,
	})
	if err != nil {
		// Rootless and some guests cannot overlay; vfs always works.
		store, err = storage.GetStore(storage.StoreOptions{
			GraphRoot:       graphRoot,
			RunRoot:         runRoot,
			GraphDriverName: "vfs",
		})
		if err != nil {
			return nil, nil, fail(codePrereq, "opening image store", err.Error())
		}
	}
	sys := &types.SystemContext{
		DockerInsecureSkipTLSVerify: types.OptionalBoolFalse,
	}
	if insecure {
		sys.DockerInsecureSkipTLSVerify = types.OptionalBoolTrue
	}
	if auth != nil && (auth.Username != "" || auth.Password != "" || auth.Auth != "") {
		sys.DockerAuthConfig = &types.DockerAuthConfig{
			Username: auth.Username,
			Password: auth.Password,
		}
		if auth.Auth != "" && auth.Username == "" {
			if decoded, err := base64.StdEncoding.DecodeString(auth.Auth); err == nil {
				if user, pass, ok := strings.Cut(string(decoded), ":"); ok {
					sys.DockerAuthConfig.Username = user
					sys.DockerAuthConfig.Password = pass
				}
			}
		}
		if auth.IdentityToken != "" {
			sys.DockerAuthConfig.IdentityToken = auth.IdentityToken
		}
		if auth.ServerAddress != "" {
			sys.DockerDaemonHost = auth.ServerAddress
		}
	}
	policyPath := filepath.Join(root, "policy.json")
	if _, err := os.Stat(policyPath); err != nil {
		// No local signature policy: accept images the registry serves over
		// (optionally verified) TLS, like a default CRI node config.
		_ = os.WriteFile(policyPath, []byte(`{"default":[{"type":"insecureAcceptAnything"}]}`), 0o644)
	}
	sys.SignaturePolicyPath = policyPath
	rt, err := libimage.RuntimeFromStore(store, &libimage.RuntimeOptions{SystemContext: sys})
	if err != nil {
		return nil, nil, fail(codeInternal, "image runtime", err.Error())
	}
	runtimeByRoot[root] = rt
	storeByRoot[root] = store
	return rt, store, nil
}

func ctx() context.Context { return context.Background() }

func opPullImage(req *opRequest) (any, error) {
	if req.Image == "" {
		return nil, fail(codeInvalid, "image is empty", "")
	}
	rt, _, err := imageRuntime(req.StoreRoot, req.Insecure, req.Auth)
	if err != nil {
		return nil, err
	}
	pulled, err := rt.Pull(ctx(), req.Image, config.PullPolicyMissing, &libimage.PullOptions{})
	if err != nil {
		return nil, fail(codeRun, "pull image", err.Error())
	}
	if len(pulled) == 0 {
		return nil, fail(codeRun, "pull returned no image", req.Image)
	}
	// Tags of the same stored image must return the same CRI image identity.
	return map[string]any{"image_ref": pulled[0].ID()}, nil
}

func convertImage(img *libimage.Image) (map[string]any, error) {
	size, err := img.Size()
	if err != nil {
		return nil, err
	}
	tags, err := img.RepoTags()
	if err != nil {
		return nil, err
	}
	repoDigests, err := img.RepoDigests()
	if err != nil {
		return nil, err
	}
	out := map[string]any{
		"id":           img.ID(),
		"repo_tags":    tags,
		"repo_digests": repoDigests,
		"size":         uint64(size),
	}
	if data, err := img.Inspect(ctx(), &libimage.InspectOptions{}); err == nil && data.Config != nil {
		cfg := data.Config
		spec := map[string]any{
			"user":        cfg.User,
			"env":         cfg.Env,
			"entrypoint":  cfg.Entrypoint,
			"cmd":         cfg.Cmd,
			"working_dir": cfg.WorkingDir,
			"labels":      cfg.Labels,
			"stop_signal": cfg.StopSignal,
		}
		exposed := map[string]string{}
		for port := range cfg.ExposedPorts {
			exposed[port] = ""
		}
		spec["exposed_ports"] = exposed
		volumes := map[string]string{}
		for volume := range cfg.Volumes {
			volumes[volume] = ""
		}
		spec["volumes"] = volumes
		out["spec"] = spec
		if cfg.User != "" {
			uidText := strings.Split(cfg.User, ":")[0]
			if uid, err := strconv.ParseInt(uidText, 10, 64); err == nil {
				out["uid"] = uid
			} else {
				out["username"] = uidText
			}
		}
	}
	return out, nil
}

func opListImages(req *opRequest) (any, error) {
	rt, _, err := imageRuntime(req.StoreRoot, req.Insecure, req.Auth)
	if err != nil {
		return nil, err
	}
	images, err := rt.ListImages(ctx(), &libimage.ListImagesOptions{})
	if err != nil {
		return nil, fail(codeInternal, "list images", err.Error())
	}
	list := []map[string]any{}
	for _, img := range images {
		converted, err := convertImage(img)
		if err != nil {
			return nil, fail(codeRun, "image metadata", err.Error())
		}
		if req.Filter != "" {
			id, _ := converted["id"].(string)
			matched := id == req.Filter
			if tags, ok := converted["repo_tags"].([]string); ok {
				for _, tag := range tags {
					if tag == req.Filter {
						matched = true
					}
				}
			}
			if !matched {
				continue
			}
		}
		list = append(list, converted)
	}
	return map[string]any{"images": list}, nil
}

func opRemoveImage(req *opRequest) error {
	rt, _, err := imageRuntime(req.StoreRoot, req.Insecure, req.Auth)
	if err != nil {
		return err
	}
	reports, errs := rt.RemoveImages(ctx(), []string{req.Image}, &libimage.RemoveImagesOptions{Force: true})
	_ = reports
	for _, err := range errs {
		if err != nil {
			return fail(codeRun, "remove image", err.Error())
		}
	}
	return nil
}

func opImageFsInfo(req *opRequest) (any, error) {
	_, store, err := imageRuntime(req.StoreRoot, req.Insecure, req.Auth)
	if err != nil {
		return nil, err
	}
	graphRoot := ""
	if store != nil {
		graphRoot = store.GraphRoot()
	}
	var used uint64
	if graphRoot != "" {
		used, _ = dirUsage(graphRoot)
	}
	return map[string]any{
		"filesystems": []map[string]any{
			{
				"timestamp_ns": unixNano(),
				"mountpoint":   graphRoot,
				"used_bytes":   used,
				"inodes_used":  uint64(0),
			},
		},
	}, nil
}

func storageContainerName(id string) string { return "rok-" + id }

func findStorageContainer(store storage.Store, id string) (*storage.Container, error) {
	containers, err := store.Containers()
	if err != nil {
		return nil, err
	}
	name := storageContainerName(id)
	for i := range containers {
		for _, n := range containers[i].Names {
			if n == name || n == id {
				return &containers[i], nil
			}
		}
		if containers[i].ID == id {
			return &containers[i], nil
		}
	}
	return nil, storage.ErrContainerUnknown
}

func opPrepareRootfs(req *opRequest) (any, error) {
	_, store, err := imageRuntime(req.StoreRoot, req.Insecure, req.Auth)
	if err != nil {
		return nil, err
	}
	img, _, err := imageRuntimeLookup(req)
	if err != nil {
		return nil, err
	}
	_ = img
	if existing, err := findStorageContainer(store, req.ID); err == nil {
		mount, err := store.Mount(existing.ID, "")
		if err != nil {
			return nil, fail(codeRun, "mount rootfs", err.Error())
		}
		return map[string]any{"rootfs": mount}, nil
	}
	container, err := store.CreateContainer("", []string{storageContainerName(req.ID)}, img.ID(), "", "", nil)
	if err != nil {
		return nil, fail(codeRun, "create storage container", err.Error())
	}
	mount, err := store.Mount(container.ID, "")
	if err != nil {
		_ = store.DeleteContainer(container.ID)
		return nil, fail(codeRun, "mount rootfs", err.Error())
	}
	return map[string]any{"rootfs": mount}, nil
}

func imageRuntimeLookup(req *opRequest) (*libimage.Image, storage.Store, error) {
	rt, store, err := imageRuntime(req.StoreRoot, req.Insecure, req.Auth)
	if err != nil {
		return nil, nil, err
	}
	img, _, err := rt.LookupImage(req.Image, &libimage.LookupImageOptions{})
	if err != nil {
		return nil, nil, fail(codeNotFound, "image not present", err.Error())
	}
	return img, store, nil
}

func opReleaseRootfs(req *opRequest) error {
	_, store, err := imageRuntime(req.StoreRoot, req.Insecure, req.Auth)
	if err != nil {
		return err
	}
	container, err := findStorageContainer(store, req.ID)
	if err != nil {
		return nil // already released
	}
	_, _ = store.Unmount(container.ID, true)
	err = store.DeleteContainer(container.ID)
	if err != nil {
		return fail(codeRun, "delete storage container", err.Error())
	}
	return nil
}
