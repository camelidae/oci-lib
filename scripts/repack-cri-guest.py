#!/usr/bin/env python3
"""Repack the published oci-cri guest with the current init, agent, and CNI plugins."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import re
import tarfile
import tempfile
import urllib.request
from pathlib import Path


PACKAGE_VERSION = "0.1.15"
PROTOCOL_VERSION = 2
CNI_PLUGINS_URL = (
    "https://github.com/containernetworking/plugins/releases/download/v1.9.1/"
    "cni-plugins-linux-arm64-v1.9.1.tgz"
)
CNI_PLUGINS_SHA256 = "56171987d3947707c3563db2f4001bccaf50fd63468611b9f3cbecb1375ee7ec"
CNI_PLUGIN_NAMES = ("bridge", "host-local", "loopback")
S_IFDIR = 0o040000
S_IFREG = 0o100000


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def align4(data: bytearray) -> None:
    data.extend(b"\0" * (-len(data) % 4))


def canonical_name(name: bytes) -> str:
    return name.decode("utf-8").removeprefix("./")


def newc_record(name: str, mode: int, data: bytes, inode: int) -> bytes:
    name_bytes = name.encode("utf-8") + b"\0"
    fields = (
        inode,
        mode,
        0,
        0,
        2 if mode & S_IFDIR else 1,
        0,
        len(data),
        0,
        0,
        0,
        0,
        len(name_bytes),
        0,
    )
    output = bytearray(b"070701")
    output.extend("".join(f"{field:08x}" for field in fields).encode())
    output.extend(name_bytes)
    align4(output)
    output.extend(data)
    align4(output)
    return bytes(output)


def replace_members(
    archive: bytes,
    updates: dict[str, tuple[int, bytes]],
    additions: dict[str, tuple[int, bytes]],
) -> bytes:
    output = bytearray()
    offset = 0
    found: set[str] = set()
    trailer_found = False
    inode = 1
    while offset < len(archive):
        start = offset
        header = archive[offset : offset + 110]
        if len(header) != 110 or header[:6] not in (b"070701", b"070702"):
            raise ValueError(f"invalid newc header at offset {offset}")
        try:
            file_size = int(header[54:62], 16)
            name_size = int(header[94:102], 16)
        except ValueError as error:
            raise ValueError(f"invalid newc fields at offset {offset}") from error
        name_start = offset + 110
        name_end = name_start + name_size
        data_start = (name_end + 3) & ~3
        data_end = data_start + file_size
        record_end = (data_end + 3) & ~3
        if name_size < 1 or record_end > len(archive):
            raise ValueError(f"truncated newc record at offset {offset}")
        name = archive[name_start : name_end - 1]
        normalized = canonical_name(name)
        payload = archive[data_start:data_end]
        offset = record_end

        if normalized in found:
            raise ValueError(f"newc archive contains duplicate member {normalized}")
        found.add(normalized)
        inode += 1
        update = updates.get(normalized) or additions.get(normalized)
        if update is not None:
            mode, payload = update
            header = header[:14] + f"{mode:08x}".encode() + header[22:]
            header = header[:54] + f"{len(payload):08x}".encode() + header[62:]
            if header[:6] == b"070702":
                header = header[:102] + f"{sum(payload) & 0xFFFFFFFF:08x}".encode()

        if name == b"TRAILER!!!":
            for added_name, (mode, data) in sorted(additions.items()):
                if added_name not in found:
                    output.extend(newc_record(added_name, mode, data, inode))
                    inode += 1
            output.extend(header)
            output.extend(archive[name_start:name_end])
            align4(output)
            output.extend(payload)
            align4(output)
            trailer_found = True
            break

        output.extend(header)
        output.extend(archive[name_start:name_end])
        align4(output)
        output.extend(payload)
        align4(output)
        if offset <= start:
            raise ValueError("newc parser made no progress")

    missing = set(updates) - found
    if missing:
        raise ValueError(f"newc archive is missing required members: {', '.join(sorted(missing))}")
    if not trailer_found:
        raise ValueError("newc archive has no trailer entry")
    return bytes(output) + archive[offset:]


def download_cni_plugins(path: Path) -> bytes:
    downloaded = not path.is_file()
    if not downloaded:
        data = path.read_bytes()
    else:
        with urllib.request.urlopen(CNI_PLUGINS_URL, timeout=60) as response:
            data = response.read()
    if sha256(data) != CNI_PLUGINS_SHA256:
        raise ValueError(f"CNI plugin tarball checksum mismatch: {path}")
    if downloaded:
        write_atomic(path, data)
    return data


def cni_plugin_files(tarball: bytes) -> dict[str, bytes]:
    plugins = {}
    with tarfile.open(fileobj=io.BytesIO(tarball), mode="r:gz") as archive:
        members = {member.name.removeprefix("./"): member for member in archive.getmembers()}
        for name in CNI_PLUGIN_NAMES:
            member = members.get(name)
            if member is None or not member.isfile():
                raise ValueError(f"CNI plugin archive is missing regular file {name}")
            stream = archive.extractfile(member)
            if stream is None:
                raise ValueError(f"could not read CNI plugin {name}")
            plugins[name] = stream.read()
    return plugins


def current_init(repo: Path) -> bytes:
    source = (repo / "xtask/src/guest.rs").read_text(encoding="utf-8")
    match = re.search(r'pub const CRI_INIT: &str = r#"(.*?)"#;', source, re.DOTALL)
    if match is None:
        raise ValueError("could not find CRI_INIT in xtask/src/guest.rs")
    return match.group(1).encode()


def write_atomic(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
        temporary.write(data)
        temporary_path = Path(temporary.name)
    temporary_path.replace(path)


def repack(source: Path, output: Path, repo: Path, agent: Path, cni_tgz: Path) -> None:
    kernel = (source / "vmlinuz").read_bytes()
    original_initrd = (source / "initramfs").read_bytes()
    attestation = json.loads((source / "attestation.json").read_text(encoding="utf-8"))

    if attestation.get("package") != "oci-cri":
        raise ValueError("source attestation is not for oci-cri")
    if attestation.get("package_version") != PACKAGE_VERSION:
        raise ValueError(f"expected oci-cri {PACKAGE_VERSION} guest")
    if attestation.get("protocol_version") != PROTOCOL_VERSION:
        raise ValueError(f"expected guest protocol {PROTOCOL_VERSION}")
    if attestation.get("kernel_sha256") != sha256(kernel):
        raise ValueError("source kernel does not match its attestation")
    if attestation.get("initramfs_sha256") != sha256(original_initrd):
        raise ValueError("source initramfs does not match its attestation")

    if original_initrd.startswith(b"\x1f\x8b"):
        archive = gzip.decompress(original_initrd)
    elif original_initrd.startswith((b"070701", b"070702")):
        archive = original_initrd
    else:
        raise ValueError("source initramfs is neither gzip nor newc cpio")

    init = current_init(repo)
    plugin_files = cni_plugin_files(download_cni_plugins(cni_tgz))
    updates = {
        "init": (S_IFREG | 0o755, init),
        "oci-cri": (S_IFREG | 0o755, agent.read_bytes()),
    }
    additions = {
        "opt": (S_IFDIR | 0o755, b""),
        "opt/cni": (S_IFDIR | 0o755, b""),
        "opt/cni/bin": (S_IFDIR | 0o755, b""),
        **{
            f"opt/cni/bin/{name}": (S_IFREG | 0o755, data)
            for name, data in plugin_files.items()
        },
    }
    packed_initrd = gzip.compress(
        replace_members(archive, updates, additions), compresslevel=9, mtime=0
    )
    members = attestation.get("members")
    if not isinstance(members, list):
        raise ValueError("source attestation has no members list")
    members_by_path = {
        member["path"]: member
        for member in members
        if isinstance(member, dict) and isinstance(member.get("path"), str)
    }
    for path, (mode, data) in {**updates, **additions}.items():
        members_by_path[path] = {
            "path": path,
            "mode": mode,
            "size": len(data),
            "sha256": sha256(data),
        }
    if "init" not in members_by_path or "oci-cri" not in members_by_path:
        raise ValueError("source attestation does not list init and oci-cri members")
    attestation["members"] = [members_by_path[path] for path in sorted(members_by_path)]
    attestation["initramfs_sha256"] = sha256(packed_initrd)

    write_atomic(output / "vmlinuz", kernel)
    write_atomic(output / "initramfs", packed_initrd)
    write_atomic(
        output / "attestation.json",
        (json.dumps(attestation, indent=2) + "\n").encode(),
    )


def main() -> None:
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source-dir",
        type=Path,
        default=Path.home() / "Library/Caches/oci-cri/downloads/v0.1.15-aarch64",
        help="directory containing the verified v0.1.15 aarch64 guest release files",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=repo / "crates/oci-cri/guest/out",
        help="output directory used by the local guest embed build",
    )
    parser.add_argument(
        "--agent",
        type=Path,
        required=True,
        help="Linux oci-cri agent binary rebuilt with guest CNI support",
    )
    parser.add_argument(
        "--cni-tgz",
        type=Path,
        default=Path.home() / "Library/Caches/oci-cri/downloads/cni-plugins-linux-arm64-v1.9.1.tgz",
        help="verified CNI plugin archive path; downloaded if missing",
    )
    args = parser.parse_args()
    repack(args.source_dir, args.output_dir, repo, args.agent, args.cni_tgz)
    print(f"repacked local oci-cri guest into {args.output_dir}")


if __name__ == "__main__":
    main()