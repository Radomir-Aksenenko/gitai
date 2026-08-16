//! Killing a whole process tree.
//!
//! Gate commands run through a shell, and a shell that gets killed does not
//! take its children with it on either platform. Left alone, a `cargo test`
//! that hangs outlives the attempt that started it, holds the workspace open,
//! and on Windows keeps the directory undeletable.
//!
//! The Docker backend does not need any of this, since removing a container
//! removes everything inside it. This is what makes the `local` backend
//! trustworthy enough to develop against.
//!
//! - **Unix**: the child is put in its own process group and the group is
//!   signalled, so every descendant that has not called `setsid` is included.
//! - **Windows**: the child is assigned to a Job Object with
//!   `KILL_ON_JOB_CLOSE`. Closing the handle kills the tree, and because the
//!   kernel owns that guarantee it holds even if gitai itself dies.

use tokio::process::Command;

/// Platform state that has to outlive the spawn call. On Windows it owns the
/// Job Object handle; dropping it kills anything still inside.
pub struct TreeGuard {
    #[cfg(windows)]
    job: Option<windows::Job>,
    #[cfg(not(windows))]
    _private: (),
}

impl TreeGuard {
    /// Prepares `cmd` so its descendants can be reached later.
    pub fn arm(cmd: &mut Command) -> Self {
        #[cfg(unix)]
        {
            // 0 means "a new group whose id is this child's pid".
            cmd.process_group(0);
            Self { _private: () }
        }

        #[cfg(windows)]
        {
            let _ = cmd;
            Self {
                job: windows::Job::create()
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, "could not create a job object; \
                                                    timed-out commands may leave children behind");
                    })
                    .ok(),
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = cmd;
            Self { _private: () }
        }
    }

    /// Called right after spawn, with the child's process handle.
    ///
    /// There is a window between the process starting and this running in
    /// which a grandchild could escape. It is small, and closing it entirely
    /// would mean spawning suspended, which tokio does not expose.
    pub fn adopt(&self, child: &tokio::process::Child) {
        #[cfg(windows)]
        {
            if let (Some(job), Some(handle)) = (self.job.as_ref(), child.raw_handle())
                && let Err(e) = job.assign(handle)
            {
                tracing::warn!(error = %e, "could not assign the child to its job object");
            }
        }

        #[cfg(not(windows))]
        {
            let _ = child;
        }
    }

    /// Kills the child and everything under it.
    pub fn kill_tree(&self, child: &mut tokio::process::Child) {
        #[cfg(unix)]
        {
            if let Some(pid) = child.id() {
                // A negative pid signals the whole group.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }

        // Windows needs no explicit step: the kill happens when the job handle
        // is dropped. Killing the direct child first makes the common case
        // immediate rather than waiting for the guard to fall out of scope.
        let _ = child.start_kill();
    }
}

#[cfg(windows)]
mod windows {
    use std::io;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub struct Job(HANDLE);

    impl Job {
        pub fn create() -> io::Result<Self> {
            // SAFETY: both arguments are optional and null is the documented
            // way to ask for an unnamed job with default security.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }

            // SAFETY: the struct is plain data, zeroed is a valid state for it,
            // and the size passed matches the type being pointed at.
            let ok = unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };

            if ok == 0 {
                let err = io::Error::last_os_error();
                // SAFETY: the handle came from CreateJobObjectW and is not
                // used again.
                unsafe { CloseHandle(handle) };
                return Err(err);
            }

            Ok(Self(handle))
        }

        pub fn assign(&self, process: std::os::windows::io::RawHandle) -> io::Result<()> {
            // SAFETY: both handles are live for the duration of the call.
            let ok = unsafe { AssignProcessToJobObject(self.0, process as HANDLE) };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing the last handle is what triggers KILL_ON_JOB_CLOSE.
            // SAFETY: the handle is owned by this type and dropped once.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: a job handle is just a kernel object id. The Win32 calls used
    // here are documented as thread safe.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}
}
