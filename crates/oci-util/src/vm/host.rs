// SPDX-License-Identifier: Apache-2.0

//! Apple Virtualization.framework boot, vsock connect, and guest materialize.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{env, sync::Mutex as StdMutex};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use libc::c_int;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZDirectorySharingDeviceConfiguration, VZEntropyDeviceConfiguration,
    VZFileHandleSerialPortAttachment, VZGenericPlatformConfiguration, VZLinuxBootLoader,
    VZNATNetworkDeviceAttachment, VZNetworkDeviceConfiguration, VZSerialPortConfiguration,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtioEntropyDeviceConfiguration,
    VZVirtioFileSystemDeviceConfiguration, VZVirtioNetworkDeviceConfiguration,
    VZVirtioSocketDevice, VZVirtioSocketDeviceConfiguration, VZVirtualMachine,
    VZVirtualMachineConfiguration,
};

use crate::vm::config::GuestConfig;
use crate::vm::error::{VmError, VmErrorKind};
use crate::vm::shares::Export;

/// A booted guest without an open vsock. Use [`connect_guest_vsock`] for each
/// connection (CRI proxies many unix sockets onto many vsock dials).
pub struct BootedVm {
    /// Virtual machine object (not `Send`; all calls go through `queue`).
    pub vm: Retained<VZVirtualMachine>,
    /// Serial dispatch queue owning the VM.
    pub queue: DispatchRetained<DispatchQueue>,
    /// Console drain thread (kept alive for the guest lifetime).
    pub console: JoinHandle<()>,
}

/// A booted guest with one open vsock pair. Framed products (builder/runner)
/// run their protocol handshake on [`StartedVm::read`] / the write half.
pub struct StartedVm {
    /// Virtual machine object (not `Send`; all calls go through `queue`).
    pub vm: Retained<VZVirtualMachine>,
    /// Serial dispatch queue owning the VM.
    pub queue: DispatchRetained<DispatchQueue>,
    /// Console drain thread (kept alive for the guest lifetime).
    pub console: JoinHandle<()>,
    /// Host side of the vsock for reading guest frames.
    pub read: File,
    /// Host side of the vsock for writing host frames.
    pub write: File,
}

/// Whether Virtualization.framework is available on this Mac.
pub fn virtualization_supported() -> bool {
    unsafe { VZVirtualMachine::isSupported() }
}

/// Boot the guest without connecting vsock.
///
/// Use this when the product opens many vsock connections (CRI byte proxy).
/// Framed single-channel products should call [`start_vm`] instead.
pub fn boot_vm(
    guest: &GuestConfig,
    kernel: &Path,
    initrd: &Path,
    exports: &[Export],
) -> Result<BootedVm, VmError> {
    let queue = DispatchQueue::new(guest.queue_label, DispatchQueueAttr::SERIAL);
    let (console_read, config) = vm_config(guest, kernel, initrd, exports)?;
    let console = spawn_console(console_read, guest.console_thread_name);
    let config_ptr = Retained::as_ptr(&config) as usize;
    let queue_ptr = &*queue as *const DispatchQueue as usize;
    let vm_slot = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
    let vm_slot_queue = Arc::clone(&vm_slot);
    queue.exec_sync(move || {
        let config = unsafe { &*(config_ptr as *const VZVirtualMachineConfiguration) };
        let queue_ref = unsafe { &*(queue_ptr as *const DispatchQueue) };
        let vm = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                config,
                queue_ref,
            )
        };
        vm_slot_queue.store(Retained::into_raw(vm), Ordering::SeqCst);
    });
    let vm = unsafe { Retained::from_raw(vm_slot.load(Ordering::SeqCst)) }
        .ok_or_else(|| VmError::new(VmErrorKind::Internal, "virtual machine was not created"))?;
    let vm_ptr = Retained::as_ptr(&vm) as usize;
    wait_on_queue(&queue, guest.boot_timeout, move |done| {
        let vm = unsafe { &*(vm_ptr as *const VZVirtualMachine) };
        let once = StdMutex::new(Some(done));
        unsafe {
            vm.startWithCompletionHandler(&RcBlock::new(move |err: *mut NSError| {
                if let Some(done) = once
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take()
                {
                    done(ns_result(err));
                }
            }));
        }
    })?;
    Ok(BootedVm { vm, queue, console })
}

