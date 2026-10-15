//! Process-group helpers shared by pipe/pty and shell command execution.
//!
//! This module centralizes the OS-specific pieces that ensure a spawned
//! command can be cleaned up reliably:
//! - `set_process_group` is called in `pre_exec` so the child starts its own
//!   process group.
//! - `detach_from_tty` starts a new session so non-interactive children do not
//!   inherit the controlling TTY.
//! - `kill_process_group_by_pid` targets the whole group (children/grandchildren)
//! - `kill_process_group` targets a known process group ID directly
//!   instead of a single PID.
//! - `set_parent_death_signal` (Linux only) arranges for the child to receive a
//!   `SIGTERM` when the parent exits, and re-checks the parent PID to avoid
//!   races during fork/exec.
//!
//! On non-Unix platforms these helpers are no-ops.

use std::io;

use tokio::process::Child;

#[cfg(unix)]
pub struct ProcessGroupLifeline {
    reader: std::os::unix::net::UnixStream,
    writer: std::os::unix::net::UnixStream,
    descriptor_limit: libc::c_int,
}

#[cfg(unix)]
impl ProcessGroupLifeline {
    /// Configure a single-use command and retain this owner through its child lifetime.
    pub fn configure_command(command: &mut std::process::Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let lifeline = Self::new()?;
        let fds = lifeline.child_fds();
        unsafe {
            command.pre_exec(move || {
                detach_from_tty()?;
                guard_process_group_lifetime(fds)
            });
        }
        Ok(lifeline)
    }

    pub fn new() -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        let above_stdio = |stream: std::os::unix::net::UnixStream| {
            let fd = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if fd == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) })
            }
        };
        let reader = above_stdio(reader)?;
        let writer = above_stdio(writer)?;
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut descriptor_limit = limit.rlim_cur.min(libc::c_int::MAX as _) as libc::c_int;
        // Existing descriptors can exceed a subsequently lowered soft limit.
        // Collect this bound before fork; no allocation or directory I/O is
        // allowed in the guardian's post-fork branch.
        for entry in std::fs::read_dir("/dev/fd")? {
            if let Some(fd) = entry?
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::c_int>().ok())
            {
                descriptor_limit = descriptor_limit.max(fd.saturating_add(1));
            }
        }
        Ok(Self {
            reader,
            writer,
            descriptor_limit,
        })
    }

    pub fn child_fds(&self) -> (libc::c_int, libc::c_int, libc::c_int) {
        use std::os::fd::AsRawFd;
        (
            self.reader.as_raw_fd(),
            self.writer.as_raw_fd(),
            self.descriptor_limit,
        )
    }

    pub fn into_writer(self) -> std::os::unix::net::UnixStream {
        self.writer
    }
}

#[cfg(target_os = "macos")]
fn close_guardian_descriptors(ready_writer: libc::c_int) -> io::Result<()> {
    let mut descriptors = [libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    }; 1024];
    loop {
        let bytes = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                descriptors.as_mut_ptr().cast(),
                std::mem::size_of_val(&descriptors) as libc::c_int,
            )
        };
        if bytes <= 0
            || bytes as usize > std::mem::size_of_val(&descriptors)
            || !(bytes as usize).is_multiple_of(std::mem::size_of::<libc::proc_fdinfo>())
        {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let count = bytes as usize / std::mem::size_of::<libc::proc_fdinfo>();
        let mut closed_any = false;
        for descriptor in descriptors.iter().take(count) {
            let fd = descriptor.proc_fd;
            if fd == libc::STDIN_FILENO || fd == ready_writer {
                continue;
            }
            if fd < 0 {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            loop {
                if unsafe { libc::close(fd) } == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EBADF) => break,
                    _ => return Err(error),
                }
            }
            closed_any = true;
        }
        if !closed_any {
            return Ok(());
        }
    }
}

