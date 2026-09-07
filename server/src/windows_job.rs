//! Own a Windows process tree so stopping a command also closes descendant output pipes.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
        },
    },
};

/// Closing the last job handle kills every process assigned to it, including descendants.
pub struct Job(OwnedHandle);

impl Job {
    pub fn spawn(command: &mut Command) -> Result<(Child, Self)> {
        // SAFETY: null attributes create a non-inheritable, unnamed job owned by this process.
        let handle = owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })
            .context("cannot create process job")?;
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the handle is live and limits has the exact layout and size required here.
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("cannot configure process job");
        }

        // Assign before the primary thread runs. Assigning a running process allows a fast
        // launcher to create an unowned descendant before the job association is installed.
        let child = command
            .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .spawn()?;
        let process = child
            .raw_handle()
            .context("new child has no process handle")?;
        // SAFETY: both handles are live; the new child is still suspended and owned by us.
        if unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error()).context("cannot assign child to process job");
        }
        resume_primary_thread(child.id().context("new child has no process ID")?)?;
        Ok((child, job))
    }
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: callers pass a newly created handle; ownership transfers exactly once.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

fn resume_primary_thread(pid: u32) -> Result<()> {
    // Stable Rust does not expose Child's primary thread handle. A suspended new process
    // has its primary thread in the system snapshot, identifiable by its owning process ID.
    // SAFETY: these flags request a read-only thread snapshot and require no pointer arguments.
    let snapshot = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })
        .context("cannot inspect the suspended child thread")?;
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: entry is a correctly sized writable structure and the snapshot handle is live.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while found {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: only a thread belonging to the child we just created is opened.
            let thread = owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })
                .context("cannot open suspended child thread")?;
            // SAFETY: the handle grants suspend/resume rights and remains live for this call.
            let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
            if previous == u32::MAX {
                return Err(io::Error::last_os_error()).context("cannot resume child thread");
            }
            anyhow::ensure!(
                previous == 1,
                "unexpected child thread suspend count: {previous}"
            );
            return Ok(());
        }
        // SAFETY: the same correctly sized structure and live snapshot are reused.
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    anyhow::bail!("cannot find the suspended child's primary thread")
}