/// Connect a new host vsock to the guest agent port and return read/write halves.
pub fn connect_guest_vsock(
    vm: &Retained<VZVirtualMachine>,
    queue: &DispatchQueue,
    port: u32,
    timeout: Duration,
) -> Result<(File, File), VmError> {
    let vm_ptr = Retained::as_ptr(vm) as usize;
    let fd = connect_vsock(vm_ptr, queue, port, timeout)?;
    prepare_fd(fd);
    let write_fd = unsafe { libc::dup(fd) };
    if write_fd < 0 {
        unsafe { libc::close(fd) };
        return Err(io_err(io::Error::last_os_error()));
    }
    prepare_fd(write_fd);
    let read = unsafe { File::from(OwnedFd::from_raw_fd(fd)) };
    let write = unsafe { File::from(OwnedFd::from_raw_fd(write_fd)) };
    Ok((read, write))
}

/// Boot the guest, connect one vsock, and return read/write file halves.
///
/// Does not perform the product protocol hello; callers do that next.
pub fn start_vm(
    guest: &GuestConfig,
    kernel: &Path,
    initrd: &Path,
    exports: &[Export],
) -> Result<StartedVm, VmError> {
    let booted = boot_vm(guest, kernel, initrd, exports)?;
    let (read, write) = connect_guest_vsock(
        &booted.vm,
        &booted.queue,
        guest.vsock_port,
        guest.boot_timeout,
    )?;
    Ok(StartedVm {
        vm: booted.vm,
        queue: booted.queue,
        console: booted.console,
        read,
        write,
    })
}

impl BootedVm {
    /// Stop the guest virtual machine.
    pub fn stop(self) {
        stop_virtual_machine(self.vm, &self.queue);
    }
}

impl StartedVm {
    /// Stop the guest virtual machine.
    pub fn stop(self) {
        stop_virtual_machine(self.vm, &self.queue);
    }
}

/// Stop a virtual machine previously returned by [`start_vm`].
pub fn stop_virtual_machine(vm: Retained<VZVirtualMachine>, queue: &DispatchQueue) {
    let vm_ptr = Retained::as_ptr(&vm) as usize;
    let _ = wait_on_queue(queue, Duration::from_secs(10), move |done| {
        let vm = unsafe { &*(vm_ptr as *const VZVirtualMachine) };
        let once = StdMutex::new(Some(done));
        unsafe {
            vm.stopWithCompletionHandler(&RcBlock::new(move |_err: *mut NSError| {
                if let Some(done) = once
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take()
                {
                    done(Ok(()));
                }
            }));
        }
    });
}

/// Poll until `file` is readable or `timeout` elapses.
pub fn wait_readable(file: &File, timeout: Duration) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let result = unsafe { libc::poll(&mut poll, 1, millis) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for guest protocol handshake",
        ));
    }
    if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "guest socket closed during protocol handshake",
        ));
    }
    Ok(())
}

/// Write embedded zstd guest images into the product cache directory.
pub fn materialize_embedded(
    guest: &GuestConfig,
    kernel_zstd: &[u8],
    initrd_zstd: &[u8],
) -> Result<(PathBuf, PathBuf), VmError> {
    let dir = guest_cache_dir(guest).join(format!("{}-{}", kernel_zstd.len(), initrd_zstd.len()));
    let kernel = dir.join("vmlinuz");
    let initrd = dir.join("initramfs");
    write_embedded(&kernel, &decompress(kernel_zstd)?)?;
    write_embedded(&initrd, &decompress(initrd_zstd)?)?;
    Ok((kernel, initrd))
}