/// Install after setsid and before exec. The owner alone retains the writer.
/// Double-fork so a shell's `wait` cannot wait for its own lifetime guardian.
#[cfg(unix)]
pub fn guard_process_group_lifetime(
    (reader, writer, descriptor_limit): (libc::c_int, libc::c_int, libc::c_int),
) -> io::Result<()> {
    // All work after fork must remain async-signal-safe, including closing the
    // CLOEXEC error pipe that std::process::spawn is waiting to see close.
    unsafe {
        libc::close(writer);
        let mut ready = [-1; 2];
        if libc::pipe(ready.as_mut_ptr()) == -1 {
            return Err(io::Error::last_os_error());
        }
        let intermediate = libc::fork();
        if intermediate == -1 {
            libc::close(ready[0]);
            libc::close(ready[1]);
            return Err(io::Error::last_os_error());
        }
        if intermediate == 0 {
            let guardian = libc::fork();
            if guardian != 0 {
                libc::close(ready[1]);
                let mut byte = 0u8;
                let mut count;
                loop {
                    count = libc::read(ready[0], (&raw mut byte).cast(), 1);
                    if count != -1
                        || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
                libc::_exit(if guardian != -1 && count == 1 { 0 } else { 1 });
            }
            libc::close(ready[0]);
            for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                libc::signal(signal, libc::SIG_IGN);
            }
            if libc::dup2(reader, libc::STDIN_FILENO) == -1 {
                libc::killpg(0, libc::SIGKILL);
                libc::_exit(1);
            }
            #[cfg(target_os = "macos")]
            {
                let _ = descriptor_limit;
                if close_guardian_descriptors(ready[1]).is_err() {
                    libc::close(ready[1]);
                    libc::_exit(1);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                #[cfg(target_os = "linux")]
                let closed =
                    libc::syscall(libc::SYS_close_range, 1u32, (ready[1] - 1) as u32, 0u32) == 0
                        && libc::syscall(
                            libc::SYS_close_range,
                            (ready[1] + 1) as u32,
                            u32::MAX,
                            0u32,
                        ) == 0;
                #[cfg(not(target_os = "linux"))]
                let closed = false;
                if !closed {
                    for fd in 1..descriptor_limit {
                        if fd != ready[1] {
                            libc::close(fd);
                        }
                    }
                }
            }
            let mut byte = 0u8;
            if libc::write(ready[1], (&raw const byte).cast(), 1) != 1 {
                libc::killpg(0, libc::SIGKILL);
                libc::_exit(1);
            }
            libc::close(ready[1]);
            loop {
                let count = libc::read(libc::STDIN_FILENO, (&raw mut byte).cast(), 1);
                if count == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if count <= 0 {
                    libc::killpg(0, libc::SIGKILL);
                    libc::_exit(1);
                }
            }
        }
        libc::close(ready[0]);
        libc::close(ready[1]);
        let mut status = 0;
        loop {
            if libc::waitpid(intermediate, &raw mut status, 0) >= 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        libc::close(reader);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
/// Ensure the child receives SIGTERM when the original parent dies.
///
/// This should run in `pre_exec` and uses `parent_pid` captured before spawn to
/// avoid a race where the parent exits between fork and exec.
pub fn set_parent_death_signal(parent_pid: libc::pid_t) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } == -1 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { libc::getppid() } != parent_pid {
        unsafe {
            libc::raise(libc::SIGTERM);
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
/// No-op on non-Linux platforms.
pub fn set_parent_death_signal(_parent_pid: i32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
/// Detach from the controlling TTY by starting a new session.
pub fn detach_from_tty() -> io::Result<()> {
    let result = unsafe { libc::setsid() };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            return set_process_group();
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn detach_from_tty() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
/// Put the calling process into its own process group.
///
/// Intended for use in `pre_exec` so the child becomes the group leader.
pub fn set_process_group() -> io::Result<()> {
    let result = unsafe { libc::setpgid(0, 0) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn set_process_group() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
/// Kill the process group for the given PID (best-effort).
///
/// This resolves the PGID for `pid` and sends SIGKILL to the whole group.
pub fn kill_process_group_by_pid(pid: u32) -> io::Result<()> {
    use std::io::ErrorKind;

    let pid = pid as libc::pid_t;
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid == -1 {
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::NotFound && err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
        return Ok(());
    }

    let result = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::NotFound && err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
    }

    Ok(())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn kill_process_group_by_pid(_pid: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn signal_process_group_id(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<bool> {
    use std::io::ErrorKind;

    let result = unsafe { libc::killpg(pgid, signal) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.kind() == ErrorKind::NotFound || err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(err);
    }

    Ok(true)
}

#[cfg(target_os = "macos")]
fn signal_process_id(pid: libc::pid_t, signal: libc::c_int) -> io::Result<bool> {
    if unsafe { libc::kill(pid, signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error);
    }

    Ok(true)
}

#[cfg(target_os = "macos")]
fn signal_process_group_with_member_fallback(
    process_group_id: u32,
    signal: libc::c_int,
    signal_group: impl FnOnce(libc::pid_t, libc::c_int) -> io::Result<bool>,
    mut signal_member: impl FnMut(libc::pid_t, libc::c_int) -> io::Result<bool>,
) -> io::Result<bool> {
    let process_group_id = libc::pid_t::try_from(process_group_id)
        .ok()
        .filter(|process_group_id| *process_group_id > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process group ID"))?;

    match signal_group(process_group_id, signal) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        result => return result,
    }

    let mut process_ids: Vec<libc::pid_t> = vec![0; 16];
    loop {
        let buffer_size = libc::c_int::try_from(std::mem::size_of_val(process_ids.as_slice()))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "process group is too large")
            })?;
        let count = unsafe {
            libc::proc_listpgrppids(
                process_group_id,
                process_ids.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = count as usize;
        if count < process_ids.len() {
            process_ids.truncate(count);
            break;
        }
        let capacity = process_ids.len().checked_mul(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "process group is too large")
        })?;
        process_ids.resize(capacity, 0);
    }
    process_ids.sort_unstable_by_key(|process_id| *process_id == process_group_id);

    let mut signalled = false;
    let mut first_error = None;
    for process_id in process_ids {
        if process_id <= 0 {
            continue;
        }
        let current_group_id = unsafe { libc::getpgid(process_id) };
        if current_group_id == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) && first_error.is_none() {
                first_error = Some(error);
            }
            continue;
        }
        if current_group_id != process_group_id {
            continue;
        }
        match signal_member(process_id, signal) {
            Ok(delivered) => signalled |= delivered,
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }

    if signalled {
        Ok(true)
    } else {
        first_error.map_or(Ok(false), Err)
    }
}

#[cfg(unix)]
/// Send SIGTERM to a specific process group ID (best-effort).
///
/// Returns `Ok(true)` when SIGTERM was delivered to an existing group and
/// `Ok(false)` when the group no longer exists.
pub fn terminate_process_group(process_group_id: u32) -> io::Result<bool> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGTERM)
}

#[cfg(target_os = "macos")]
/// Retry a denied SIGTERM against the exact group's individual members.
pub fn terminate_process_group_with_member_fallback(process_group_id: u32) -> io::Result<bool> {
    signal_process_group_with_member_fallback(
        process_group_id,
        libc::SIGTERM,
        signal_process_group_id,
        signal_process_id,
    )
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn terminate_process_group(_process_group_id: u32) -> io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
/// Send SIGINT to a specific process group ID (best-effort).
pub fn interrupt_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGINT).map(|_| ())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn interrupt_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
/// Kill a specific process group ID (best-effort).
pub fn kill_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id as libc::pid_t, libc::SIGKILL).map(|_| ())
}

#[cfg(target_os = "macos")]
/// Retry a denied SIGKILL against the exact group's individual members.
pub fn kill_process_group_with_member_fallback(process_group_id: u32) -> io::Result<()> {
    signal_process_group_with_member_fallback(
        process_group_id,
        libc::SIGKILL,
        signal_process_group_id,
        signal_process_id,
    )
    .map(|_| ())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn kill_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
/// Kill the process group for a tokio child (best-effort).
pub fn kill_child_process_group(child: &mut Child) -> io::Result<()> {
    if let Some(pid) = child.id() {
        return kill_process_group_by_pid(pid);
    }

    Ok(())
}

#[cfg(not(unix))]
/// No-op on non-Unix platforms.
pub fn kill_child_process_group(_child: &mut Child) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
#[path = "process_group_tests.rs"]
mod tests;
