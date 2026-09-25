//! 单实例锁。
//!
//! 2026-09-25 实测：学校对**同一账号**只允许一个有效会话 —— 连续登录 A→B→C，
//! 前一个立刻失效。所以跑两个实例（或者脚本跑着的时候去浏览器刷新页面）会互相
//! 把对方顶掉，两边都触发自动重登录、再互相顶，放课瞬间全废。
//! 这里用一个 PID 锁文件把"同一台机器上跑两个实例"直接拦下来。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::log;

pub struct InstanceLock {
    path: PathBuf,
    held: bool,
}

/// 锁文件路径的 C 字符串副本，只为让**信号处理函数**能直接 `unlink`。
///
/// 为什么需要它：原版 Python 在只读阶段收到 Ctrl-C 会走 `atexit` 把锁删掉；
/// 而我们那边是信号处理函数里 `_exit(130)`（异步信号安全的要求：不能分配、不能加锁），
/// 析构函数不会跑。于是就留一个残留锁文件 —— 虽然下次运行会把它当"进程已死"忽略掉，
/// 但没必要留这个垃圾。`unlink` 本身是异步信号安全的，缺的只是一个写死的路径。
#[cfg(unix)]
static LOCK_PATH_FOR_SIGNAL: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

/// 记住锁文件路径（取锁成功时调用一次）。
#[cfg(unix)]
pub fn remember_for_signal_cleanup(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        let _ = LOCK_PATH_FOR_SIGNAL.set(c);
    }
}

/// 在信号处理函数里删掉锁文件（只做一次 unlink，不加锁、不分配）。
///
/// 安全性：只在锁是我们自己写的那份时删？—— 信号处理函数里读不了文件内容，
/// 所以这里是无条件 unlink。能接受的理由是：这条路径只在"本进程确实持锁"之后
/// 才可能发生（路径是在取锁成功时才记进来的）。
#[cfg(unix)]
pub fn remove_lock_in_signal_handler() {
    if let Some(p) = LOCK_PATH_FOR_SIGNAL.get() {
        unsafe {
            libc::unlink(p.as_ptr());
        }
    }
}

impl InstanceLock {
    /// 取锁。返回 `None` 表示已经有一个实例在跑（消息已经打印出来）。
    pub fn acquire(path: &Path, force: bool) -> Option<InstanceLock> {
        if !force && path.exists() {
            let pid = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| t.trim().parse::<i64>().ok())
                .unwrap_or(0);
            // 只有"PID 是别人、且确实是我们这个程序的实例"才拦。
            // （自己持有自己写的锁不算冲突 —— 例如将来再有重启自己的逻辑。）
            if pid != std::process::id() as i64 && pid_is_our_instance(pid) {
                log::log(&format!(
                    "✗ 检测到已经有一个实例在跑（PID {pid}，锁文件 {}）",
                    path.display()
                ));
                log::log("  学校对同一账号只允许一个有效会话：再跑一个会把那个顶掉，");
                log::log("  两个进程互相踢、互相重登录，放课瞬间两边都抢不到。");
                log::log("  确认那个进程已经没用了再加 --force，或先 kill 掉它。");
                return None;
            }
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::File::create(path).and_then(|mut f| {
            write!(f, "{}", std::process::id())?;
            f.sync_all()
        }) {
            Ok(()) => {
                #[cfg(unix)]
                remember_for_signal_cleanup(path);
                Some(InstanceLock {
                    path: path.to_path_buf(),
                    held: true,
                })
            }
            // 锁文件写不了就别拦着用户
            Err(_) => Some(InstanceLock {
                path: path.to_path_buf(),
                held: false,
            }),
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        // 只删自己写的那份（别人可能已经接手了）
        let mine = std::process::id().to_string();
        if let Ok(text) = std::fs::read_to_string(&self.path) {
            if text.trim() == mine {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// 判断锁文件里的 PID 是不是**本程序**的实例，而不是恰好复用了同一 PID 的无关进程。
///
/// 踩过的坑（原版注释里也记着）：容器/系统里 PID 5 往往是常驻进程，只判断"PID 存活"
/// 会让锁永远解不开，用户只会看到"已经有一个实例在跑"却怎么也找不到那个进程。
#[cfg(unix)]
fn pid_is_our_instance(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // kill(pid, 0)：不发信号，只探活（与原版 os.kill(pid, 0) 一致）
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EPERM) => true, // 存在但不属于我们，按存在处理
            _ => false,                // ESRCH / 其它：当它已经没了
        };
    }
    // Linux 上还能看 cmdline，认一认它到底是不是我们这类程序
    let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        // 读不到（macOS 没有 /proc、或没权限）就只能相信 PID
        return true;
    };
    let cmd = String::from_utf8_lossy(&cmd);
    // 认这几个关键字：本程序的名字、原版 python 脚本、以及仓库里的探针工具
    ["course-grabber", "grab", "probe", "python"]
        .iter()
        .any(|k| cmd.contains(k))
}

/// Windows 上没有便宜的"这个 PID 还活着吗"（要 OpenProcess + 句柄，那是另一套
/// 绑定）。这里**保守地当作活着**：宁可让用户看到"已经有一个实例在跑"并加
/// `--force`，也不要在没跑的时候放过去 —— 同一账号两个实例互相踢的代价更大。
#[cfg(not(unix))]
fn pid_is_our_instance(pid: i64) -> bool {
    pid > 0 && pid <= i32::MAX as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_our_own_process() {
        // 当前进程显然是"活着的自己"（cmdline 里也有 course-grabber）
        assert!(pid_is_our_instance(std::process::id() as i64));
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_counts_as_alive() {
        // PID 1 属于别的用户：kill 回 EPERM → 按"存在"处理。
        // 与原版一致：宁可拦住用户，也不要把别人持着的锁当成失效的。
        if std::path::Path::new("/proc/1").exists() && unsafe { libc::kill(1, 0) } != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error();
            if errno == Some(libc::EPERM) {
                assert!(pid_is_our_instance(1));
            }
        }
    }

    #[test]
    fn rejects_bogus_pids() {
        assert!(!pid_is_our_instance(0));
        assert!(!pid_is_our_instance(-1));
        // 一个几乎不可能存在的 PID
        assert!(!pid_is_our_instance(0x7fff_fffe));
    }

    #[test]
    fn lock_file_lifecycle() {
        let dir = std::env::temp_dir().join(format!("cg-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.lock");
        let _ = std::fs::remove_file(&path);

        let lock = InstanceLock::acquire(&path, false).unwrap();
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(lock);
        assert!(!path.exists(), "drop 之后锁文件应当被删掉");

        // 别人的锁（一个几乎不可能存在的 PID）不该拦我们
        std::fs::write(&path, "2147483646").unwrap();
        assert!(InstanceLock::acquire(&path, false).is_some());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
