//! Windows command containment for `rosen run` (SPEC R3). The job handle
//! belongs only to the supervising client; closing it kills every member,
//! including when Windows closes it after a forced client exit.

use std::ffi::OsStr;
use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::{ptr, thread, time::Duration};

use windows_sys::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES,
    STARTUPINFOEXW,
};

use crate::process::{command_line, terminated};

/// A launcher and all its descendants, contained before any of them runs.
/// Dropping it terminates and drains the job before returning to the caller.
/// `program` must be an executable, not a batch script; arguments use CRT
/// quoting. Standard input/output, directory and environment are inherited,
/// with `environment` overriding named variables.
pub struct ProcessTree {
    job: Job,
    process: OwnedHandle,
}

impl ProcessTree {
    pub fn spawn(
        program: &Path,
        args: &[&OsStr],
        environment: &[(&OsStr, &OsStr)],
    ) -> io::Result<Self> {
        let job = Job::new()?;
        let process = create_process(&job.handle, program, args, &environment_block(environment)?)?;
        Ok(Self { job, process })
    }

    /// Keeps ownership until the entire tree exits, returning the launcher's
    /// status. A failed launcher stops its remaining descendants as well.
    pub fn wait(self) -> io::Result<ExitStatus> {
        // SAFETY: this owned process handle remains live throughout the wait.
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0;
        // SAFETY: the handle is live and code is a writable out parameter.
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let status = ExitStatus::from_raw(code);
        if !status.success() {
            self.job.terminate()?;
        }
        self.job.wait_empty()?;
        Ok(status)
    }
}

struct Job {
    handle: OwnedHandle,
}

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: null attributes/name create a private, non-inheritable job.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a fresh owned handle.
        let job = Self {
            handle: unsafe { OwnedHandle::from_raw_handle(raw) },
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the initialized buffer has the layout/size required by this
        // information class, and the handle is live. No breakaway is allowed.
        let set = unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn active(&self) -> io::Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: info is an initialized, writable buffer of the requested
        // class and size. The owned job handle stays live throughout the call.
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                ptr::from_mut(&mut info).cast(),
                std::mem::size_of_val(&info) as u32,
                ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }

    fn wait_empty(&self) -> io::Result<()> {
        while self.active()? != 0 {
            thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }

    fn terminate(&self) -> io::Result<()> {
        // SAFETY: this private job handle has terminate access. The supervisor
        // is outside the job, so it can wait for every member to stop.
        if unsafe { TerminateJobObject(self.handle.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        self.wait_empty()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if !matches!(self.active(), Ok(0)) {
            // Drain before the caller drops its queue turn. Closing the handle
            // also enforces kill-on-close if querying/termination ever fails.
            let _ = self.terminate();
        }
    }
}

fn environment_block(overrides: &[(&OsStr, &OsStr)]) -> io::Result<Vec<u16>> {
    let mut environment: Vec<_> = std::env::vars_os().collect();
    for (name, value) in overrides {
        environment.retain(|(key, _)| {
            !key.as_encoded_bytes()
                .eq_ignore_ascii_case(name.as_encoded_bytes())
        });
        environment.push(((*name).to_owned(), (*value).to_owned()));
    }
    // Windows requires its Unicode environment block sorted without case.
    environment.sort_by_cached_key(|(name, _)| name.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (name, value) in environment {
        let mut pair = name;
        pair.push("=");
        pair.push(value);
        block.extend(terminated(&pair)?);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

fn standard_handles() -> io::Result<[Option<OwnedHandle>; 3]> {
    let mut handles = [None, None, None];
    for (slot, which) in
        handles
            .iter_mut()
            .zip([STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE])
    {
        // SAFETY: these calls take only well-defined standard-handle constants.
        let (source, process) = unsafe { (GetStdHandle(which), GetCurrentProcess()) };
        if source.is_null() || source == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut duplicate = ptr::null_mut();
        // SAFETY: the source belongs to this process, duplicate is writable,
        // and the returned handle is owned here. Only these duplicates cross
        // the process boundary; the job and queue handles never do.
        if unsafe {
            DuplicateHandle(
                process,
                source,
                process,
                &mut duplicate,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: DuplicateHandle returned a new owned handle.
        *slot = Some(unsafe { OwnedHandle::from_raw_handle(duplicate) });
    }
    Ok(handles)
}

fn create_process(
    job: &OwnedHandle,
    program: &Path,
    args: &[&OsStr],
    environment: &[u16],
) -> io::Result<OwnedHandle> {
    let application = terminated(program.as_os_str())?;
    let mut line = command_line(program.as_os_str(), args)?;
    let handles = standard_handles()?;
    let raw_handles = handles.each_ref().map(|handle| {
        handle
            .as_ref()
            .map_or(ptr::null_mut(), AsRawHandle::as_raw_handle)
    });
    let inherited: Vec<_> = raw_handles
        .iter()
        .copied()
        .filter(|handle| !handle.is_null())
        .collect();
    let jobs = [job.as_raw_handle()];
    let mut attributes = Attributes::new(if inherited.is_empty() { 1 } else { 2 })?;
    attributes.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    if !inherited.is_empty() {
        attributes.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited)?;
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of_val(&startup) as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        startup.StartupInfo.hStdInput,
        startup.StartupInfo.hStdOutput,
        startup.StartupInfo.hStdError,
    ] = raw_handles;
    startup.lpAttributeList = attributes.pointer();
    launch(
        &application,
        &mut line,
        environment,
        &startup,
        !inherited.is_empty(),
    )
}

fn launch(
    application: &[u16],
    line: &mut [u16],
    environment: &[u16],
    startup: &STARTUPINFOEXW,
    inherit: bool,
) -> io::Result<OwnedHandle> {
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: strings/environment are terminated and remain live, line is
    // writable, startup/info have the required layouts and sizes, and every
    // attribute value outlives creation. The explicit handle list excludes
    // the non-inheritable job handle. Job assignment happens inside creation.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(inherit),
            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            ptr::null(),
            &startup.StartupInfo,
            &mut info,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful creation returns two fresh owned handles.
    let (process, _thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(info.hProcess),
            OwnedHandle::from_raw_handle(info.hThread),
        )
    };
    Ok(process)
}

struct Attributes<'a> {
    // usize provides the alignment the Windows attribute list requires.
    storage: Vec<usize>,
    values: PhantomData<&'a [HANDLE]>,
}

impl<'a> Attributes<'a> {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: a null list asks for its required size through bytes.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: storage is aligned, writable and at least bytes long.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &mut bytes)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            storage,
            values: PhantomData,
        })
    }

    fn pointer(&mut self) -> *mut std::ffi::c_void {
        self.storage.as_mut_ptr().cast()
    }

    fn add(&mut self, kind: u32, handles: &'a [HANDLE]) -> io::Result<()> {
        // SAFETY: the list is initialized; handles has the size/layout for
        // these handle-list attributes and its borrow outlives the list.
        let added = unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                kind as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if added == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        // SAFETY: this list was initialized and has not been deleted yet.
        unsafe { DeleteProcThreadAttributeList(self.pointer()) };
    }
}
