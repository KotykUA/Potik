use std::{
    ffi::CStr,
    fs::File,
    io::{self, Read},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::Command,
};

pub(crate) fn anonymous_file(name: &CStr, flags: libc::c_uint) -> io::Result<File> {
    let fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn stop_with_parent(command: &mut Command) {
    let parent = std::process::id() as libc::pid_t;
    // Only async-signal-safe operations are used between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::EPIPE));
            }
            Ok(())
        });
    }
}

pub(crate) fn random_hex<const N: usize>() -> io::Result<String> {
    let mut bytes = [0u8; N];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
