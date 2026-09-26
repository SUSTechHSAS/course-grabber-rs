//! 终端后端：raw mode、按键解码、整帧输出。
//!
//! 为什么不用 crossterm/ratatui：这个项目的卖点是**一个文件、零第三方运行时**，
//! 而那两家加起来要拖进十来个 crate 和几百 KB —— 这正是 `Cargo.toml` 里
//! 连 clap、serde_json 都没要的原因。终端这一层真正需要的只有四件事：
//! `tcgetattr/tcsetattr`、`ioctl(TIOCGWINSZ)`、`poll`、`read`，
//! 前两个 libc 里现成（`lock.rs` 已经在用它够得着的那部分），所以自己写反而更合身。
//!
//! 平台范围：Linux / macOS（`cfg(unix)`）。Windows 的控制台要 `SetConsoleMode`
//! 那一套 Win32 API，libc 里没有，为一个平台引一整份 FFI（而且在这台机器上没法验证）
//! 不划算 —— 那边 `enter()` 直接返回一句"请直接编辑 config.json"。
//!
//! 退出时的终端复原走三条路，缺一条用户就会得到一个不回显、不换行的 shell：
//! 1. 正常退出 / 错误退出 → `Drop`；
//! 2. panic → 自定义 panic hook（release 是 `panic=abort`，`Drop` 不会跑）；
//! 3. `kill`（SIGTERM/SIGHUP/SIGINT/SIGQUIT）→ 信号处理函数里直接还原并 `_exit`。

use std::collections::VecDeque;
use std::io::{Read, Write};

/// 一个按键（或一段粘贴）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// 括号粘贴模式下一次性到达的一整段文本
    Paste(String),
    Enter,
    Tab,
    BackTab,
    Esc,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Ctrl + 字母（小写）
    Ctrl(char),
    /// stdin 关掉了（管道读完）—— 应用层该退出，否则会空转
    Eof,
    /// 认不出来的转义序列，忽略即可
    Unknown,
}

/// 进入界面时发给终端的控制序列：备用屏幕 + 隐藏光标 + 括号粘贴 + 关闭自动换行。
#[cfg(unix)]
const ENTER_SEQ: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[?7l";
/// 复原：恢复自动换行 + 关括号粘贴 + 显示光标 + 回主屏幕。
const LEAVE_SEQ: &[u8] = b"\x1b[?7h\x1b[?2004l\x1b[?25h\x1b[?1049l";

pub fn is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// 终端尺寸（列, 行）。拿不到就给 80×24。
pub fn size() -> (usize, usize) {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }
    let env = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(d)
    };
    (env("COLUMNS", 80), env("LINES", 24))
}

/// 进入 raw mode 并把终端切到备用屏幕。
pub fn enter() -> Result<Term, String> {
    #[cfg(not(unix))]
    {
        return Err(
            "交互式配置界面只在 Linux/macOS 上提供。Windows 请直接编辑 config.json\
             （第一次运行会在程序旁边生成一份模板）。"
                .to_string(),
        );
    }
    #[cfg(unix)]
    {
        if !is_tty() {
            return Err(
                "需要一个真正的终端（当前 stdin/stdout 不是 tty，可能是重定向或管道）。\n\
                 在交互式终端里直接跑 `course-grabber tui`；只想看/改文件的话直接编辑 config.json 也一样。"
                    .to_string(),
            );
        }
        let orig = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err("读不到终端设置（tcgetattr 失败）".to_string());
            }
            t
        };
        unsafe {
            std::ptr::write(
                std::ptr::addr_of_mut!(ORIG),
                std::mem::MaybeUninit::new(orig),
            )
        };
        install_panic_hook();
        // 被 kill 掉也要把终端还给用户
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
        }
        ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut raw = orig;
        unsafe {
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSAFLUSH, &raw) != 0 {
                ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
                return Err("进不了 raw mode（tcsetattr 失败）".to_string());
            }
        }
        write_out(ENTER_SEQ);
        Ok(Term {
            pending: VecDeque::new(),
            buf: Vec::new(),
            pasting: false,
        })
    }
}

// ---------------------------------------------------------------------------
// 退出时还原终端
// ---------------------------------------------------------------------------

#[cfg(unix)]
static mut ORIG: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();

