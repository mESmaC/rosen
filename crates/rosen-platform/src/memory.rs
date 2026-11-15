//! The machine's installed memory, which sizes the run queue's slots (SPEC R3
//! "Run queue"): parallel builds that together overrun memory page to disk and
//! finish later than the same builds taking turns.

/// Bytes of physical memory the operating system can use, or `None` when it
/// won't say.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn physical_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `status` is an initialized repr(C) MEMORYSTATUSEX whose
    // dwLength names its own size, as the call requires; the pointer is to a
    // local that outlives the synchronous call, which only writes into it.
    let success = unsafe { GlobalMemoryStatusEx(&mut status) };
    (success != 0).then_some(status.ullTotalPhys)
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub fn physical_memory() -> Option<u64> {
    // SAFETY: sysconf takes a plain integer name and touches no memory of ours.
    let (pages, page_size) =
        unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    let pages = u64::try_from(pages).ok()?;
    let page_size = u64::try_from(page_size).ok()?;
    pages.checked_mul(page_size)
}

#[cfg(not(any(windows, unix)))]
pub fn physical_memory() -> Option<u64> {
    None
}
