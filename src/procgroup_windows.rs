//! Puts each command in a job object — the Windows equivalent of a process
//! group — so killing it takes the benchmark binary with it.

use std::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};

/// Nothing to configure before spawn: the job is created and joined after the
/// process exists, because a job can only be assigned an existing process.
pub fn configure(_cmd: &mut Command) {}

/// Creates a job with kill-on-close and assigns the child to it, then
/// terminates it. Called only on the timeout path.
pub fn kill_tree(child: &mut Child) {
    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if !job.is_null() {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            let handle = get_handle(child);
            if !handle.is_null() {
                AssignProcessToJobObject(job, handle);
            }
            TerminateJobObject(job, 1);
            CloseHandle(job);
        }
    }
    let _ = child.kill();
}

fn get_handle(child: &Child) -> HANDLE {
    use std::os::windows::io::AsRawHandle;
    child.as_raw_handle() as HANDLE
}
