use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
};
use tokio::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, FILETIME, GetLastError,
        INVALID_HANDLE_VALUE,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, OpenJobObjectW,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        SystemInformation::{
            GlobalMemoryStatusEx, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
            MEMORYSTATUSEX,
        },
        SystemServices::JOB_OBJECT_QUERY,
        Threading::{
            CREATE_SUSPENDED, GetCurrentProcess, GetSystemTimes, IsWow64Process2, OpenThread,
            ResumeThread, THREAD_SUSPEND_RESUME,
        },
    },
};

pub(super) struct Job {
    handle: OwnedHandle,
    pub name: String,
}

pub(super) struct SuspendedThread(OwnedHandle);

impl SuspendedThread {
    pub fn resume(self) -> Result<()> {
        let previous = unsafe { ResumeThread(self.0.as_raw_handle()) };
        ensure!(
            previous != u32::MAX,
            "resume command: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            previous == 1,
            "unexpected command thread suspend count {previous}"
        );
        Ok(())
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn active(handle: &OwnedHandle) -> Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    ensure!(
        unsafe {
            QueryInformationJobObject(
                handle.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        } != 0,
        "inspect command job: {}",
        std::io::Error::last_os_error()
    );
    Ok(info.ActiveProcesses)
}

impl Job {
    fn create() -> Result<Self> {
        // Global names let the same user reconcile an attempt after a session change.
        let name = format!("Global\\YLCC-{}", uuid::Uuid::new_v4());
        let handle = unsafe { CreateJobObjectW(null(), wide(&name).as_ptr()) };
        ensure!(
            !handle.is_null(),
            "create command job: {}",
            std::io::Error::last_os_error()
        );
        let existing = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        ensure!(!existing, "command job identity already exists");
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        ensure!(
            unsafe {
                SetInformationJobObject(
                    handle.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } != 0,
            "configure command job: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self { handle, name })
    }

    pub fn active(&self) -> Result<u32> {
        active(&self.handle)
    }

    pub fn terminate(&self) -> Result<()> {
        ensure!(
            unsafe { TerminateJobObject(self.handle.as_raw_handle(), 1) } != 0,
            "terminate command job: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}

pub(super) async fn spawn(command: &mut Command) -> Result<(Child, Job, SuspendedThread)> {
    let job = Job::create()?;
    // No command code runs before its entire future process tree belongs to the job.
    command.creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn().context("start suspended command")?;
    let attached = (|| {
        let process = child
            .raw_handle()
            .context("spawned process has no handle")?;
        ensure!(
            unsafe { AssignProcessToJobObject(job.handle.as_raw_handle(), process) } != 0,
            "assign command job: {}",
            std::io::Error::last_os_error()
        );
        let pid = child.id().context("spawned process has no PID")?;
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        ensure!(
            snapshot != INVALID_HANDLE_VALUE,
            "inspect suspended command: {}",
            std::io::Error::last_os_error()
        );
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut thread_id = None;
        let mut present = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while present != 0 {
            if entry.th32OwnerProcessID == pid {
                ensure!(
                    thread_id.is_none(),
                    "suspended command has unexpected additional threads"
                );
                thread_id = Some(entry.th32ThreadID);
            }
            present = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
        }
        ensure!(
            unsafe { GetLastError() } == ERROR_NO_MORE_FILES,
            "enumerate suspended command threads: {}",
            std::io::Error::last_os_error()
        );
        let thread = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME,
                0,
                thread_id.context("suspended command thread not found")?,
            )
        };
        ensure!(
            !thread.is_null(),
            "open suspended command thread: {}",
            std::io::Error::last_os_error()
        );
        Ok::<_, anyhow::Error>(SuspendedThread(unsafe {
            OwnedHandle::from_raw_handle(thread)
        }))
    })();
    match attached {
        Ok(thread) => Ok((child, job, thread)),
        Err(error) => {
            match tokio::time::timeout(std::time::Duration::from_secs(5), child.kill()).await {
                Ok(Ok(())) => {}
                cleanup => {
                    return Err(anyhow::Error::new(super::UncertainCleanup(format!(
                        "job setup failed: {error:#}; suspended command cleanup could not be confirmed: {cleanup:?}"
                    ))));
                }
            }
            Err(error)
        }
    }
}

pub(super) fn absent(name: &str) -> Result<bool> {
    ensure!(
        name.strip_prefix("Global\\YLCC-")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
        "invalid saved Windows job identity"
    );
    let handle = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, wide(name).as_ptr()) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32),
            "cannot inspect saved command job: {error}"
        );
        return Ok(true);
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    Ok(active(&handle)? == 0)
}

pub(super) fn native_arch() -> Result<&'static str> {
    let mut process = 0;
    let mut native = 0;
    ensure!(
        unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) } != 0,
        "inspect native Windows architecture: {}",
        std::io::Error::last_os_error()
    );
    match native {
        IMAGE_FILE_MACHINE_AMD64 => Ok("x86_64"),
        IMAGE_FILE_MACHINE_ARM64 => Ok("aarch64"),
        value => anyhow::bail!("unsupported native Windows architecture {value}"),
    }
}

pub(super) fn memory_bytes() -> Result<u64> {
    let mut info = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    ensure!(
        unsafe { GlobalMemoryStatusEx(&mut info) } != 0,
        "inspect physical memory: {}",
        std::io::Error::last_os_error()
    );
    Ok(info.ullTotalPhys)
}

/// Cumulative (busy, total) CPU time; kernel time includes idle time.
pub(super) fn cpu_ticks() -> Option<(u64, u64)> {
    let [mut idle, mut kernel, mut user] = [FILETIME::default(); 3];
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let ticks = |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let total = ticks(kernel) + ticks(user);
    Some((total.saturating_sub(ticks(idle)), total))
}

/// (used, total) physical memory in bytes.
pub(super) fn memory_usage() -> Option<(u64, u64)> {
    let mut info = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    (unsafe { GlobalMemoryStatusEx(&mut info) } != 0)
        .then(|| (info.ullTotalPhys - info.ullAvailPhys, info.ullTotalPhys))
}
