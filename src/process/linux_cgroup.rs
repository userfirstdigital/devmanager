//! Linux managed-session custody. A short-lived guardian runs as the main
//! process of one delegated systemd user service. The provider joins that
//! cgroup before exec, stops for host attestation, then detaches from ptrace so
//! terminal job control and debuggers work normally. EOF from the exact host or
//! guardian death kills the whole service, including detached descendants.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::identity::{ManagedProcessId, ManagedProcessIdentity};
use crate::providers::capabilities::ProviderExecutable;
use serde::{Deserialize, Serialize};

const STARTUP: Duration = Duration::from_secs(5);
const CLEANUP: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(5);
const MAX_FRAME: usize = 512 * 1024;
const MAX_MEMBERS: usize = 4096;
const NO_STATUS: i32 = i32::MIN;
const PREFIX: &str = "devmanager-session-";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchWire {
    executable: Vec<u8>,
    arguments: Vec<Vec<u8>>,
    cwd: Vec<u8>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    root: u32,
    cgroup: String,
}

fn error(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}
fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Linux session deadline expired")
}
fn checkpoint(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(timeout())
    } else {
        Ok(())
    }
}
fn limits(stream: &UnixStream, deadline: Instant) -> io::Result<()> {
    checkpoint(deadline)?;
    let left = deadline.saturating_duration_since(Instant::now());
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))
}
fn send<T: Serialize>(stream: &mut UnixStream, value: &T, deadline: Instant) -> io::Result<()> {
    limits(stream, deadline)?;
    let bytes = serde_json::to_vec(value).map_err(error)?;
    if bytes.len() > MAX_FRAME {
        return Err(error("Linux session frame exceeds limit"));
    }
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    limits(stream, deadline)?;
    stream.write_all(&bytes)
}
fn receive<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
    deadline: Instant,
) -> io::Result<T> {
    limits(stream, deadline)?;
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(error("Linux session frame exceeds limit"));
    }
    let mut bytes = vec![0; length];
    limits(stream, deadline)?;
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(error)
}
fn credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut peer as *mut _ as *mut _,
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of_val(&peer)
        || peer.pid <= 0
        || peer.uid != unsafe { libc::geteuid() }
    {
        return Err(error("Linux session peer is not the same user"));
    }
    Ok(peer)
}
fn pidfd(pid: u32) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw as RawFd) })
}
fn exited(fd: &OwnedFd) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(poll.revents & libc::POLLIN != 0)
}
fn kill_exact(process: &OwnedFd) -> io::Result<()> {
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            process.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e);
        }
    }
    Ok(())
}
fn same_file(left: &File, right: &File) -> io::Result<bool> {
    let a = left.metadata()?;
    let b = right.metadata()?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}