fn guest_cache_dir(guest: &GuestConfig) -> PathBuf {
    match env::var("HOME") {
        Ok(home) if !home.is_empty() => {
            PathBuf::from(home).join(format!("Library/Caches/{}", guest.cache_dir_name))
        }
        _ => env::temp_dir().join(format!("{}-guest-{}", guest.cache_dir_name, unsafe {
            libc::getuid()
        })),
    }
}

fn decompress(bytes: &[u8]) -> Result<Vec<u8>, VmError> {
    zstd::decode_all(bytes).map_err(|err| {
        VmError::new(
            VmErrorKind::Internal,
            format!("decompressing the embedded guest: {err}"),
        )
    })
}

fn write_embedded(path: &Path, bytes: &[u8]) -> Result<(), VmError> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == bytes {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            VmError::new(
                VmErrorKind::Internal,
                format!("creating {}: {err}", parent.display()),
            )
        })?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes).map_err(|err| {
        VmError::new(
            VmErrorKind::Internal,
            format!("writing {}: {err}", tmp.display()),
        )
    })?;
    std::fs::rename(&tmp, path).map_err(|err| {
        VmError::new(
            VmErrorKind::Internal,
            format!("renaming {} to {}: {err}", tmp.display(), path.display()),
        )
    })
}

fn connect_vsock(
    vm_ptr: usize,
    queue: &DispatchQueue,
    port: u32,
    boot_timeout: Duration,
) -> Result<c_int, VmError> {
    let deadline = Instant::now() + boot_timeout;
    let mut last = String::from("guest has not opened the virtio socket");
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt = wait_on_queue(queue, remaining, move |done| {
            let vm = unsafe { &*(vm_ptr as *const VZVirtualMachine) };
            let once = StdMutex::new(Some(done));
            unsafe {
                let devices = vm.socketDevices();
                if devices.count() == 0 {
                    if let Some(done) = once
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .take()
                    {
                        done(Err("virtual machine has no virtio socket device".into()));
                    }
                    return;
                }
                let device = devices.objectAtIndex(0);
                let Ok(socket) = device.downcast::<VZVirtioSocketDevice>() else {
                    if let Some(done) = once
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .take()
                    {
                        done(Err("socket device is not a virtio socket".into()));
                    }
                    return;
                };
                socket.connectToPort_completionHandler(
                    port,
                    &RcBlock::new(
                        move |conn: *mut objc2_virtualization::VZVirtioSocketConnection,
                              err: *mut NSError| {
                            let Some(done) = once
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .take()
                            else {
                                return;
                            };
                            if conn.is_null() {
                                done(Err(ns_message(err)));
                                return;
                            }
                            let raw = (*conn).fileDescriptor();
                            if raw < 0 {
                                done(Err("virtio socket connection is closed".into()));
                                return;
                            }
                            let fd = libc::dup(raw);
                            if fd < 0 {
                                done(Err(io::Error::last_os_error().to_string()));
                            } else {
                                done(Ok(fd));
                            }
                        },
                    ),
                );
            }
        });
        match attempt {
            Ok(fd) => return Ok(fd),
            Err(err) if err.message == "timed out waiting for the virtual machine" => {
                return Err(VmError::new(VmErrorKind::Prerequisite, last));
            }
            Err(err) => {
                last = err.message;
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Err(VmError::new(VmErrorKind::Prerequisite, last))
}

fn wait_on_queue<T, F>(queue: &DispatchQueue, timeout: Duration, start: F) -> Result<T, VmError>
where
    T: Send + 'static,
    F: FnOnce(Box<dyn FnOnce(Result<T, String>) + Send>) + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    queue.exec_async(move || {
        let tx = tx;
        start(Box::new(move |result| {
            let _ = tx.send(result);
        }));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) => Err(VmError::new(VmErrorKind::Prerequisite, message)),
        Err(_) => Err(VmError::new(
            VmErrorKind::Prerequisite,
            "timed out waiting for the virtual machine",
        )),
    }
}

fn select_vm_cpu_count(host_count: Option<usize>, min: usize, max: usize) -> usize {
    host_count.unwrap_or(2).clamp(min, max)
}

/// One gibibyte is the framework floor and is not enough for the node workloads
/// plus the in-cluster registry. Four gibibytes keeps that guest from failing
/// atomic page allocation while the registry is running.
const VM_MEMORY_SIZE: u64 = 4 * 1024 * 1024 * 1024;

fn select_vm_memory_size(preferred: u64, min: u64, max: u64) -> u64 {
    preferred.clamp(min, max)
}

fn vm_config(
    guest: &GuestConfig,
    kernel: &Path,
    initrd: &Path,
    exports: &[Export],
) -> Result<(OwnedFd, Retained<VZVirtualMachineConfiguration>), VmError> {
    let config = unsafe { VZVirtualMachineConfiguration::new() };
    let kernel_url = file_url(kernel)?;
    let initrd_url = file_url(initrd)?;
    let loader =
        unsafe { VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel_url) };
    unsafe {
        loader.setInitialRamdiskURL(Some(&initrd_url));
        loader.setCommandLine(&NSString::from_str(guest.kernel_cmdline));
        config.setBootLoader(Some(&loader));
        config.setPlatform(&VZGenericPlatformConfiguration::new());
    }
    let cpu = {
        let min = unsafe { VZVirtualMachineConfiguration::minimumAllowedCPUCount() };
        let max = unsafe { VZVirtualMachineConfiguration::maximumAllowedCPUCount() };
        select_vm_cpu_count(
            thread::available_parallelism()
                .ok()
                .map(|count| count.get()),
            min,
            max,
        )
    };
    let memory = {
        let min = unsafe { VZVirtualMachineConfiguration::minimumAllowedMemorySize() };
        let max = unsafe { VZVirtualMachineConfiguration::maximumAllowedMemorySize() };
        select_vm_memory_size(VM_MEMORY_SIZE, min, max)
    };
    unsafe {
        config.setCPUCount(cpu);
        config.setMemorySize(memory);
    }

    let entropy = unsafe { VZVirtioEntropyDeviceConfiguration::new() };
    let entropy_ref: &VZEntropyDeviceConfiguration = &entropy;
    unsafe { config.setEntropyDevices(&NSArray::from_slice(&[entropy_ref])) };

    let nat = unsafe { VZNATNetworkDeviceAttachment::new() };
    let nic = unsafe { VZVirtioNetworkDeviceConfiguration::new() };
    unsafe { nic.setAttachment(Some(&nat)) };
    let nic_ref: &VZNetworkDeviceConfiguration = &nic;
    unsafe { config.setNetworkDevices(&NSArray::from_slice(&[nic_ref])) };

    let socket = unsafe { VZVirtioSocketDeviceConfiguration::new() };
    let socket_ref: &objc2_virtualization::VZSocketDeviceConfiguration = &socket;
    unsafe { config.setSocketDevices(&NSArray::from_slice(&[socket_ref])) };

    let mut devices = Vec::new();
    for export in exports {
        let host_url = file_url(&export.host)?;
        let shared = unsafe {
            objc2_virtualization::VZSharedDirectory::initWithURL_readOnly(
                objc2_virtualization::VZSharedDirectory::alloc(),
                &host_url,
                export.read_only,
            )
        };
        let share = unsafe {
            objc2_virtualization::VZSingleDirectoryShare::initWithDirectory(
                objc2_virtualization::VZSingleDirectoryShare::alloc(),
                &shared,
            )
        };
        let device = unsafe {
            VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str(&export.tag),
            )
        };
        unsafe { device.setShare(Some(&share)) };
        devices.push(device);
    }
    let directory_refs: Vec<&VZDirectorySharingDeviceConfiguration> =
        devices.iter().map(|device| device as _).collect();
    unsafe { config.setDirectorySharingDevices(&NSArray::from_slice(&directory_refs)) };

    let (console_read, console_write) = anon_pipe()?;
    let null = File::open("/dev/null").map_err(io_err)?;
    let stdin = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
        NSFileHandle::alloc(),
        null.into_raw_fd(),
        true,
    );
    let stdout = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
        NSFileHandle::alloc(),
        console_write.into_raw_fd(),
        true,
    );
    let attachment = unsafe {
        VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
            VZFileHandleSerialPortAttachment::alloc(),
            Some(&stdin),
            Some(&stdout),
        )
    };
    let port = unsafe { VZVirtioConsoleDeviceSerialPortConfiguration::new() };
    unsafe { port.setAttachment(Some(&attachment)) };
    let port_ref: &VZSerialPortConfiguration = &port;
    unsafe { config.setSerialPorts(&NSArray::from_slice(&[port_ref])) };

    unsafe { config.validateWithError() }
        .map_err(|err| VmError::new(VmErrorKind::Prerequisite, ns_error_text(&err)))?;
    Ok((console_read, config))
}

