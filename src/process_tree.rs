//! Isolated command process trees shared by bounded command runners.

use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(windows)]
use std::os::windows::{io::AsRawHandle as _, process::CommandExt as _};
use std::process::{Child, Command};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

#[cfg(unix)]
pub(crate) struct ProcessTree(pub(crate) Option<libc::pid_t>);

#[cfg(unix)]
pub(crate) fn configure_process_tree(command: &mut Command) {
    command.process_group(0);
}

#[cfg(unix)]
pub(crate) fn attach_process_tree(child: &mut Child) -> io::Result<ProcessTree> {
    Ok(ProcessTree(Some(child.id() as libc::pid_t)))
}

#[cfg(unix)]
impl ProcessTree {
    pub(crate) fn terminate(&mut self, child: &mut Child) {
        if let Some(process_group) = self.0.take() {
            let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        }
        // The primary can call setsid(2) and leave the process group that was
        // assigned immediately after spawn. Always check and kill the exact
        // child as well so the subsequent reap cannot wait on a live escapee.
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Some(process_group) = self.0.take() {
            let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
pub(crate) struct ProcessTree(HANDLE);

#[cfg(windows)]
pub(crate) fn configure_process_tree(command: &mut Command) {
    command.creation_flags(CREATE_SUSPENDED);
}

#[cfg(windows)]
pub(crate) fn attach_process_tree(child: &mut Child) -> io::Result<ProcessTree> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(error);
    }
    if unsafe { AssignProcessToJobObject(job, child.as_raw_handle().cast()) } == 0 {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(error);
    }
    let process_tree = ProcessTree(job);
    if let Err(error) = resume_suspended_child(child) {
        drop(process_tree);
        return Err(error);
    }
    Ok(process_tree)
}

#[cfg(windows)]
fn resume_suspended_child(child: &Child) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        let mut has_entry = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        let mut resumed = 0_usize;
        while has_entry {
            if entry.th32OwnerProcessID == child.id() {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let result = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if result == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed = resumed.saturating_add(1);
            }
            has_entry = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        if resumed == 0 {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "could not find the suspended command primary thread",
            ))
        } else {
            Ok(())
        }
    })();
    unsafe { CloseHandle(snapshot) };
    result
}

#[cfg(windows)]
impl ProcessTree {
    pub(crate) fn terminate(&mut self, child: &mut Child) {
        if !self.0.is_null() {
            unsafe {
                TerminateJobObject(self.0, 1);
                CloseHandle(self.0);
            }
            self.0 = std::ptr::null_mut();
        }
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}
