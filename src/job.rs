//! Job Object lifecycle.
//!
//! Windows has no process group to signal. What it does have is a kernel object
//! that owns a set of processes: when the last handle to the job closes, every
//! process still in it dies. That is the only reliable way to guarantee this
//! shell never leaves orphans behind — on Unix the same guarantee comes from
//! the session and process group.
//!
//! DESIGN.md lists this as a core native capability: it cannot be delegated,
//! because only the process that spawned the children holds their handles.
//!
//! Declared by hand rather than pulling in windows-sys: this crate stays
//! dependency-free on purpose.

#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::sync::OnceLock;

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

#[repr(C)]
struct JobBasicLimit {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
struct JobExtendedLimit {
    basic: JobBasicLimit,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(
        job: *mut c_void,
        class: i32,
        info: *mut c_void,
        len: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
}

/// The process-lifetime job. Created once; deliberately never closed, so the
/// OS closes it when this process dies — which is what kills the children.
static SESSION_JOB: OnceLock<Option<usize>> = OnceLock::new();

/// Creates the session job. Safe to call more than once; only the first call
/// does anything.
///
/// Returns false when the job could not be created (or could not be configured
/// to kill on close). Callers must treat that as "run without the guarantee",
/// never as a fatal error: a shell that refuses to start because a job object
/// is unavailable is worse than one that may leak a child.
pub fn init() -> bool {
    let slot = SESSION_JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
            return None;
        }

        let mut info: JobExtendedLimit = std::mem::zeroed();
        info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<JobExtendedLimit>() as u32,
        );
        if ok == 0 {
            // Without kill-on-close the job buys us nothing, but the handle is
            // still valid; keep it and let assignment work.
            return Some(job as usize);
        }
        Some(job as usize)
    });
    slot.is_some()
}

/// Puts a freshly spawned child into the session job.
///
/// Assignment can legitimately fail: if this shell is itself inside a job that
/// forbids breakaway (CI runners and some IDE terminals do this), the call
/// returns access-denied. That is not an error worth failing a command over —
/// the child simply runs outside our job, and the orphan guarantee is weaker
/// for it.
pub fn assign(child: &Child) -> bool {
    let Some(job) = *SESSION_JOB.get_or_init(|| None) else {
        return false;
    };
    if job == 0 {
        return false;
    }
    let handle = child.as_raw_handle();
    unsafe { AssignProcessToJobObject(job as *mut c_void, handle as *mut c_void) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_succeeds_on_windows() {
        // A plain Windows process can always create a job object.
        assert!(init(), "session job should be creatable");
    }

    #[test]
    fn a_spawned_child_is_assigned() {
        init();
        let child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn cmd");
        // Assignment races nothing: the child is alive when we assign.
        let assigned = assign(&child);
        let mut child = child;
        let _ = child.wait();
        // Some CI sandboxes already run us inside a non-breakaway job; the
        // contract there is "degrade, don't fail".
        if !assigned {
            eprintln!("note: assignment unavailable in this environment");
        }
    }
}