fn spawn_console(fd: OwnedFd, name: &str) -> JoinHandle<()> {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut file = File::from(fd);
            let mut buf = [0u8; 4096];
            loop {
                match file.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut err = io::stderr().lock();
                        let _ = err.write_all(&buf[..n]);
                        let _ = err.flush();
                    }
                }
            }
        })
        .expect("console thread")
}

fn anon_pipe() -> Result<(OwnedFd, OwnedFd), VmError> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io_err(io::Error::last_os_error()));
    }
    Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
        OwnedFd::from_raw_fd(fds[1])
    }))
}

fn prepare_fd(fd: c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        let fd_flags = libc::fcntl(fd, libc::F_GETFD);
        if fd_flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC);
        }
    }
}

fn file_url(path: &Path) -> Result<Retained<NSURL>, VmError> {
    let text = path.to_str().ok_or_else(|| {
        VmError::new(
            VmErrorKind::InvalidArgument,
            format!("path is not valid Unicode: {}", path.display()),
        )
    })?;
    Ok(NSURL::fileURLWithPath(&NSString::from_str(text)))
}

fn ns_result(err: *mut NSError) -> Result<(), String> {
    if err.is_null() {
        Ok(())
    } else {
        Err(ns_message(err))
    }
}

fn ns_message(err: *mut NSError) -> String {
    if err.is_null() {
        "virtual machine failed".to_string()
    } else {
        ns_error_text(unsafe { &*err })
    }
}

