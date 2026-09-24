//! Platform process-tree ownership for external analyzer and formatter commands.

#[cfg(unix)]
pub(super) struct ProcessTreeGuard {
    group_id: i32,
    armed: bool,
}

#[cfg(unix)]
impl ProcessTreeGuard {
    pub(super) fn attach(child: &tokio::process::Child) -> Result<Self, String> {
        let group_id = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| "failed to identify external command process group".to_string())?;
        Ok(Self {
            group_id,
            armed: true,
        })
    }

    pub(super) fn terminate(&mut self) {
        if self.armed {
            // The shell leads a new group containing its ordinary descendants.
            unsafe { libc::kill(-self.group_id, libc::SIGKILL) };
            self.armed = false;
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(windows)]
pub(super) struct ProcessTreeGuard {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for ProcessTreeGuard {}

#[cfg(windows)]
impl ProcessTreeGuard {
    pub(super) fn attach(child: &tokio::process::Child) -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(format!(
                "failed to create external command job: {}",
                std::io::Error::last_os_error()
            ));
        }
        let guard = Self { job };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            return Err(format!(
                "failed to configure external command job: {}",
                std::io::Error::last_os_error()
            ));
        }
        let Some(process) = child.raw_handle() else {
            return Err("external command exited before job assignment".to_string());
        };
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            return Err(format!(
                "failed to assign external command to job: {}",
                std::io::Error::last_os_error()
            ));
        }
        resume_suspended_primary_thread(
            child
                .id()
                .ok_or_else(|| "external command exited before resume".to_string())?,
        )?;
        Ok(guard)
    }

    pub(super) fn terminate(&mut self) {
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
    }
}

#[cfg(windows)]
fn resume_suspended_primary_thread(process_id: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(format!(
            "failed to inspect suspended command threads: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    let outcome = loop {
        if !found {
            break Err("suspended external command thread was not found".to_string());
        }
        if entry.th32OwnerProcessID == process_id {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                break Err(format!(
                    "failed to open suspended command thread: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let previous = unsafe { ResumeThread(thread) };
            let resume_error = (previous == u32::MAX).then(std::io::Error::last_os_error);
            unsafe { CloseHandle(thread) };
            break if let Some(err) = resume_error {
                Err(format!("failed to resume external command thread: {err}"))
            } else {
                Ok(())
            };
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        found = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    };
    unsafe { CloseHandle(snapshot) };
    outcome
}
