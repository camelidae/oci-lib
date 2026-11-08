#!/usr/bin/env python3
"""Feed diorama-vm kernel/runsc into the oci-cri guest embed layout.

Updates crates/oci-cri/guest/out/{vmlinuz,initramfs,attestation.json} so the
existing macOS embed path (build.rs / ROK_GUEST_*) picks up:

- kernel Image from diorama-vm
- /bin/runsc from diorama-vm
- current CRI_INIT (sets ROK_OCI_RUNTIME when runsc is present)
- optional rebuilt Linux oci-cri agent with the external-runtime shim
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import re
import sys
import tempfile
from pathlib import Path

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


def load_cpio(initrd: bytes) -> bytes:
    if initrd.startswith(b"\x1f\x8b"):
        return gzip.decompress(initrd)
    if initrd.startswith((b"070701", b"070702")):
        return initrd
    raise ValueError("initramfs is neither gzip nor newc cpio")


def wire(
    source: Path,
    output: Path,
    repo: Path,
    kernel: Path,
    runsc: Path,
    agent: Path | None,
) -> None:
    original_initrd = (source / "initramfs").read_bytes()
    attestation = json.loads((source / "attestation.json").read_text(encoding="utf-8"))
    if attestation.get("package") != "oci-cri":
        raise ValueError("source attestation is not for oci-cri")
    if attestation.get("protocol_version") != 2:
        raise ValueError("expected guest protocol 2")

    archive = load_cpio(original_initrd)
    init = current_init(repo)
    runsc_bytes = runsc.read_bytes()
    kernel_bytes = kernel.read_bytes()

    updates: dict[str, tuple[int, bytes]] = {
        "init": (S_IFREG | 0o755, init),
    }
    if agent is not None:
        updates["oci-cri"] = (S_IFREG | 0o755, agent.read_bytes())

    additions: dict[str, tuple[int, bytes]] = {
        "bin": (S_IFDIR | 0o755, b""),
        "bin/runsc": (S_IFREG | 0o755, runsc_bytes),
        "var": (S_IFDIR | 0o755, b""),
        "var/run": (S_IFDIR | 0o755, b""),
        "var/run/runsc": (S_IFDIR | 0o755, b""),
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
    attestation["members"] = [members_by_path[path] for path in sorted(members_by_path)]
    attestation["kernel_sha256"] = sha256(kernel_bytes)
    attestation["initramfs_sha256"] = sha256(packed_initrd)

    write_atomic(output / "vmlinuz", kernel_bytes)
    write_atomic(output / "initramfs", packed_initrd)
    write_atomic(
        output / "attestation.json",
        (json.dumps(attestation, indent=2, sort_keys=False) + "\n").encode(),
    )


def main() -> int:
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source-dir",
        type=Path,
        default=repo / "crates/oci-cri/guest/out",
        help="existing oci-cri guest/out used as the base archive",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=repo / "crates/oci-cri/guest/out",
        help="output guest/out directory for the macOS embed",
    )
    parser.add_argument(
        "--diorama-vm",
        type=Path,
        default=Path("/Volumes/safe-vol/workspace/orgs/di-framework/diorama-vm"),
        help="diorama-vm checkout providing artifacts/Image and artifacts/runsc",
    )
    parser.add_argument(
        "--kernel",
        type=Path,
        default=None,
        help="override kernel Image path (defaults to <diorama-vm>/artifacts/Image)",
    )
    parser.add_argument(
        "--runsc",
        type=Path,
        default=None,
        help="override runsc path (defaults to <diorama-vm>/artifacts/runsc)",
    )
    parser.add_argument(
        "--agent",
        type=Path,
        default=None,
        help="optional rebuilt Linux oci-cri binary with the external-runtime shim",
    )
    args = parser.parse_args()
    kernel = args.kernel or (args.diorama_vm / "artifacts/Image")
    runsc = args.runsc or (args.diorama_vm / "artifacts/runsc")
    for label, path in (("kernel", kernel), ("runsc", runsc)):
        if not path.is_file():
            print(f"missing {label}: {path}", file=sys.stderr)
            return 1
    if args.agent is not None and not args.agent.is_file():
        print(f"missing agent: {args.agent}", file=sys.stderr)
        return 1
    wire(args.source_dir, args.output_dir, repo, kernel, runsc, args.agent)
    print(f"wired diorama-vm guest into {args.output_dir}")
    print(f"  kernel: {kernel}")
    print(f"  runsc:  {runsc}")
    if args.agent is not None:
        print(f"  agent:  {args.agent}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