fn ns_error_text(err: &NSError) -> String {
    err.localizedDescription().to_string()
}

fn io_err(err: io::Error) -> VmError {
    VmError::new(VmErrorKind::Internal, format!("guest socket: {err}"))
}

#[cfg(test)]
mod tests {
    use super::{VM_MEMORY_SIZE, select_vm_cpu_count, select_vm_memory_size};

    #[test]
    fn vm_cpu_count_tracks_host_parallelism_with_framework_bounds() {
        assert_eq!(select_vm_cpu_count(Some(16), 1, 16), 16);
        assert_eq!(select_vm_cpu_count(Some(16), 1, 8), 8);
        assert_eq!(select_vm_cpu_count(None, 1, 8), 2);
    }

    #[test]
    fn vm_memory_size_prefers_four_gibibytes_within_framework_bounds() {
        let gib = 1024 * 1024 * 1024;
        assert_eq!(VM_MEMORY_SIZE, 4 * gib);
        assert_eq!(select_vm_memory_size(VM_MEMORY_SIZE, gib, 8 * gib), 4 * gib);
        assert_eq!(select_vm_memory_size(VM_MEMORY_SIZE, gib, 2 * gib), 2 * gib);
        assert_eq!(
            select_vm_memory_size(VM_MEMORY_SIZE, 8 * gib, 16 * gib),
            8 * gib
        );
    }
}
