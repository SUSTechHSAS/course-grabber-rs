//! 输出。
//!
//! 原版 Python 在 `sys.platform == "win32"` 时要手动把 stdout 重配成 UTF-8
//! （否则中文直接 `UnicodeEncodeError` 崩掉）。Rust 的 `println!` 写的是 UTF-8
//! 字节、并且 Windows 上会走 `WriteConsoleW`，所以这里**不需要**那段黑客代码 ——
//! 这也是"重写"顺带消掉的一类平台坑。

use crate::timeutil;

/// 直接打印一行（原版的 `log()`）。
///
/// 用 `println!` 而不是直接写 `std::io::stdout()`：Rust 的 stdout 是行缓冲，
/// 每次换行都会刷出去（等价于原版的 `flush=True`），而且这样 `cargo test`
/// 能正常捕获输出 —— 自检会打很多日志，不该把测试输出淹掉。
pub fn log(msg: &str) {
    println!("{msg}");
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
