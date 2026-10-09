//! Windows 进程探测 / 终止所需的最小 FFI 声明（对应 Unix 侧 `libc::kill`）。
//!
//! @intent 与 `libc` 同理：纯 FFI 声明，无编译期成本。只在使用到它的 Windows
//!         目标上编译；Unix 侧继续用 `libc`，两侧调用点按平台分体。

use std::ffi::c_void;

/// 查询退出码所需的最小进程权限（PROCESS_QUERY_LIMITED_INFORMATION）。
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
/// 终止进程所需权限（PROCESS_TERMINATE）。
pub const PROCESS_TERMINATE: u32 = 0x0001;
/// `GetExitCodeProcess` 在进程存活时返回的退出码（STILL_ACTIVE）。
pub const STILL_ACTIVE: u32 = 259;
/// Win32 `BOOL` 的「假」。
pub const FALSE: i32 = 0;

/// 进程句柄。
pub type Handle = *mut c_void;

#[link(name = "kernel32")]
extern "system" {
    /// 按 pid 打开进程句柄；失败返回 NULL。
    pub fn OpenProcess(
        dw_desired_access: u32,
        b_inherit_handle: i32,
        dw_process_id: u32,
    ) -> Handle;
    /// 终止进程。
    pub fn TerminateProcess(h_process: Handle, u_exit_code: u32) -> i32;
    /// 读取进程退出码；进程存活时为 STILL_ACTIVE。
    pub fn GetExitCodeProcess(h_process: Handle, lp_exit_code: *mut u32) -> i32;
    /// 关闭句柄。
    pub fn CloseHandle(h_object: Handle) -> i32;
}