#[cfg(unix)]
static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 把终端还原。可以重复调用；信号处理函数里也走它，所以只用异步信号安全的调用。
#[cfg(unix)]
unsafe fn restore_now() {
    use std::sync::atomic::Ordering;
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    libc::tcsetattr(
        0,
        libc::TCSAFLUSH,
        std::ptr::addr_of!(ORIG).cast::<libc::termios>(),
    );
    let _ = libc::write(1, LEAVE_SEQ.as_ptr().cast(), LEAVE_SEQ.len());
}

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    unsafe {
        restore_now();
        libc::_exit(128 + sig);
    }
}

/// panic 也要还原（release 用 `panic=abort`，析构函数不会跑）。
#[cfg(unix)]
fn install_panic_hook() {
    static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ONCE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        #[cfg(unix)]
        unsafe {
            restore_now();
        }
        prev(info);
    }));
}

pub struct Term {
    pending: VecDeque<Key>,
    /// 还没解析完的输入字节（转义序列和 UTF-8 都可能被 read 切开）
    buf: Vec<u8>,
    pasting: bool,
}

impl Term {
    /// 取一个按键；`timeout_ms` 内没有输入就返回 `None`（用来做重绘/响应尺寸变化）。
    pub fn next_key(&mut self, timeout_ms: i32) -> Option<Key> {
        loop {
            if let Some(k) = self.pending.pop_front() {
                return Some(k);
            }
            if self.buf.is_empty() {
                if !wait_readable(timeout_ms) {
                    return None;
                }
                if !self.fill() {
                    return Some(Key::Eof);
                }
                continue;
            }
            self.decode();
            if let Some(k) = self.pending.pop_front() {
                return Some(k);
            }
            // 剩下的要么是孤立的 ESC，要么是个没读完的序列：再等一小会儿。
            if !wait_readable(30) {
                if self.buf == [0x1b] {
                    self.buf.clear();
                    return Some(Key::Esc);
                }
                self.buf.clear();
                return None;
            }
            if !self.fill() {
                return Some(Key::Eof);
            }
        }
    }

    /// 覆盖整个屏幕：先回左上角并藏起光标，每行末尾擦到行尾。
    pub fn draw(&mut self, frame: &str) {
        let mut out = String::with_capacity(frame.len() + 32);
        out.push_str("\x1b[H\x1b[?25l");
        let mut lines = frame.split('\n');
        if let Some(first) = lines.next() {
            out.push_str(first);
            out.push_str("\x1b[K");
        }
        for l in lines {
            out.push_str("\r\n");
            out.push_str(l);
            out.push_str("\x1b[K");
        }
        write_out(out.as_bytes());
    }

    /// 把硬件光标放到 `(row, col)`（都是 1 起）并显示出来 —— 编辑输入框时用。
    pub fn show_cursor_at(&mut self, row: usize, col: usize) {
        write_out(format!("\x1b[{};{}H\x1b[?25h", row, col).as_bytes());
    }

    pub fn hide_cursor(&mut self) {
        write_out(b"\x1b[?25l");
    }

    pub fn bell(&mut self) {
        write_out(b"\x07");
    }

    fn push(&mut self, k: Key) {
        self.pending.push_back(k);
    }

