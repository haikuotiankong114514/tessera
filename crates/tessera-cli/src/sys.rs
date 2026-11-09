//! Process memory and CPU counters for benchmarks.

use std::time::Duration;

/// Process memory, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Memory {
    pub working_set: u64,
    pub peak_working_set: u64,
    /// Committed private memory (Task Manager's "commit size").
    pub private_bytes: u64,
}

#[cfg(windows)]
pub fn memory() -> Memory {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut _, c.cb) };
    if ok.is_err() {
        return Memory::default();
    }
    Memory {
        working_set: c.WorkingSetSize as u64,
        peak_working_set: c.PeakWorkingSetSize as u64,
        private_bytes: c.PrivateUsage as u64,
    }
}

/// User + kernel CPU time consumed by the whole process so far.
#[cfg(windows)]
pub fn cpu_time() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.is_err() {
        return Duration::ZERO;
    }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Duration::from_nanos((ticks(k) + ticks(u)) * 100)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn memory() -> Memory {
    // Development fallback (Linux): read /proc/self/status.
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| -> u64 {
        s.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    };
    Memory {
        working_set: field("VmRSS:"),
        peak_working_set: field("VmHWM:"),
        private_bytes: field("RssAnon:"),
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn cpu_time() -> Duration {
    // utime + stime from /proc/self/stat, in clock ticks (assume 100 Hz).
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = s.rsplit(')').next().unwrap_or("");
    let f: Vec<u64> = after
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    // After the ')' come fields 3.. (state, ppid, ...); the non-numeric state
    // is dropped by the filter, so utime (field 14) and stime (15) land at 10 and 11.
    let ticks = f.get(10).copied().unwrap_or(0) + f.get(11).copied().unwrap_or(0);
    Duration::from_millis(ticks * 10)
}

#[cfg(target_os = "macos")]
fn usage() -> libc::rusage_info_v2 {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            (&mut info as *mut libc::rusage_info_v2).cast(),
        );
    }
    info
}

#[cfg(target_os = "macos")]
pub fn memory() -> Memory {
    let info = usage();
    let mut peak: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut peak);
    }
    Memory {
        working_set: info.ri_resident_size,
        peak_working_set: peak.ru_maxrss.max(0) as u64,
        private_bytes: info.ri_phys_footprint,
    }
}

#[cfg(target_os = "macos")]
pub fn cpu_time() -> Duration {
    // proc_pid_rusage CPU counters use Mach ticks on Apple Silicon.
    // getrusage exposes timeval units on both supported architectures.
    let mut info: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut info) } != 0 {
        return Duration::ZERO;
    }
    let user = Duration::from_secs(info.ru_utime.tv_sec.max(0) as u64)
        + Duration::from_micros(info.ru_utime.tv_usec.max(0) as u64);
    let system = Duration::from_secs(info.ru_stime.tv_sec.max(0) as u64)
        + Duration::from_micros(info.ru_stime.tv_usec.max(0) as u64);
    user + system
}
