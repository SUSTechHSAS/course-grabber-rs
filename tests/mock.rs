//! 假学校服务端：只说这套工具真正用到的那几句话，跑在 127.0.0.1 的临时端口上。
//!
//! 对应原版 `tests/test_offline.py` 里的 `_MockSchool`。目的不是"模拟一所完整的学校"，
//! 而是让**真实的代码路径**（真实的 TCP、真实的 HTTP 解析、真实的分类与重试循环）
//! 跑起来，这样协议、策略、状态机上的错都会被抓住。

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 一张"够用"的假 JPEG：假服务端只校验魔数（起始三个字节必须是 ff d8 ff），
/// 桩 solver 不看内容。这样整套自检不需要真图，也不需要真模型。
pub const FAKE_JPEG: &[u8] = b"\xff\xd8\xff\xe0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\xff\xd9";

#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub form: Vec<(String, String)>,
    pub cookie: String,
}

impl Call {
    pub fn field(&self, name: &str) -> String {
        self.form
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// 一次 login.do 的答复。
#[derive(Clone, Debug)]
pub enum LoginReply {
    Ok,
    Code(&'static str, &'static str),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 正常：业务接口一律 code=1
    Normal,
    /// 业务接口要求 `JSESSIONID=S1`，否则回 302（用来测"只读自愈 / 写不重放"）
    GuardSession,
    /// 放课前后学校重排数据：容量返回 0/0，写请求一律被拒（真实事故的复现）
    InitOutage,
    /// 用分块编码（Transfer-Encoding: chunked）回响应 —— Java 后端很可能会这么回，
    /// 而我们这套 HTTP 读取器是自己写的，必须验证分块能正确读完
    Chunked,
}

pub struct Mock {
    pub port: u16,
    pub calls: Arc<Mutex<Vec<Call>>>,
    /// 写请求（volunteer.do）到达的时刻，相对 `t0`
    pub writes: Arc<Mutex<Vec<f64>>>,
    pub t0: Instant,
    script: Arc<Mutex<VecDeque<LoginReply>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Mock {
    pub fn start(mode: Mode, script: Vec<LoginReply>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));
        let stop = Arc::new(AtomicBool::new(false));
        let t0 = Instant::now();

        let accept = {
            let calls = Arc::clone(&calls);
            let writes = Arc::clone(&writes);
            let script = Arc::clone(&script);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let calls = Arc::clone(&calls);
                            let writes = Arc::clone(&writes);
                            let script = Arc::clone(&script);
                            std::thread::spawn(move || {
                                serve(stream, mode, calls, writes, script, t0)
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Mock {
            port,
            calls,
            writes,
            t0,
            script,
            stop,
            accept: Some(accept),
        }
    }

    pub fn calls_to(&self, suffix: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.path.ends_with(suffix))
            .cloned()
            .collect()
    }

    pub fn write_count(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    pub fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(
    mut stream: TcpStream,
    mode: Mode,
    calls: Arc<Mutex<Vec<Call>>>,
    writes: Arc<Mutex<Vec<f64>>>,
    script: Arc<Mutex<VecDeque<LoginReply>>>,
    t0: Instant,
) {
    // keep-alive：一个连接上可能连着来好几条请求（客户端就是这么复用的）
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    while let Some(req) = read_request(&mut stream) {
        let (method, path, form, cookie) = req;
        let path_only = path.split('?').next().unwrap_or(&path).to_string();
        calls.lock().unwrap().push(Call {
            method: method.clone(),
            path: path_only.clone(),
            form: form.clone(),
            cookie: cookie.clone(),
        });

        let mut cookies: Vec<String> = Vec::new();
        let body: String = if method == "POST" && path_only.ends_with("vcode.do") {
            json(r#"{"code":"1","data":{"token":"vt-1"}}"#)
        } else if method == "POST" && path_only.ends_with("login.do") {
            let step = script.lock().unwrap().pop_front().unwrap_or(LoginReply::Ok);
            match step {
                LoginReply::Ok => {
                    cookies.push("JSESSIONID=S1; Path=/".to_string());
                    cookies.push("EXTRA=W1; Path=/".to_string());
                    let name = form
                        .iter()
                        .find(|(k, _)| k == "loginName")
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    json(&format!(
                        r#"{{"code":"1","data":{{"token":"tok-new","name":"测试同学","number":"{name}"}}}}"#
                    ))
                }
                LoginReply::Code(code, msg) => {
                    json(&format!(r#"{{"code":"{code}","msg":"{msg}"}}"#))
                }
            }
        } else if method == "GET" && path_only.ends_with("image.do") {
            cookies.push("route=r1; Path=/".to_string());
            cookies.push("insert_cookie=ic1; Path=/".to_string());
            respond(&mut stream, 200, "image/jpeg", FAKE_JPEG, &cookies);
            continue;
        } else if mode == Mode::GuardSession && !cookie.contains("JSESSIONID=S1") {
            json(r#"{"code":"302","msg":"未登录用户"}"#)
        } else if method == "POST" && path_only.ends_with("volunteer.do") {
            // 写请求在任何模式下都要记账（自检要能断言"发了几发"）
            writes.lock().unwrap().push(t0.elapsed().as_secs_f64());
            if mode == Mode::InitOutage {
                // 真实措辞：放课瞬间学校就是这个状态，而且**它被算作可重试**
                json(r#"{"code":"0","msg":"选课系统正在初始化,请稍候..."}"#)
            } else {
                json(r#"{"code":"1","msg":"添加选课志愿成功"}"#)
            }
        } else if mode == Mode::InitOutage && path_only.ends_with("capacity.do") {
            // 初始化中的真实行为：没有数据，返回 0/0
            json(r#"{"code":"1","data":{"nonMainClassCapacity":"0","nonMainElectiveNumber":"0"}}"#)
        } else {
            json(r#"{"code":"1","data":{"campus":"01"},"dataList":[]}"#)
        };
        respond(
            &mut stream,
            200,
            "application/json",
            body.as_bytes(),
            &cookies,
        );
    }
}

fn json(s: &str) -> String {
    s.to_string()
}

/// 当前时间的 HTTP-date（IMF-fixdate）—— 只读请求靠它做对时，所以必须是真的"现在"。
fn http_date() -> String {
    // timeutil::civil 按配置时区显示，所以先减掉偏移拿 UTC。
    // 星期几随便写：解析器不看它（Python 的 parsedate_to_datetime 也不看）。
    let t = course_grabber::timeutil::unix_now();
    let utc = course_grabber::timeutil::civil(t - course_grabber::timeutil::offset_secs() as f64);
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "Fri, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        utc.day,
        MON[(utc.month - 1) as usize],
        utc.year,
        utc.hour,
        utc.minute,
        utc.second
    )
}

fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8], cookies: &[String]) {
    let mut head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nDate: {}\r\nConnection: keep-alive\r\n",
        body.len(),
        http_date()
    );
    for c in cookies {
        head.push_str(&format!("Set-Cookie: {c}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// 分块编码的响应：故意切成两段，并带上分块扩展与结尾的 trailer，
/// 把"分块头里带分号参数""终止块后有 trailer"这两个常见形态一起覆盖掉。
fn respond_chunked(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &[u8],
    cookies: &[String],
) {
    let mut head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {ctype}\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n"
    );
    for c in cookies {
        head.push_str(&format!("Set-Cookie: {c}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let (a, b) = body.split_at(body.len() / 2);
    for (i, chunk) in [a, b].iter().enumerate() {
        if chunk.is_empty() {
            continue;
        }
        // 第一段带个分块扩展参数（真实服务器会这么干）
        let ext = if i == 0 { ";ext=1" } else { "" };
        let _ = stream.write_all(format!("{:x}{ext}\r\n", chunk.len()).as_bytes());
        let _ = stream.write_all(chunk);
        let _ = stream.write_all(b"\r\n");
    }
    // 终止块 + trailer
    let _ = stream.write_all(b"0\r\nX-Trailer: 1\r\n\r\n");
    let _ = stream.flush();
}

/// 一条解析出来的请求：(方法, 路径, 表单字段, Cookie 头)
type ParsedRequest = (String, String, Vec<(String, String)>, String);

/// 读一条请求（请求行 + 头 + 按 Content-Length 读全 body）。
fn read_request(stream: &mut TcpStream) -> Option<ParsedRequest> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        match stream.read(&mut tmp) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return None,
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    let mut cookie = String::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            } else if k == "cookie" {
                cookie = v.to_string();
            }
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&body[..content_length.min(body.len())]).into_owned();
    Some((method, path, parse_form(&text), cookie))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `application/x-www-form-urlencoded` 的解析（+ 号是空格、%XX 是字节）。
fn parse_form(text: &str) -> Vec<(String, String)> {
    text.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
