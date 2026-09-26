//! 输出。
//!
//! 原版 Python 在 `sys.platform == "win32"` 时要手动把 stdout 重配成 UTF-8
//! （否则中文直接 `UnicodeEncodeError` 崩掉）。Rust 的 `println!` 写的是 UTF-8
//! 字节、并且 Windows 上会走 `WriteConsoleW`，所以这里**不需要**那段黑客代码 ——
//! 这也是"重写"顺带消掉的一类平台坑。

use std::cell::RefCell;

use crate::timeutil;

/// 日志出口：收到一行就交出去（TUI 用它把日志搬进自己的面板）。
type Sink = Box<dyn Fn(&str)>;

thread_local! {
    /// 当前线程的日志出口。设置之后 `log()` 不再往 stdout 打，而是交给它。
    ///
    /// TUI 里跑联网任务（登录 / 拉课程目录）时需要这个：那些代码是用 `info()`
    /// 汇报进度的，直接 `println!` 会把输出喷在界面中间，而用户其实很需要看到
    /// 「[auth] 验证码 1234 margin=0.99（第 1/6 张）」这类过程。收进缓冲区之后，
    /// 界面就能把它们当成自己的日志面板来画。
    ///
    /// 做成 thread-local（而不是全局锁）是因为：任务跑在独立线程上，日志天然只属于
    /// 那个线程；也不必担心两个任务互相污染对方的输出。
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// 把 `body()` 期间本线程的日志交给 `sink`（不再进 stdout）。
///
/// 线程结束或 panic 时不用管：下一次调用会覆盖它，而线程本身是新的。
pub fn with_sink<R>(sink: impl Fn(&str) + 'static, body: impl FnOnce() -> R) -> R {
    SINK.with(|s| *s.borrow_mut() = Some(Box::new(sink)));
    let out = body();
    SINK.with(|s| *s.borrow_mut() = None);
    out
}

/// 直接打印一行（原版的 `log()`）。
///
/// 用 `println!` 而不是直接写 `std::io::stdout()`：Rust 的 stdout 是行缓冲，
/// 每次换行都会刷出去（等价于原版的 `flush=True`），而且这样 `cargo test`
/// 能正常捕获输出 —— 自检会打很多日志，不该把测试输出淹掉。
pub fn log(msg: &str) {
    let handled = SINK.with(|s| match &*s.borrow() {
        Some(sink) => {
            sink(msg);
            true
        }
        None => false,
    });
    if !handled {
        println!("{msg}");
    }
}

/// 带 `[HH:MM:SS.mmm]` 前缀（原版的 `info()`）。
pub fn info(msg: &str) {
    log(&format!("[{}] {msg}", timeutil::ts()));
}

/// 空行
pub fn blank() {
    log("");
}

/// 一条横线/感叹号分隔线，宽度与原版一致。
pub fn rule(ch: char) {
    log(&ch.to_string().repeat(72));
}
