//! Hand-written bindings for the Darwin APIs libc doesn't cover cleanly.
use libc::{c_int, c_void};

pub const PROC_PIDVNODEPATHINFO: c_int = 9;
pub const VNODE_PATH_INFO_SIZE: usize = 2352;
/// `vnode_info_path.vip_path` of `pvi_cdir`: follows a 152 byte `vnode_info`.
pub const VNODE_CDIR_PATH: std::ops::Range<usize> = 152..152 + 1024;
pub const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
pub const RUSAGE_INFO_V0: c_int = 0;
pub const HOST_VM_INFO64: c_int = 4;
pub const HOST_VM_INFO64_COUNT: u32 = 38;
pub const PROCESSOR_CPU_LOAD_INFO: c_int = 2;
pub const KERN_SUCCESS: c_int = 0;

/// `rusage_info_v0`. Times are mach ticks, not nanoseconds.
#[repr(C)]
#[derive(Default)]
pub struct RusageInfoV0 {
    pub uuid: [u8; 16],
    pub user_time: u64,
    pub system_time: u64,
    pub pkg_idle_wkups: u64,
    pub interrupt_wkups: u64,
    pub pageins: u64,
    pub wired_size: u64,
    pub resident_size: u64,
    pub phys_footprint: u64,
    pub proc_start_abstime: u64,
    pub proc_exit_abstime: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct MachTimebase {
    pub numer: u32,
    pub denom: u32,
}

/// `vm_statistics64`, HOST_VM_INFO64_COUNT natural_t words.
#[repr(C, align(8))]
#[derive(Default)]
pub struct VmStatistics64 {
    pub free_count: u32,
    pub active_count: u32,
    pub inactive_count: u32,
    pub wire_count: u32,
    pub zero_fill_count: u64,
    pub reactivations: u64,
    pub pageins: u64,
    pub pageouts: u64,
    pub faults: u64,
    pub cow_faults: u64,
    pub lookups: u64,
    pub hits: u64,
    pub purges: u64,
    pub purgeable_count: u32,
    pub speculative_count: u32,
    pub decompressions: u64,
    pub compressions: u64,
    pub swapins: u64,
    pub swapouts: u64,
    pub compressor_page_count: u32,
    pub throttled_count: u32,
    pub external_page_count: u32,
    pub internal_page_count: u32,
    pub total_uncompressed_pages_in_compressor: u64,
}

/// `xsw_usage` from `vm.swapusage`.
#[repr(C)]
#[derive(Default)]
pub struct XswUsage {
    pub total: u64,
    pub avail: u64,
    pub used: u64,
    pub pagesize: u32,
    pub encrypted: u32,
}

unsafe extern "C" {
    pub fn proc_pidpath(pid: c_int, buf: *mut c_void, size: u32) -> c_int;
    pub fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buf: *mut c_void,
        size: c_int,
    ) -> c_int;
    pub fn proc_pid_rusage(pid: c_int, flavor: c_int, buf: *mut c_void) -> c_int;
    pub fn mach_timebase_info(info: *mut MachTimebase) -> c_int;
    pub fn mach_host_self() -> u32;
    pub fn host_statistics64(host: u32, flavor: c_int, info: *mut c_void, count: *mut u32)
    -> c_int;
    pub fn host_processor_info(
        host: u32,
        flavor: c_int,
        cpu_count: *mut u32,
        info: *mut *mut i32,
        info_count: *mut u32,
    ) -> c_int;
    pub fn vm_deallocate(task: u32, addr: usize, size: usize) -> c_int;
    pub static mach_task_self_: u32;
}

/// Reads a fixed-size value by sysctl name.
pub fn sysctl_read<T: Default>(name: &str) -> Option<T> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value = T::default();
    let mut size = size_of::<T>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && size == size_of::<T>()).then_some(value)
}

pub fn sysctl_string(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut buf = [0u8; 128];
    let mut size = buf.len();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then(|| c_bytes(&buf[..size.min(buf.len())]))
}

/// Text before the first NUL.
pub fn c_bytes(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
