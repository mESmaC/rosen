//! Installed memory sizes the run queue's reserve; current memory
//! headroom gates new turns (SPEC R3 "Run queue").

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
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let pages = u64::try_from(pages).ok()?;
    let page_size = u64::try_from(page_size).ok()?;
    pages.checked_mul(page_size)
}

#[cfg(not(any(windows, unix)))]
pub fn physical_memory() -> Option<u64> {
    None
}

/// Available memory in bytes, including reclaimable file cache on Windows and
/// capped by free system commit. `None` means this OS cannot report it; it
/// never means zero free bytes.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn available_memory() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    let size = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
    let mut status = PERFORMANCE_INFORMATION {
        cb: size,
        ..Default::default()
    };
    // SAFETY: status is an initialized repr(C) buffer of the supplied size,
    // valid for the entire synchronous call. No pointer escapes.
    if unsafe { GetPerformanceInfo(&mut status, size) } == 0 {
        return None;
    }
    let commit = status.CommitLimit.saturating_sub(status.CommitTotal);
    // `PhysicalAvailable` is standby + free + zero pages; `SystemCache` is
    // standby + the system file cache working set, which Windows trims under
    // pressure (a large ReFS volume can hold tens of GiB there). Taking their
    // max counts that reclaimable cache without double-counting standby pages.
    let physical = status
        .PhysicalAvailable
        .max(status.SystemCache)
        .min(status.PhysicalTotal);
    let pages = commit.min(physical);
    u64::try_from(pages)
        .ok()?
        .checked_mul(status.PageSize as u64)
}

#[cfg(target_os = "linux")]
pub fn available_memory() -> Option<u64> {
    let memory = std::fs::read_to_string("/proc/meminfo").ok()?;
    // MemAvailable includes reclaimable caches, unlike MemFree, and excludes
    // swap: starting more work must not depend on swapping existing work out.
    let line = memory
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?;
    line.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
// libc deprecates its Mach bindings in favour of the mach2 crate. The call is the same;
// moving one function to a new dependency is not worth it while libc still provides it.
#[allow(deprecated)]
pub fn available_memory() -> Option<u64> {
    // Keep one host send right for this process's lifetime, rather than leak
    // another right on every queue poll.
    static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    // SAFETY: mach_host_self takes no pointers and returns our host send right.
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    // SAFETY: vm_statistics64 is a plain C data buffer; zero is valid for all
    // its integer fields, including those an older kernel does not fill.
    let mut status: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: the flavor and count match status's layout. Both output buffers
    // remain valid for the synchronous call and the host right stays live.
    let result = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            std::ptr::from_mut(&mut status).cast(),
            &mut count,
        )
    };
    // SAFETY: sysconf takes a plain integer name and touches no memory of ours.
    let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    if result != libc::KERN_SUCCESS {
        return None;
    }
    // Speculative pages are already in free_count; adding them would double
    // count headroom. Include inactive pages as reclaimable headroom.
    (u64::from(status.free_count) + u64::from(status.inactive_count)).checked_mul(page_size)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn available_memory() -> Option<u64> {
    None
}
