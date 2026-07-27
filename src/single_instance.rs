//! 单实例守护。
//!
//! 双开的代价不是“多开一个窗口”这么轻：RequestGovernor 的限流冷却、
//! 熔断与 429 历史只在进程内按 origin 共享，第二个进程等于把全部网络
//! 治理翻倍绕过；两个进程还会互相覆盖配置文件与会话状态。因此重复启动
//! 一律拦下，并把已有窗口叫到前台。

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONINFORMATION, MB_OK,
        SW_RESTORE,
    };

    /// `Local\` 前缀让守护限定在当前登录会话：同一台机器上不同 Windows
    /// 用户各自可以跑一份，同一用户双开则被拦。
    const MUTEX_NAME: &str = r"Local\Course-snatching-single-instance";

    /// 持有单实例锁。必须活到进程结束——提前 drop 就等于放开了锁。
    pub struct Guard(HANDLE);

    impl Drop for Guard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 句柄来自 CreateMutexW，且只在此处关闭一次。
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 抢占单实例锁。`None` 表示已有实例在运行，调用方应立即退出。
    pub fn acquire() -> Option<Guard> {
        acquire_named(MUTEX_NAME)
    }

    /// 名称可注入，测试才能用独立的锁名验证真实实现，而不会被用户机器上
    /// 真的开着一份程序干扰。
    fn acquire_named(mutex_name: &str) -> Option<Guard> {
        let name = wide(mutex_name);
        // SAFETY: name 是以 NUL 结尾的合法 UTF-16 缓冲且在调用期间存活；
        // 空属性指针表示默认安全属性。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        // GetLastError 必须紧跟调用读取，中间不能插入任何其他 Win32 调用。
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if handle.is_null() {
            // 极少见：拿不到互斥体不该让用户没法用程序，放行且不再守护。
            return Some(Guard(std::ptr::null_mut()));
        }
        if already_running {
            // 自己这份句柄要显式关掉，否则会延长锁的存活期。
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Guard(handle))
    }

    /// 把已有实例的窗口叫到前台；叫不动才弹提示，避免窗口已经在眼前还多一个框。
    pub fn focus_existing_and_notify(window_title: &str) {
        let title = wide(window_title);
        // SAFETY: 类名传空表示不限定窗口类；title 为 NUL 结尾缓冲。
        let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if !hwnd.is_null() {
            // SAFETY: hwnd 由 FindWindowW 返回，非空即为有效窗口句柄。
            let raised = unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd)
            };
            // 跨进程置前台可能被系统拒绝，此时退回提示框。
            if raised != 0 {
                return;
            }
        }
        let text = wide("Course-snatching 已在运行，请使用已打开的那个窗口。");
        let caption = wide("Course-snatching");
        // SAFETY: 两个缓冲均为 NUL 结尾且在调用期间存活；空父窗口句柄合法。
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            )
        };
    }

    #[cfg(test)]
    mod tests {
        use super::acquire_named;

        // 守护的全部价值在于“第二次必须拿不到”。同名互斥体在同一进程内
        // 再次创建也会报 ERROR_ALREADY_EXISTS，因此可以直接测生产实现。
        #[test]
        fn second_instance_is_refused_until_the_holder_drops() {
            let name = format!(r"Local\Course-snatching-test-{}", std::process::id());
            let first = acquire_named(&name).expect("first instance must win the lock");
            assert!(
                acquire_named(&name).is_none(),
                "second instance must be refused while the first holds the lock"
            );
            drop(first);
            // 持有者退出后锁必须真的放开，否则崩溃/重启后程序再也起不来。
            let again = acquire_named(&name);
            assert!(again.is_some(), "lock must be released with its guard");
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows 平台不提供守护（本程序只面向 Windows 发布）。
    pub struct Guard;

    pub fn acquire() -> Option<Guard> {
        Some(Guard)
    }

    pub fn focus_existing_and_notify(_window_title: &str) {}
}

pub use imp::{acquire, focus_existing_and_notify};
