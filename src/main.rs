//! `course-grabber` 可执行文件的入口。
//!
//! 逻辑都在 lib（`src/lib.rs` 里列的各模块）里；这里只负责三件"进程级"的事：
//! SIGPIPE、解析命令行、把退出码翻译给操作系统。

use std::process::ExitCode;

use course_grabber::{cli, grab, log};

fn main() -> ExitCode {
    // `course-grabber --offline | head` 这种用法会把管道提前关掉。
    // Rust 默认忽略 SIGPIPE，于是写 stdout 只会拿到 EPIPE —— 恢复成默认动作，
    // 进程安静地退出，而不是打一串刺眼的错误（原版是捕获 BrokenPipeError）。
    //
    // 只在 unix 上做：Windows 的 CRT 里根本没有 SIGPIPE 这个东西（管道的语义也不同）。
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(cli::Parsed::Run(args)) => *args,
        Ok(cli::Parsed::Print(text)) => {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprint!("{msg}");
            return ExitCode::from(2);
        }
    };

    match grab::main(args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            log::log(&format!("\n✗ {e}"));
            ExitCode::from(2)
        }
    }
}