fn send_files(stream: &UnixStream, files: [RawFd; 2]) -> io::Result<()> {
    let mut byte = [b'P'];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(&files) as u32) } as usize;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(&files) as u32) as usize;
        std::ptr::copy_nonoverlapping(files.as_ptr(), libc::CMSG_DATA(header).cast(), 2);
        if libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn receive_files(stream: &UnixStream) -> io::Result<[OwnedFd; 2]> {
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut files = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let size = (*header)
                    .cmsg_len
                    .saturating_sub(libc::CMSG_LEN(0) as usize);
                for i in 0..size / std::mem::size_of::<RawFd>() {
                    files.push(OwnedFd::from_raw_fd(
                        *libc::CMSG_DATA(header).cast::<RawFd>().add(i),
                    ));
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    if count != 1 || byte != [b'P'] || msg.msg_flags & libc::MSG_CTRUNC != 0 || files.len() != 2 {
        return Err(error(
            "Linux session requires exactly one PTY and one executable descriptor",
        ));
    }
    Ok(files.try_into().unwrap())
}
fn open_child(directory: &File, name: &std::ffi::CStr, flags: i32) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn members(directory: &File, guardian: u32, deadline: Instant) -> io::Result<Vec<u32>> {
    const MAX_GROUPS: usize = 256;
    checkpoint(deadline)?;
    let mut queue = vec![(
        open_child(directory, c".", libc::O_RDONLY | libc::O_DIRECTORY)?,
        true,
    )];
    let mut groups = 1;
    let mut pids = std::collections::BTreeSet::new();
    while let Some((group, is_root)) = queue.pop() {
        checkpoint(deadline)?;
        let mut text = String::new();
        let processes = match open_child(&group, c"cgroup.procs", libc::O_RDONLY) {
            Ok(file) => file,
            Err(e) if !is_root && matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENODEV)) => {
                continue
            }
            Err(e) => return Err(e),
        };
        processes
            .take((MAX_MEMBERS * 12 + 1) as u64)
            .read_to_string(&mut text)?;
        if text.len() > MAX_MEMBERS * 12 {
            return Err(error("Linux process group exceeds membership bound"));
        }
        for line in text.lines() {
            let pid = line.parse::<u32>().map_err(error)?;
            if pid == 0 {
                return Err(error("Linux process group returned zero PID"));
            }
            if pid != guardian {
                pids.insert(pid);
            }
            if pids.len() > MAX_MEMBERS {
                return Err(error("Linux process group exceeds membership bound"));
            }
        }
        // A fresh open file description avoids sharing directory offsets
        // between simultaneous read-only inventory requests.
        let enumeration = match open_child(&group, c".", libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(file) => file,
            Err(e) if !is_root && matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENODEV)) => {
                continue
            }
            Err(e) => return Err(e),
        };
        let raw = enumeration.into_raw_fd();
        let directory = unsafe { libc::fdopendir(raw) };
        if directory.is_null() {
            unsafe {
                libc::close(raw);
            }
            return Err(io::Error::last_os_error());
        }
        struct Directory(*mut libc::DIR);
        impl Drop for Directory {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let directory = Directory(directory);
        loop {
            checkpoint(deadline)?;
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(directory.0) };
            if entry.is_null() {
                let errno = unsafe { *libc::__errno_location() };
                if errno != 0 {
                    return Err(io::Error::from_raw_os_error(errno));
                }
                break;
            }
            let entry = unsafe { &*entry };
            if entry.d_type != libc::DT_DIR && entry.d_type != libc::DT_UNKNOWN {
                continue;
            }
            let name = unsafe { std::ffi::CStr::from_ptr(entry.d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            match open_child(&group, name, libc::O_RDONLY | libc::O_DIRECTORY) {
                Ok(child) => {
                    groups += 1;
                    if groups > MAX_GROUPS {
                        return Err(error("Linux session exceeds subgroup bound"));
                    }
                    queue.push((child, false));
                }
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    checkpoint(deadline)?;
    Ok(pids.into_iter().collect())
}

#[derive(Debug)]
struct Shared {
    directory: File,
    kill: File,
    guardian: u32,
    guardian_process: OwnedFd,
    wrapper_process: OwnedFd,
    root: ManagedProcessIdentity,
    root_status: AtomicI32,
    resumed: AtomicBool,
    joined: AtomicBool,
    stopping: AtomicBool,
    control: Mutex<UnixStream>,
}
impl Shared {
    fn terminate(&self) -> io::Result<()> {
        self.stopping.store(true, Ordering::Release);
        // The guardian stays outside the workload cgroup, reaps the root and
        // delivers its actual exit status after this atomic tree-wide kill.
        match (&self.kill).write_all(b"1") {
            Ok(()) => Ok(()),
            Err(_e) if self.joined.load(Ordering::Acquire) && exited(&self.guardian_process)? => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    fn active(&self, deadline: Instant) -> io::Result<Vec<u32>> {
        match members(&self.directory, self.guardian, deadline) {
            Ok(pids) => Ok(pids),
            Err(e)
                if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENODEV))
                    && exited(&self.guardian_process)? =>
            {
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }
}

#[derive(Debug)]
pub(crate) struct LinuxCgroup {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    unit: String,
}
#[derive(Debug)]
pub(crate) struct PendingLinuxChild {
    shared: Arc<Shared>,
    armed: bool,
    deadline: Instant,
}
#[derive(Debug, Clone)]
pub(crate) struct LinuxChild {
    shared: Arc<Shared>,
}

struct StartingService {
    child: Option<Child>,
    unit: String,
}
fn join_wrapper(child: &mut Child, deadline: Instant) {
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
impl Drop for StartingService {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            // Stop only this attempt's unique service, then join both wrappers.
            // No shell, global process scan or unbounded systemctl wait.
            if let Ok(mut stop) = Command::new("/usr/bin/systemctl")
                .args(["--user", "--no-block", "stop", &self.unit])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                join_wrapper(&mut stop, Instant::now() + CLEANUP);
            }
            join_wrapper(&mut child, Instant::now() + CLEANUP);
        }
    }
}

impl LinuxCgroup {
    pub(crate) fn spawn(
        guardian_executable: &Path,
        slave: OwnedFd,
        executable: &ProviderExecutable,
        arguments: &[OsString],
        cwd: &Path,
        environment: &BTreeMap<OsString, OsString>,
        deadline: Instant,
    ) -> io::Result<(PendingLinuxChild, Self)> {
        checkpoint(deadline)?;
        let executable_file = executable.open_linux_exec_file().map_err(error)?;
        let guardian_file = File::open(guardian_executable)?;
        let unit = format!("{PREFIX}{}", uuid::Uuid::now_v7().simple());
        let endpoint = SocketAddr::from_abstract_name(unit.as_bytes())?;
        let listener = UnixListener::bind_addr(&endpoint)?;
        listener.set_nonblocking(true)?;
        let parent = std::process::id();
        let creation =
            crate::services::platform_service::capture_process_creation_time_100ns(parent)
                .ok_or_else(|| error("Linux host creation identity unavailable"))?;
        let child = Command::new("/usr/bin/systemd-run")
            .args([
                "--user",
                "--quiet",
                "--wait",
                "--collect",
                "--service-type=exec",
                "--unit",
                &unit,
                "--property=Delegate=yes",
                "--property=KillMode=control-group",
                "--property=KillSignal=SIGKILL",
                "--property=TimeoutStartSec=5s",
                "--property=TimeoutStopSec=5s",
            ])
            .arg(guardian_executable)
            .arg("--linux-session-guardian")
            .arg(&unit)
            .arg(parent.to_string())
            .arg(creation.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut starting = StartingService {
            child: Some(child),
            unit: format!("{unit}.service"),
        };
        let wrapper_process = pidfd(starting.child.as_ref().unwrap().id())?;
        let (mut control, guardian, guardian_process) = loop {
            checkpoint(deadline)?;
            if starting.child.as_mut().unwrap().try_wait()?.is_some() {
                return Err(error("Linux managed sessions require a working systemd user manager with cgroup v2 delegation"));
            }
            match listener.accept() {
                Ok((control, _)) => {
                    let peer = credentials(&control)?;
                    let process = pidfd(peer.pid as u32)?;
                    let observed = File::open(format!("/proc/{}/exe", peer.pid))?;
                    if !same_file(&guardian_file, &observed)? || exited(&process)? {
                        continue;
                    }
                    break (control, peer.pid as u32, process);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => return Err(e),
            }
        };
        limits(&control, deadline)?;
        send_files(&control, [slave.as_raw_fd(), executable_file.as_raw_fd()])?;
        send(
            &mut control,
            &LaunchWire {
                executable: executable.canonical_path().as_os_str().as_bytes().to_vec(),
                arguments: arguments.iter().map(|s| s.as_bytes().to_vec()).collect(),
                cwd: cwd.as_os_str().as_bytes().to_vec(),
                environment: environment
                    .iter()
                    .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                    .collect(),
            },
            deadline,
        )?;
        let ready: Ready = receive(&mut control, deadline)?;
        let expected_suffix = format!("/{unit}.service");
        let reported = std::fs::read_to_string(format!("/proc/{guardian}/cgroup"))?;
        let current = reported
            .lines()
            .find_map(|s| s.strip_prefix("0::"))
            .ok_or_else(|| error("cgroup v2 is required"))?;
        if !current.ends_with(&expected_suffix)
            || ready.cgroup != format!("/sys/fs/cgroup{current}/workload")
        {
            return Err(error("Linux guardian is not in its exact owned service"));
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&ready.cgroup)?;
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(directory.as_raw_fd(), &mut fs) } != 0
            || fs.f_type != libc::CGROUP2_SUPER_MAGIC
        {
            return Err(error("Linux session group is not cgroup v2"));
        }
        let kill = open_child(&directory, c"cgroup.kill", libc::O_WRONLY)?;
        let root_process = pidfd(ready.root)?;
        if !members(&directory, guardian, deadline)?.contains(&ready.root) {
            return Err(error("Linux provider is outside its owned cgroup"));
        }
        let actual = File::open(format!("/proc/{}/exe", ready.root))?;
        if !same_file(&actual, &executable_file)? || exited(&root_process)? {
            return Err(error(
                "Linux exec barrier image does not match the attested native file",
            ));
        }
        executable.validate_bound_identity().map_err(error)?;
        let creation =
            crate::services::platform_service::capture_process_creation_time_100ns(ready.root)
                .ok_or_else(|| error("Linux root identity unavailable"))?;
        let root = ManagedProcessIdentity::new(
            ManagedProcessId::new(ready.root, creation).map_err(error)?,
            executable.canonical_path().to_path_buf(),
        )
        .map_err(error)?;
        let mut reader = control.try_clone()?;
        reader.set_read_timeout(None)?;
        let shared = Arc::new(Shared {
            directory,
            kill,
            guardian,
            guardian_process,
            wrapper_process,
            root,
            root_status: AtomicI32::new(NO_STATUS),
            resumed: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            control: Mutex::new(control),
        });
        let mut owner = Self {
            shared: shared.clone(),
            worker: None,
            unit,
        };
        let worker_state = shared.clone();
        // Arm ownership before transferring the wrapper into the reader actor.
        let mut wrapper_owner = starting;
        owner.worker = Some(
            std::thread::Builder::new()
                .name("linux-session-status".into())
                .spawn(move || {
                    let mut acknowledgement = [0];
                    if reader.read_exact(&mut acknowledgement).is_ok() && acknowledgement == [b'R']
                    {
                        worker_state.resumed.store(true, Ordering::Release);
                        let mut status = [0; 4];
                        if reader.read_exact(&mut status).is_ok() {
                            worker_state
                                .root_status
                                .store(i32::from_le_bytes(status), Ordering::Release);
                        }
                        let mut drain = [0; 1];
                        let _ = reader.read(&mut drain);
                    }
                    join_wrapper(
                        wrapper_owner.child.as_mut().unwrap(),
                        Instant::now() + CLEANUP,
                    );
                    wrapper_owner.child.take();
                    worker_state.joined.store(true, Ordering::Release);
                })?,
        );
        Ok((
            PendingLinuxChild {
                shared,
                armed: true,
                deadline,
            },
            owner,
        ))
    }
    pub(crate) fn root(&self) -> &ManagedProcessIdentity {
        &self.shared.root
    }
    pub(crate) fn internal_name(&self) -> &str {
        &self.unit
    }
    pub(crate) fn active_process_ids(&self, deadline: Instant) -> io::Result<Vec<u32>> {
        self.shared.active(deadline)
    }
    pub(crate) fn inspect_member(
        &self,
        pid: u32,
        deadline: Instant,
    ) -> io::Result<ManagedProcessIdentity> {
        checkpoint(deadline)?;
        let process = pidfd(pid)?;
        if !self.shared.active(deadline)?.contains(&pid) || exited(&process)? {
            return Err(error("process is not a live member of this Linux session"));
        }
        let creation = crate::services::platform_service::capture_process_creation_time_100ns(pid)
            .ok_or_else(|| error("Linux member creation identity unavailable"))?;
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))?;
        let identity = ManagedProcessIdentity::new(
            ManagedProcessId::new(pid, creation).map_err(error)?,
            executable,
        )
        .map_err(error)?;
        if exited(&process)?
            || crate::services::platform_service::capture_process_creation_time_100ns(pid)
                != Some(creation)
            || !self.shared.active(deadline)?.contains(&pid)
        {
            return Err(error("Linux member changed during exact inspection"));
        }
        checkpoint(deadline)?;
        Ok(identity)
    }
    pub(crate) fn terminate(&self) -> io::Result<()> {
        self.shared.terminate()
    }
    pub(crate) fn resumed(&self) -> bool {
        self.shared.resumed.load(Ordering::Acquire)
    }
    pub(crate) fn settled(&self) -> bool {
        self.shared.joined.load(Ordering::Acquire)
    }
    pub(crate) fn join(&mut self, deadline: Instant) -> io::Result<()> {
        while !self.settled() {
            checkpoint(deadline)?;
            std::thread::sleep(POLL);
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| error("Linux session status actor panicked"))?;
        }
        if !self.shared.active(deadline)?.is_empty() {
            return Err(error("Linux session still owns live members"));
        }
        Ok(())
    }
}
impl Drop for LinuxCgroup {
    fn drop(&mut self) {
        let _ = self.terminate();
        // Final owner loss also stops a paused guardian and its exact wrapper.
        // Wake the status reader ourselves; never depend on that guardian
        // running, or on a responsive user manager, to join our actor.
        let _ = kill_exact(&self.shared.guardian_process);
        let _ = kill_exact(&self.shared.wrapper_process);
        if let Ok(control) = self.shared.control.lock() {
            let _ = control.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl PendingLinuxChild {
    pub(crate) fn resume(mut self) -> io::Result<LinuxChild> {
        {
            let mut stream = self
                .shared
                .control
                .lock()
                .map_err(|_| error("Linux control poisoned"))?;
            checkpoint(self.deadline)?;
            stream.set_write_timeout(Some(
                self.deadline.saturating_duration_since(Instant::now()),
            ))?;
            stream.write_all(b"R")?;
        }
        while !self.shared.resumed.load(Ordering::Acquire) {
            checkpoint(self.deadline)?;
            if self.shared.joined.load(Ordering::Acquire) {
                return Err(error("Linux guardian did not acknowledge process resume"));
            }
            std::thread::sleep(POLL);
        }
        self.armed = false;
        Ok(LinuxChild {
            shared: self.shared.clone(),
        })
    }
    pub(crate) fn abort_and_wait(mut self) -> io::Result<()> {
        self.shared
            .control
            .lock()
            .map_err(|_| error("Linux control poisoned"))?
            .shutdown(Shutdown::Write)?;
        self.shared.terminate()?;
        self.armed = false;
        let deadline = Instant::now() + CLEANUP;
        while !self.shared.joined.load(Ordering::Acquire) {
            checkpoint(deadline)?;
            std::thread::sleep(POLL);
        }
        Ok(())
    }
}
impl Drop for PendingLinuxChild {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(control) = self.shared.control.lock() {
                let _ = control.shutdown(Shutdown::Write);
            }
            let _ = self.shared.terminate();
        }
    }
}
impl portable_pty::ChildKiller for LinuxChild {
    fn kill(&mut self) -> io::Result<()> {
        self.shared.terminate()
    }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}
impl portable_pty::Child for LinuxChild {
    fn process_id(&self) -> Option<u32> {
        Some(self.shared.root.id().pid())
    }
    fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
        let status = self.shared.root_status.load(Ordering::Acquire);
        if status != NO_STATUS {
            return Ok(Some(std::process::ExitStatus::from_raw(status).into()));
        }
        if self.shared.joined.load(Ordering::Acquire) {
            return Err(error("Linux guardian exited without root status"));
        }
        Ok(None)
    }
    fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Dispatch only the internal guardian mode, before any profile or host boot.
#[doc(hidden)]
pub fn run_guardian_subcommand(args: &[String]) -> Option<std::process::ExitCode> {
    if args.first().is_none_or(|s| s != "--linux-session-guardian") {
        return None;
    }
    Some(match guardian(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::from(2),
    })
}
fn guardian(args: &[String]) -> io::Result<()> {
    if args.len() != 4
        || !args[1].starts_with(PREFIX)
        || args[1].len() != PREFIX.len() + 32
        || !args[1][PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(error("invalid Linux guardian invocation"));
    }
    let parent: u32 = args[2].parse().map_err(error)?;
    let creation: u64 = args[3].parse().map_err(error)?;
    let parent_process = pidfd(parent)?;
    if crate::services::platform_service::capture_process_creation_time_100ns(parent)
        != Some(creation)
    {
        return Err(error("Linux guardian parent identity changed"));
    }
    let deadline = Instant::now() + STARTUP;
    let endpoint = SocketAddr::from_abstract_name(args[1].as_bytes())?;
    let mut control = UnixStream::connect_addr(&endpoint)?;
    if credentials(&control)?.pid as u32 != parent || exited(&parent_process)? {
        return Err(error("Linux guardian control peer changed"));
    }
    limits(&control, deadline)?;
    let [slave, executable] = receive_files(&control)?;
    if unsafe { libc::isatty(slave.as_raw_fd()) } != 1 {
        return Err(error("Linux session input is not a native PTY"));
    }
    let wire: LaunchWire = receive(&mut control, deadline)?;
    if wire.arguments.len() > 256 || wire.environment.len() > 256 {
        return Err(error("Linux launch field bound exceeded"));
    }
    let mut argv = vec![CString::new(wire.executable.clone()).map_err(error)?];
    for argument in &wire.arguments {
        argv.push(CString::new(argument.as_slice()).map_err(error)?);
    }
    let mut env = Vec::new();
    for (key, value) in &wire.environment {
        if key.is_empty() || key.contains(&b'=') {
            return Err(error("invalid Linux environment name"));
        }
        let mut pair = key.clone();
        pair.push(b'=');
        pair.extend(value);
        env.push(CString::new(pair).map_err(error)?);
    }
    let argv_ptrs: Vec<usize> = argv
        .iter()
        .map(|s| s.as_ptr() as usize)
        .chain([0])
        .collect();
    let env_ptrs: Vec<usize> = env.iter().map(|s| s.as_ptr() as usize).chain([0]).collect();
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    let relative = own
        .lines()
        .find_map(|s| s.strip_prefix("0::"))
        .ok_or_else(|| error("Linux managed sessions require cgroup v2"))?;
    if !relative.ends_with(&format!("/{}.service", args[1])) {
        return Err(error("Linux guardian is outside its service"));
    }
    let group_path = format!("/sys/fs/cgroup{relative}/workload");
    std::fs::create_dir(&group_path)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&group_path)?;
    struct KillOnDrop(File);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.write_all(b"1");
        }
    }
    let kill = KillOnDrop(open_child(&directory, c"cgroup.kill", libc::O_WRONLY)?);
    let membership = open_child(&directory, c"cgroup.procs", libc::O_WRONLY)?;
    let mut command = Command::new(OsString::from_vec(wire.executable));
    command
        .current_dir(PathBuf::from(OsString::from_vec(wire.cwd)))
        .env_clear();
    let slave: File = slave.into();
    command
        .stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave);
    unsafe {
        command.pre_exec(move || {
            // Writing zero moves this exact child into its workload before
            // exec. Fork, setsid and debugger operations retain cgroup custody.
            if libc::write(membership.as_raw_fd(), c"0".as_ptr().cast(), 1) != 1 {
                return Err(io::Error::last_os_error());
            }
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Execute the selected native descriptor. Replacing its pathname
            // after host attestation cannot substitute another program.
            libc::fexecve(
                executable.as_raw_fd(),
                argv_ptrs.as_ptr().cast(),
                env_ptrs.as_ptr().cast(),
            );
            Err(io::Error::last_os_error())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id() as i32;
    loop {
        checkpoint(deadline)?;
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            if !libc::WIFSTOPPED(status) || libc::WSTOPSIG(status) != libc::SIGTRAP {
                return Err(error("Linux provider did not reach exec barrier"));
            }
            break;
        }
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        std::thread::sleep(POLL);
    }
    send(
        &mut control,
        &Ready {
            root: pid as u32,
            cgroup: group_path,
        },
        deadline,
    )?;
    let mut resume = [0];
    limits(&control, deadline)?;
    control.read_exact(&mut resume)?;
    if resume != [b'R'] {
        return Err(error("invalid Linux session resume"));
    }
    if unsafe { libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    control.write_all(b"R")?;
    control.set_nonblocking(true)?;
    let root_process = pidfd(pid as u32)?;
    let mut events = open_child(&directory, c"cgroup.events", libc::O_RDONLY)?;
    let mut root_exited = false;
    loop {
        // Clear the cgroup change notification before testing membership. The root
        // pidfd handles ordinary exit; populated changes handle detached
        // descendants after root exit. Read before the predicate so a later exit cannot lose its wakeup.
        events.seek(SeekFrom::Start(0))?;
        let mut event_bytes = [0u8; 256];
        let event_count = events.read(&mut event_bytes)?;
        let populated = std::str::from_utf8(&event_bytes[..event_count])
            .map_err(error)?
            .lines()
            .find_map(|line| line.strip_prefix("populated "))
            .ok_or_else(|| error("Linux cgroup populated fact is absent"))?;
        if !matches!(populated, "0" | "1") {
            return Err(error("invalid Linux cgroup populated fact"));
        }
        match control.read(&mut resume) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(error("Linux session resume is one-way")),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        if !root_exited {
            if let Some(status) = child.try_wait()? {
                control.write_all(&status.into_raw().to_le_bytes())?;
                root_exited = true;
            }
        }
        if root_exited && populated == "0" {
            drop(kill);
            return Ok(());
        }
        let mut waiters = [
            libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if root_exited {
                    -1
                } else {
                    root_process.as_raw_fd()
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: events.as_raw_fd(),
                events: libc::POLLPRI,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(waiters.as_mut_ptr(), waiters.len() as _, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{Child as _, PtySize};

    fn helper() -> PathBuf {
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("devmanager-process-test-helper")
    }
    fn read_until(fd: RawFd, needle: &[u8]) {
        let deadline = Instant::now() + STARTUP;
        let mut output = Vec::new();
        while !output.windows(needle.len()).any(|s| s == needle) {
            checkpoint(deadline).unwrap_or_else(|_| {
                panic!(
                    "PTY did not produce {:?}: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&output)
                )
            });
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut poll, 1, 100) };
            assert!(n >= 0);
            if n > 0 {
                let mut bytes = [0u8; 8192];
                let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
                assert!(count > 0, "PTY ended before expected output");
                output.extend_from_slice(&bytes[..count as usize]);
                assert!(output.len() < 1024 * 1024);
            }
        }
    }
    fn marker(path: &Path) {
        let deadline = Instant::now() + STARTUP;
        while !path.exists() {
            checkpoint(deadline).unwrap();
            std::thread::sleep(POLL);
        }
    }

    // This is an OS integration gate, run explicitly with --ignored in a
    // logged-in systemd user session. Unit-only CI has no user service manager.
    #[test]
    #[ignore = "requires a systemd user manager with cgroup v2 delegation"]
    fn linux_cgroup_native_pty_exec_job_control_and_exact_cleanup() {
        for cleanup in [
            "terminate",
            "guardian-crash",
            "control-eof",
            "paused-owner-drop",
        ] {
            let root = tempfile::tempdir().unwrap();
            let pair = portable_pty::native_pty_system()
                .openpty(PtySize::default())
                .unwrap();
            let master = pair.master.as_raw_fd().unwrap();
            let mut writer = pair.master.take_writer().unwrap();
            let executable = ProviderExecutable::from_path("/usr/bin/bash").unwrap();
            let env = BTreeMap::from([
                (OsString::from("PATH"), OsString::from("/usr/bin")),
                (OsString::from("HOME"), root.path().as_os_str().to_owned()),
                (OsString::from("TERM"), OsString::from("xterm-256color")),
                (OsString::from("PS1"), OsString::from("OWNED> ")),
            ]);
            let (pending, mut job) = LinuxCgroup::spawn(
                &helper(),
                pair.slave.try_clone_owned_fd().unwrap(),
                &executable,
                &["--noprofile".into(), "--norc".into(), "-i".into()],
                root.path(),
                &env,
                Instant::now() + STARTUP,
            )
            .unwrap();
            drop(pair.slave);
            let provider_pid = job.root().id().pid();
            let root_handle = pidfd(provider_pid).unwrap();
            writer.write_all(b"printf ready > started\n").unwrap();
            std::thread::sleep(Duration::from_millis(30));
            assert!(
                !root.path().join("started").exists(),
                "provider ran before host release"
            );
            let mut child = pending.resume().unwrap();
            read_until(master, b"OWNED> ");
            marker(&root.path().join("started"));
            assert_eq!(
                std::fs::read_to_string(root.path().join("started")).unwrap(),
                "ready"
            );
            let status = std::fs::read_to_string(format!("/proc/{provider_pid}/status")).unwrap();
            assert!(
                status.lines().any(|s| s
                    .strip_prefix("TracerPid:")
                    .is_some_and(|v| v.trim() == "0")),
                "provider must be available to debuggers"
            );
            if cleanup == "terminate" {
                let switches = || {
                    std::fs::read_to_string(format!("/proc/{}/status", job.shared.guardian))
                        .unwrap()
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("voluntary_ctxt_switches:")
                                .map(|value| value.trim().parse::<u64>().unwrap())
                        })
                        .unwrap()
                };
                let before = switches();
                std::thread::sleep(Duration::from_millis(300));
                assert!(
                    switches().saturating_sub(before) <= 3,
                    "idle guardian has a hot polling loop"
                );
            }
            writer.write_all(b"/usr/bin/sleep 30\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
            writer.write_all(b"\x1a").unwrap();
            read_until(master, b"Stopped");
            writer.write_all(b"kill -CONT %1; kill %1; setsid /usr/bin/bash -c 'echo $$ > detached; exec /usr/bin/sleep 30' < /dev/null > /dev/null 2>&1 &\n").unwrap();
            marker(&root.path().join("detached"));
            let detached: u32 = std::fs::read_to_string(root.path().join("detached"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let descendant_handle = pidfd(detached).unwrap();
            assert!(job
                .active_process_ids(Instant::now() + STARTUP)
                .unwrap()
                .contains(&detached));
            if cleanup == "paused-owner-drop" {
                assert_eq!(
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            job.shared.guardian_process.as_raw_fd(),
                            libc::SIGSTOP,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        )
                    },
                    0
                );
                let started = Instant::now();
                drop(job);
                assert!(
                    started.elapsed() < CLEANUP,
                    "paused guardian blocked owner teardown"
                );
                let deadline = Instant::now() + CLEANUP;
                while !exited(&root_handle).unwrap() || !exited(&descendant_handle).unwrap() {
                    checkpoint(deadline).unwrap();
                    std::thread::sleep(POLL);
                }
                continue;
            }
            if cleanup == "guardian-crash" {
                assert_eq!(
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            job.shared.guardian_process.as_raw_fd(),
                            libc::SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        )
                    },
                    0
                );
            } else if cleanup == "control-eof" {
                job.shared
                    .control
                    .lock()
                    .unwrap()
                    .shutdown(Shutdown::Both)
                    .unwrap();
            } else {
                job.terminate().unwrap();
            }
            job.join(Instant::now() + CLEANUP).unwrap();
            assert!(exited(&root_handle).unwrap());
            assert!(exited(&descendant_handle).unwrap());
            assert!(job
                .active_process_ids(Instant::now() + STARTUP)
                .unwrap()
                .is_empty());
            if cleanup == "terminate" {
                assert_eq!(child.try_wait().unwrap().unwrap().signal(), Some("Killed"));
            }
        }
    }

    #[test]
    #[ignore = "requires a systemd user manager with cgroup v2 delegation"]
    fn linux_cgroup_abandoned_pending_launch_never_executes_provider_instructions() {
        let root = tempfile::tempdir().unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let executable = ProviderExecutable::from_path("/usr/bin/bash").unwrap();
        let (pending, mut job) = LinuxCgroup::spawn(
            &helper(),
            pair.slave.try_clone_owned_fd().unwrap(),
            &executable,
            &["-c".into(), "printf bad > started".into()],
            root.path(),
            &BTreeMap::new(),
            Instant::now() + STARTUP,
        )
        .unwrap();
        let process = pidfd(job.root().id().pid()).unwrap();
        drop(pending);
        job.join(Instant::now() + CLEANUP).unwrap();
        assert!(exited(&process).unwrap());
        assert!(!root.path().join("started").exists());
    }
    #[test]
    #[ignore = "requires a systemd user manager with cgroup v2 delegation"]
    fn linux_cgroup_waits_for_descendants_after_a_successful_root_exit() {
        let root = tempfile::tempdir().unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let executable = ProviderExecutable::from_path("/usr/bin/bash").unwrap();
        let (pending, mut job) = LinuxCgroup::spawn(
            &helper(),
            pair.slave.try_clone_owned_fd().unwrap(),
            &executable,
            &[
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                "/usr/bin/setsid /usr/bin/bash -c 'printf ready > detached; while test ! -e release; do /usr/bin/sleep .01; done' < /dev/null & while test ! -e detached; do /usr/bin/sleep .01; done; exit 0".into(),
            ],
            root.path(),
            &BTreeMap::new(),
            Instant::now() + STARTUP,
        )
        .unwrap();
        let mut child = pending.resume().unwrap();
        let deadline = Instant::now() + STARTUP;
        while child.try_wait().unwrap().is_none() {
            checkpoint(deadline).unwrap();
            std::thread::sleep(POLL);
        }
        assert_eq!(child.try_wait().unwrap().unwrap().exit_code(), 0);
        assert!(
            !job.active_process_ids(Instant::now() + STARTUP)
                .unwrap()
                .is_empty(),
            "root exit released a still-running descendant"
        );
        std::fs::write(root.path().join("release"), b"release").unwrap();
        job.join(Instant::now() + CLEANUP).unwrap();
        assert!(job
            .active_process_ids(Instant::now() + STARTUP)
            .unwrap()
            .is_empty());
    }
    #[test]
    #[ignore = "requires a systemd user manager with cgroup v2 delegation"]
    fn linux_cgroup_expired_resume_cannot_run_provider_instructions() {
        let root = tempfile::tempdir().unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let executable = ProviderExecutable::from_path("/usr/bin/bash").unwrap();
        let (mut pending, mut job) = LinuxCgroup::spawn(
            &helper(),
            pair.slave.try_clone_owned_fd().unwrap(),
            &executable,
            &["-c".into(), "printf bad > started".into()],
            root.path(),
            &BTreeMap::new(),
            Instant::now() + STARTUP,
        )
        .unwrap();
        let process = pidfd(job.root().id().pid()).unwrap();
        pending.deadline = Instant::now();
        assert!(pending.resume().is_err());
        job.join(Instant::now() + CLEANUP).unwrap();
        assert!(exited(&process).unwrap());
        assert!(!root.path().join("started").exists());
    }
    #[test]
    #[ignore = "requires a systemd user manager with cgroup v2 delegation"]
    fn linux_cgroup_registry_keeps_nested_members_and_exact_zero_authority() {
        use crate::domain::id::ResourceId;
        use crate::domain::operation::ResourceFence;
        use crate::process::identity::ProcessOwner;
        use crate::process::job::ManagedProcessJob;
        use crate::process::registry::{
            ManagedProcessState, ProcessDisplayLabel, ProcessRegistry, RegisteredProcess,
        };
        let root = tempfile::tempdir().unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let executable = ProviderExecutable::from_path("/usr/bin/bash").unwrap();
        let (pending, session) = LinuxCgroup::spawn(
            &helper(),
            pair.slave.try_clone_owned_fd().unwrap(),
            &executable,
            &[
                "-c".into(),
                "printf started > started; while :; do /usr/bin/sleep 30; done".into(),
            ],
            root.path(),
            &BTreeMap::new(),
            Instant::now() + STARTUP,
        )
        .unwrap();
        let identity = session.root().clone();
        let process = pidfd(identity.id().pid()).unwrap();
        // Move only this test-owned, still-gated root into a delegated child
        // group. cgroup.procs on the parent alone cannot observe it.
        let group = std::fs::read_link(format!(
            "/proc/self/fd/{}",
            session.shared.directory.as_raw_fd()
        ))
        .unwrap();
        let nested = group.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("cgroup.procs"), identity.id().pid().to_string()).unwrap();
        let resource = ResourceId::new();
        let mut registry = ProcessRegistry::new();
        let fence = registry
            .register(RegisteredProcess::new(
                ResourceFence::new(resource, 1),
                ProcessOwner::Host,
                identity.clone(),
                ProcessDisplayLabel::new("nested native session").unwrap(),
                ManagedProcessJob::from_linux_session(session),
            ))
            .unwrap();
        assert_eq!(
            registry
                .drain_job_completions_until(resource, Instant::now() + STARTUP)
                .unwrap(),
            0
        );
        assert_eq!(
            registry.current(resource).unwrap().state(),
            ManagedProcessState::Starting
        );
        assert!(!root.path().join("started").exists());
        let mut child = pending.resume().unwrap();
        registry.commit_resumed_exact(&fence).unwrap();
        marker(&root.path().join("started"));
        let job = registry.current(resource).unwrap().job();
        assert_eq!(
            job.inspect_process(identity.id().pid()).unwrap().identity(),
            &identity
        );
        assert!(
            job.inspect_process(std::process::id()).is_err(),
            "foreign harness must not gain session ownership"
        );
        assert!(registry.active_process_zero_proof_exact(&fence).is_err());
        assert!(registry.begin_stopping_exact(&fence));
        registry
            .current(resource)
            .unwrap()
            .job()
            .terminate_tree()
            .unwrap();
        let deadline = Instant::now() + CLEANUP;
        let proof = loop {
            checkpoint(deadline).unwrap();
            registry
                .drain_job_completions_until(resource, deadline)
                .unwrap();
            if let Ok(proof) = registry.active_process_zero_proof_exact(&fence) {
                break proof;
            }
            std::thread::sleep(POLL);
        };
        assert!(registry.settle_active_process_zero_exact(proof).unwrap());
        assert_eq!(
            registry.current(resource).unwrap().state(),
            ManagedProcessState::ZeroSettled
        );
        assert!(exited(&process).unwrap());
        assert!(child.try_wait().unwrap().is_some());
    }
}
