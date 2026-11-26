//! What the host says about the machine: its physical memory and the
//! processors this process may run on, which Windows reports to titles
//! (`GlobalMemoryStatusEx`, `GetSystemInfo`).

use crate::Host;

/// What the host says about the machine.
pub trait System {
    /// Physical memory: total bytes and bytes available now.
    fn physical_memory() -> (u64, u64);
    /// Logical processors this process may run on.
    fn logical_processors() -> usize;
}

impl System for Host {
    fn physical_memory() -> (u64, u64) {
        host::physical_memory()
    }

    fn logical_processors() -> usize {
        // The host may decline to say (a restricted sandbox): one processor
        // is then the safe answer.
        std::thread::available_parallelism().map_or(1, |n| n.get())
    }
}

#[cfg(target_os = "linux")]
mod host {
    /// `MemTotal:` and `MemAvailable:` of `/proc/meminfo`, in bytes; zeros
    /// for what cannot be read.
    pub(super) fn physical_memory() -> (u64, u64) {
        let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
            return (0, 0);
        };
        (field(&info, "MemTotal:"), field(&info, "MemAvailable:"))
    }

    /// The value of one `/proc/meminfo` line, given there in kB, in bytes.
    fn field(info: &str, name: &str) -> u64 {
        info.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kb| kb.parse::<u64>().ok())
            .map_or(0, |kb| kb.saturating_mul(1024))
    }
}

#[cfg(target_os = "macos")]
mod host {
    use std::sync::OnceLock;

    unsafe extern "C" {
        /// `<mach/mach_init.h>`: a send right to this machine's host port
        /// (libc's copy is deprecated).
        fn mach_host_self() -> libc::mach_port_t;

        /// `<mach/vm_page_size.h>`: the kernel's page size, the unit
        /// `host_statistics64` counts pages in: 16 KiB on Apple silicon,
        /// for an x86-64 process under Rosetta 2 too, whose own pages
        /// (`vm_page_size`) are 4 KiB.
        static vm_kernel_page_size: libc::vm_size_t;
    }

    /// The host port, taken once: each `mach_host_self` call adds a
    /// reference to the right, so holding one for the process's life keeps
    /// repeated queries from piling references up.
    fn host_port() -> libc::mach_port_t {
        static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
        // SAFETY: a plain query with no arguments.
        *PORT.get_or_init(|| unsafe { mach_host_self() })
    }

    /// `hw.memsize` for the total; the free and inactive pages of
    /// `HOST_VM_INFO64` for what is available (inactive pages are given back
    /// on demand, as Windows counts its standby list available).
    pub(super) fn physical_memory() -> (u64, u64) {
        let total = mem_size();
        // SAFETY: a plain-data struct, all of whose bit patterns are valid.
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `stats` holds `count` integers, as the kernel is told; it
        // writes no more than that.
        let result = unsafe {
            libc::host_statistics64(
                host_port(),
                libc::HOST_VM_INFO64,
                (&raw mut stats).cast(),
                &mut count,
            )
        };
        if result != libc::KERN_SUCCESS {
            return (total, 0);
        }
        // SAFETY: a value libSystem sets before `main` and never changes.
        let page = unsafe { vm_kernel_page_size } as u64;
        let pages = u64::from(stats.free_count) + u64::from(stats.inactive_count);
        (total, pages.saturating_mul(page).min(total))
    }

    /// `sysctlbyname("hw.memsize")`: the physical memory in bytes; 0 if the
    /// host refused.
    fn mem_size() -> u64 {
        let mut bytes: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: a NUL-terminated name and an 8-byte buffer whose length is
        // passed with it; nothing is written to the host.
        let result = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut bytes).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 && len == std::mem::size_of::<u64>() {
            bytes
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_has_memory() {
        let (total, available) = Host::physical_memory();
        assert!(total > available, "{total} > {available}");
        assert!(available > 0);
        assert!(total >= 1 << 30, "{total} bytes");
    }

    #[test]
    fn the_machine_has_processors() {
        assert!(Host::logical_processors() >= 1);
    }
}
