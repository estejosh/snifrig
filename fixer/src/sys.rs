//! Low-level Windows calls the governor needs. STUB: an agent fills these bodies.
//! Contract (every function closes its handles and never panics; failures return Err/None):
//! - snapshot(): every process via NtQuerySystemInformation(SystemProcessInformation=5) into a
//!   reusable buffer held by the caller: pid, ppid, image name, create time (FILETIME u64),
//!   total CPU time (kernel+user, 100 ns units). One syscall, no per-process handles.
//! - foreground_pid(): GetForegroundWindow + GetWindowThreadProcessId; None if no window.
//! - get_priority(pid) / set_priority(pid, class): GetPriorityClass / SetPriorityClass with
//!   PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION. Never set above NORMAL.
//! - set_efficiency(pid, on): SetProcessInformation(ProcessPowerThrottling) with
//!   PROCESS_POWER_THROTTLING_EXECUTION_SPEED in ControlMask, StateMask = on ? that bit : 0.
//!   Turning off must hand control back to Windows: ControlMask = 0, StateMask = 0.
//! - set_io_priority(pid, prio): NtSetInformationProcess(ProcessIoPriority = 33), u32 value
//!   0 = very low, 1 = low, 2 = normal. Declare ntdll via raw-dylib.
//! - set_memory_priority(pid, prio): SetProcessInformation(ProcessMemoryPriority) with
//!   MEMORY_PRIORITY_INFORMATION; 1 = very low .. 5 = normal.
//! - set_affinity(pid, mask): SetProcessAffinityMask. get_affinity returns (process, system).
//! - cap_cpu(pid, percent): put the process in a new job object (AssignProcessToJobObject,
//!   nested jobs are fine on Windows 8+) with JOBOBJECT_CPU_RATE_CONTROL_INFORMATION
//!   ENABLE | HARD_CAP, CpuRate = percent * 100. Keep the job handle in a process-wide table
//!   keyed by pid so uncap(pid) can set the rate back to 100% (a process cannot leave a job).
//! - image_matches(pid, created, name): true if the pid still has that create time and name
//!   (OpenProcess + GetProcessTimes + QueryFullProcessImageNameW). Used before every change.
//! - cmdline(pid): command line via NtQueryInformationProcess class 60, "" if unreadable.

#[derive(Clone, Debug, Default)]
pub struct Proc { pub pid: u32, pub ppid: u32, pub name: String, pub created: u64, pub cpu: u64 }

pub const IDLE: u32 = 0x40;
pub const BELOW_NORMAL: u32 = 0x4000;
pub const NORMAL: u32 = 0x20;

pub fn snapshot(buf: &mut Vec<u8>) -> Vec<Proc> { let _ = buf; todo!() }
pub fn foreground_pid() -> Option<u32> { todo!() }
pub fn get_priority(pid: u32) -> Option<u32> { let _ = pid; todo!() }
pub fn set_priority(pid: u32, class: u32) -> Result<(), String> { let _ = (pid, class); todo!() }
pub fn set_efficiency(pid: u32, on: bool) -> Result<(), String> { let _ = (pid, on); todo!() }
pub fn set_io_priority(pid: u32, prio: u32) -> Result<(), String> { let _ = (pid, prio); todo!() }
pub fn set_memory_priority(pid: u32, prio: u32) -> Result<(), String> { let _ = (pid, prio); todo!() }
pub fn get_affinity(pid: u32) -> Option<(u64, u64)> { let _ = pid; todo!() }
pub fn set_affinity(pid: u32, mask: u64) -> Result<(), String> { let _ = (pid, mask); todo!() }
pub fn cap_cpu(pid: u32, percent: u32) -> Result<(), String> { let _ = (pid, percent); todo!() }
pub fn uncap(pid: u32) -> Result<(), String> { let _ = pid; todo!() }
pub fn image_matches(pid: u32, created: u64, name: &str) -> bool { let _ = (pid, created, name); todo!() }
pub fn cmdline(pid: u32) -> String { let _ = pid; todo!() }