    /// 读一批字节。返回 false 表示 stdin 没了。
    fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 1024];
        match std::io::stdin().read(&mut chunk) {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    /// 尽量把 `buf` 头部的完整序列解析成按键；解析不了（数据不完整）就留给下一次。
    fn decode(&mut self) {
        while !self.buf.is_empty() {
            if self.pasting {
                if !self.decode_paste() {
                    return;
                }
                continue;
            }
            let b = self.buf[0];
            match b {
                0x1b => {
                    if self.buf.len() < 2 {
                        return; // 可能只是按了 Esc，交给 next_key 的超时判定
                    }
                    match self.buf[1] {
                        b'[' => {
                            let mut j = 2;
                            while j < self.buf.len() && !(0x40..=0x7e).contains(&self.buf[j]) {
                                j += 1;
                            }
                            if j >= self.buf.len() {
                                return; // CSI 还没收完
                            }
                            let seq: Vec<u8> = self.buf.drain(..=j).collect();
                            let k = csi_key(&seq);
                            self.push(k);
                        }
                        b'O' => {
                            if self.buf.len() < 3 {
                                return;
                            }
                            let c = self.buf[2];
                            self.buf.drain(..3);
                            self.push(match c {
                                b'A' => Key::Up,
                                b'B' => Key::Down,
                                b'C' => Key::Right,
                                b'D' => Key::Left,
                                b'H' => Key::Home,
                                b'F' => Key::End,
                                _ => Key::Unknown,
                            });
                        }
                        _ => {
                            // Alt+键：当成 Esc + 那个键
                            self.buf.remove(0);
                            self.push(Key::Esc);
                        }
                    }
                }
                b'\r' | b'\n' => {
                    self.buf.remove(0);
                    self.push(Key::Enter);
                }
                b'\t' => {
                    self.buf.remove(0);
                    self.push(Key::Tab);
                }
                0x7f | 0x08 => {
                    self.buf.remove(0);
                    self.push(Key::Backspace);
                }
                0x00 => {
                    self.buf.remove(0);
                    self.push(Key::Ctrl('@'));
                }
                c @ 0x01..=0x1a => {
                    self.buf.remove(0);
                    self.push(Key::Ctrl((b'a' + c - 1) as char));
                }
                _ => {
                    let len = utf8_len(b);
                    if len == 0 {
                        self.buf.remove(0); // 非法字节，丢掉
                        continue;
                    }
                    if self.buf.len() < len {
                        return; // 多字节字符被切开了
                    }
                    let bytes: Vec<u8> = self.buf.drain(..len).collect();
                    if let Some(c) = std::str::from_utf8(&bytes)
                        .ok()
                        .and_then(|s| s.chars().next())
                    {
                        self.push(Key::Char(c));
                    }
                }
            }
        }
    }

    /// 括号粘贴模式下的内容解析；返回 false 表示数据还没收完。
    fn decode_paste(&mut self) -> bool {
        const END: &[u8] = b"\x1b[201~";
        if let Some(p) = find_sub(&self.buf, END) {
            let content: Vec<u8> = self.buf.drain(..p).collect();
            self.buf.drain(..END.len());
            self.pasting = false;
            if !content.is_empty() {
                self.push(Key::Paste(String::from_utf8_lossy(&content).into_owned()));
            }
            return true;
        }
        // 还没见到终止符：把已经确定不是终止符前缀的部分交出去
        let keep = partial_suffix(&self.buf, END);
        let mut n = self.buf.len() - keep;
        // 别把一个多字节字符劈成两半
        while n > 0 && std::str::from_utf8(&self.buf[..n]).is_err() {
            n -= 1;
        }
        if n == 0 {
            return false;
        }
        let content: Vec<u8> = self.buf.drain(..n).collect();
        self.push(Key::Paste(String::from_utf8_lossy(&content).into_owned()));
        false
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            restore_now();
        }
        #[cfg(not(unix))]
        write_out(LEAVE_SEQ);
    }
}

fn write_out(bytes: &[u8]) {
    let mut out = std::io::stdout();
    // 终端没了（EPIPE）也当写成功：这时候用户的目标已经不在屏幕上了
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

#[cfg(unix)]
fn wait_readable(ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // EINTR（比如窗口大小变化）当作"没输入"，让上层重绘一帧
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 && (pfd.revents & libc::POLLIN) != 0 }
}

#[cfg(not(unix))]
fn wait_readable(_ms: i32) -> bool {
    false
}

