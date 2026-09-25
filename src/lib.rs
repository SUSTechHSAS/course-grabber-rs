//! 教务系统抢课 · 放课窗口精准首发（Rust 版）
//!
//! 这里只有模块声明：真正的入口在 `src/main.rs`（`course-grabber` 这个可执行文件）。
//! 做成 lib + bin 两件套的理由只有一个 —— **自检要能引用内部模块**：
//! `tests/offline.rs` 用假学校服务端驱动真实的登录/重试/复核代码，
//! 那正是原版 `tests/test_offline.py` 干的事。
//!
//! 各模块与原版 Python 文件的对应关系：
//!
//! | 这里 | 原版 |
//! | --- | --- |
//! | `config.rs` | `school_config.py` —— 一切"学校相关"的东西都从 config.json 读 |
//! | `auth.rs` | `school_auth.py` —— 自动登录 / 被踢重登录 |
//! | `grab.rs` | `grab.py` —— 预检、对时、首发、重试循环、复核 |
//! | `des.rs` | `desencode.py` + `cus_base64.py` —— 密码加密 |
//! | `captcha.rs` | 原版的 `python/solver.py` + `libccm.so`（现在是编译进来的依赖） |
//!
//! 原版里还有、这里**没有**的东西：
//! * PyInstaller 打包脚本（不再需要：`cargo build --release` 就是一个文件）
//! * `sys.path` 注入、按目录加载 solver.py、换解释器重跑那一套（动态库没了）
//! * 读 Chrome Cookie 那条回退路径（会话来源只剩自动登录与显式给的 `--cookie`/`--url`）
//! * Windows 上强制 UTF-8 输出（Rust 本来就不会因此崩）

pub mod auth;
pub mod captcha;
pub mod cli;
pub mod clock;
pub mod config;
pub mod des;
pub mod grab;
pub mod httpc;
pub mod json;
pub mod lock;
pub mod log;
pub mod pacer;
pub mod timeutil;
