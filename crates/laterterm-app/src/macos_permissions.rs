//! macOS 权限探测与系统设置深链（设置面板「权限」节用）。
//!
//! 只读探测、不弹窗；「去授权」按钮深链到对应的系统设置面板，由用户手动
//! 勾选（与 LaterKey 同口径：状态即时查询，授权动作引导而非代办）。
//!
//! ListenEvent 的查询 API（CGPreflightListenEventAccess）在旧系统上不存在——
//! 直接 extern 会让 dyld 在老 macOS 启动即崩，因此经 dlsym 弱探测：取不到
//! 符号按「本系统无此项」处理，不误报未授权。辅助功能查询
//! （AXIsProcessTrusted）自 macOS 10.4 就有，直接 extern 安全。

/// 辅助功能 API 是否可用（全局热键、读窗口等自动化能力的门槛）。
pub fn accessibility_trusted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }
    // SAFETY: 无副作用的只读系统查询
    unsafe { AXIsProcessTrusted() != 0 }
}

/// ListenEvent（监听事件）权限状态：`None` = 本系统没有这项独立权限
/// （macOS < 15，无需授权），`Some(granted)` = 有且是否已授权。
pub fn listen_event_access() -> Option<bool> {
    const RTLD_LAZY: std::ffi::c_int = 1;
    unsafe extern "C" {
        fn dlopen(
            filename: *const std::ffi::c_char,
            flag: std::ffi::c_int,
        ) -> *mut std::ffi::c_void;
        fn dlsym(
            handle: *mut std::ffi::c_void,
            symbol: *const std::ffi::c_char,
        ) -> *mut std::ffi::c_void;
        fn dlclose(handle: *mut std::ffi::c_void) -> std::ffi::c_int;
    }
    // SAFETY: 固定系统框架路径 + 固定符号名；句柄成对 dlclose，无泄漏
    unsafe {
        let handle = dlopen(
            c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics".as_ptr(),
            RTLD_LAZY,
        );
        if handle.is_null() {
            return None;
        }
        let sym = dlsym(handle, c"CGPreflightListenEventAccess".as_ptr());
        let result = if sym.is_null() {
            None
        } else {
            let preflight: extern "C" fn() -> u8 = std::mem::transmute(sym);
            Some(preflight() != 0)
        };
        dlclose(handle);
        result
    }
}

/// 系统设置各隐私面板的深链 URL（`open` 直接吃）。完全磁盘访问沿用本仓库
/// 已验证的新式锚点，其余两块用 LaterKey 真机验证过的旧式锚点。
pub const ACCESSIBILITY_PANE: &str = "com.apple.preference.security?Privacy_Accessibility";
pub const LISTEN_EVENT_PANE: &str = "com.apple.preference.security?Privacy_ListenEvent";
pub const ALL_FILES_PANE: &str = "com.apple.settings.privacy?Privacy_AllFiles_AllFiles";

/// 深链打开一个系统设置面板。
pub fn open_privacy(pane: &str) {
    let _ = std::process::Command::new("open")
        .arg(format!("x-apple.systempreferences:{pane}"))
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机冒烟：两条查询都能跑通（结果机器相关，不断言授权状态）。
    #[test]
    fn permission_queries_run() {
        eprintln!(
            "辅助功能={}",
            if accessibility_trusted() {
                "已授权"
            } else {
                "未授权"
            }
        );
        match listen_event_access() {
            Some(granted) => eprintln!("监听事件={}", if granted { "已授权" } else { "未授权" }),
            None => eprintln!("监听事件=本系统无此项"),
        }
        // 深链 URL 至少是 open 能接的形态（不真开面板，只验证拼接）
        assert!(ACCESSIBILITY_PANE.starts_with("com.apple"));
    }
}