/// 解析 `ESC [ ... <终止字节>`。修饰键（`1;5A` 之类）一律当裸方向键处理 ——
/// 这个界面不给方向键配别的含义。
fn csi_key(seq: &[u8]) -> Key {
    let final_byte = match seq.last() {
        Some(b) => *b,
        None => return Key::Unknown,
    };
    let params = std::str::from_utf8(&seq[2..seq.len() - 1]).unwrap_or("");
    match final_byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'Z' => Key::BackTab,
        b'~' => match params {
            "1" | "7" => Key::Home,
            "2" => Key::Unknown, // Insert
            "3" => Key::Delete,
            "4" | "8" => Key::End,
            "5" => Key::PageUp,
            "6" => Key::PageDown,
            _ => Key::Unknown,
        },
        _ => Key::Unknown,
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        0
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// `buf` 末尾有多少字节可能是 `needle` 的前缀（这些要留着等下一批数据）。
fn partial_suffix(buf: &[u8], needle: &[u8]) -> usize {
    let max = needle.len().min(buf.len());
    (1..max)
        .rev()
        .find(|&k| buf[buf.len() - k..] == needle[..k])
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn term_with(bytes: &[u8]) -> Term {
        Term {
            pending: VecDeque::new(),
            buf: bytes.to_vec(),
            pasting: false,
        }
    }

    fn keys_of(bytes: &[u8]) -> Vec<Key> {
        let mut t = term_with(bytes);
        t.decode();
        std::mem::take(&mut t.pending).into_iter().collect()
    }

    #[test]
    fn decodes_arrows_and_specials() {
        assert_eq!(keys_of(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(
            keys_of(b"\x1b[B\x1b[C\x1b[D"),
            vec![Key::Down, Key::Right, Key::Left]
        );
        assert_eq!(
            keys_of(b"\x1b[3~\x1b[5~\x1b[6~"),
            vec![Key::Delete, Key::PageUp, Key::PageDown]
        );
        assert_eq!(keys_of(b"\x1b[1;5A"), vec![Key::Up]); // 带修饰键
        assert_eq!(keys_of(b"\x1bOH\x1bOF"), vec![Key::Home, Key::End]);
        assert_eq!(keys_of(b"\x1b[Z"), vec![Key::BackTab]);
    }

    #[test]
    fn decodes_text_and_ctrl() {
        assert_eq!(
            keys_of(b"ab\r"),
            vec![Key::Char('a'), Key::Char('b'), Key::Enter]
        );
        assert_eq!(keys_of(b"\x03\x13"), vec![Key::Ctrl('c'), Key::Ctrl('s')]);
        assert_eq!(keys_of(b"\x7f"), vec![Key::Backspace]);
        assert_eq!(keys_of("课".as_bytes()), vec![Key::Char('课')]);
    }

    #[test]
    fn incomplete_input_waits_for_more() {
        // 半截转义序列：先不产出按键，等后续字节到了再解析
        let mut t = term_with(b"\x1b[");
        t.decode();
        assert!(t.pending.is_empty());
        t.buf.extend_from_slice(b"A");
        t.decode();
        assert_eq!(t.pending.pop_front(), Some(Key::Up));

        // 被切开的 UTF-8 也一样
        let mut t = term_with(&"课".as_bytes()[..2]);
        t.decode();
        assert!(t.pending.is_empty());
        t.buf.extend_from_slice(&"课".as_bytes()[2..]);
        t.decode();
        assert_eq!(t.pending.pop_front(), Some(Key::Char('课')));
    }

    #[test]
    fn lone_esc_stays_in_buffer_for_timeout() {
        let mut t = term_with(b"\x1b");
        t.decode();
        assert!(t.pending.is_empty());
        assert_eq!(t.buf, vec![0x1b]);
    }

    #[test]
    fn bracketed_paste_is_one_key() {
        // 粘贴权限只在 csi 解析里打开，这里直接构造"已经进入粘贴模式"的状态
        let mut t = term_with("前\x1b[201~".as_bytes());
        t.pasting = true;
        t.decode();
        assert_eq!(t.pending.pop_front(), Some(Key::Paste("前".to_string())));
        assert!(!t.pasting);
    }

    #[test]
    fn paste_split_across_reads_keeps_bytes_back() {
        // 终止符只到了一半，不能把它当成内容吐出去
        let mut t = term_with(b"abc\x1b[20");
        t.pasting = true;
        t.decode();
        assert_eq!(t.pending.pop_front(), Some(Key::Paste("abc".to_string())));
        assert_eq!(t.buf, b"\x1b[20".to_vec());
        t.buf.extend_from_slice(b"1~");
        t.decode();
        assert!(t.pending.is_empty());
        assert!(!t.pasting);
    }

    #[test]
    fn partial_suffix_finds_terminator_prefix() {
        assert_eq!(partial_suffix(b"abc\x1b[20", b"\x1b[201~"), 4);
        assert_eq!(partial_suffix(b"abc", b"\x1b[201~"), 0);
        assert_eq!(partial_suffix(b"", b"\x1b[201~"), 0);
    }
}
