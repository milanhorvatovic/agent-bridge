//! Capturing the process's stdout for the wire, and getting every other
//! writer off it.
//!
//! The runtime speaks JSON-RPC on stdout, so one stray `println!` from
//! anywhere in the process would corrupt the wire for the client reading it.
//! Three layers guard against that; this is the middle one. At startup the
//! original stdout is duplicated — the duplicate is the *only* handle the
//! framer ever writes to — and descriptor 1 is then repointed at the log (or
//! discarded), so a language-level write to stdout from library code lands in
//! the log and never on the wire. The lint in `clippy.toml` is the first
//! layer and the framing fuzz the third; this one closes the gap between them
//! at runtime.
//!
//! This is the reason this crate is not `forbid(unsafe_code)`: duplicating and
//! redirecting the raw descriptor is below what the standard library exposes.
//! The unsafety is confined here, behind [`capture_stdout`], which hands back
//! a plain async writer.

use std::path::Path;

/// Where descriptor 1 is repointed once the wire has its own copy of stdout.
#[derive(Debug, Clone, Copy)]
pub enum StdoutRedirect<'a> {
    /// Send stray stdout to the platform's null sink.
    Discard,
    /// Send it to this file — the runtime log, so a stray write is captured
    /// rather than lost.
    ToFile(&'a Path),
}

/// Duplicate the current stdout for the wire and repoint descriptor 1 at
/// `redirect`, returning the async writer the framer owns.
///
/// After this returns, `println!` and every other language-level stdout write
/// reaches `redirect`, while the returned writer alone reaches the real
/// stdout the client reads. Call it once, early in startup, before any
/// subsystem that might write.
///
/// # Errors
///
/// If the descriptor cannot be duplicated, or the redirect target cannot be
/// opened or installed.
pub fn capture_stdout(redirect: StdoutRedirect) -> std::io::Result<tokio::fs::File> {
    let std_file = platform::capture(redirect)?;
    Ok(tokio::fs::File::from_std(std_file))
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsRawFd, FromRawFd};

    use super::StdoutRedirect;

    pub(super) fn capture(redirect: StdoutRedirect) -> std::io::Result<File> {
        // SAFETY: `dup` takes a descriptor and returns a fresh one referring
        // to the same open file description; it touches no memory. A negative
        // return is the documented error path, converted below.
        let wire_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if wire_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `dup` returned a valid, owned descriptor; wrapping it in a
        // `File` transfers that ownership so it is closed on drop.
        let wire = unsafe { File::from_raw_fd(wire_fd) };
        // `dup` produces a descriptor with FD_CLOEXEC clear, so without this a
        // hosted CLI spawned later would inherit a writable handle to the real
        // protocol stdout across `exec` — a second writer on the wire the whole
        // capture exists to prevent. Set close-on-exec so only this process
        // holds it.
        // SAFETY: `fcntl(F_SETFD, FD_CLOEXEC)` takes a valid descriptor and a
        // flag integer and touches no memory; the negative return is converted.
        if unsafe { libc::fcntl(wire.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let target = match redirect {
            StdoutRedirect::Discard => OpenOptions::new().write(true).open("/dev/null")?,
            StdoutRedirect::ToFile(path) => {
                OpenOptions::new().create(true).append(true).open(path)?
            }
        };
        // SAFETY: both arguments are valid open descriptors; `dup2` repoints
        // descriptor 1 at the redirect target's file description, closing
        // whatever it pointed at. It touches no memory.
        let rc = unsafe { libc::dup2(target.as_raw_fd(), libc::STDOUT_FILENO) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // `dup2` duplicated the target into descriptor 1, so the original
        // `target` handle is now redundant and closing it (on drop) leaves
        // descriptor 1 pointing at the same file.
        drop(target);
        Ok(wire)
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};

    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE, SetStdHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::StdoutRedirect;

    pub(super) fn capture(redirect: StdoutRedirect) -> std::io::Result<File> {
        // SAFETY: these calls take no memory this code owns. `GetStdHandle`
        // and `GetCurrentProcess` return pseudo/real handles; `DuplicateHandle`
        // writes the duplicate through the out-pointer we provide, and its
        // BOOL return is checked before the handle is used.
        let wire = unsafe {
            let original = GetStdHandle(STD_OUTPUT_HANDLE);
            let process = GetCurrentProcess();
            let mut duplicate: HANDLE = std::ptr::null_mut();
            let ok = DuplicateHandle(
                process,
                original,
                process,
                &raw mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            );
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            duplicate
        };

        let target = match redirect {
            StdoutRedirect::Discard => OpenOptions::new().write(true).open("NUL")?,
            StdoutRedirect::ToFile(path) => {
                OpenOptions::new().create(true).append(true).open(path)?
            }
        };
        // SAFETY: `SetStdHandle` stores the handle value as the process's
        // standard-output handle; it does not take ownership, so the file must
        // outlive the redirect — which is why it is leaked below.
        let installed =
            unsafe { SetStdHandle(STD_OUTPUT_HANDLE, target.as_raw_handle() as HANDLE) };
        if installed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Unlike `dup2`, `SetStdHandle` does not duplicate: descriptor-1's
        // handle now *is* this file's, so closing the file would dangle the
        // standard handle. Leak it for the process's lifetime — it is the log
        // or the null device, reclaimed by the OS at exit.
        std::mem::forget(target);

        // SAFETY: `DuplicateHandle` returned a valid, owned handle; wrapping it
        // in a `File` transfers ownership so it is closed on drop.
        Ok(unsafe { File::from_raw_handle(wire as RawHandle) })
    }
}
