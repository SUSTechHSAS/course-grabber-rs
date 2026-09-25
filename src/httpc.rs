//! 极简 HTTP/1.1 客户端。
//!
//! 为什么不拉 reqwest/hyper/ureq：这所学校的接口是**明文 http**，没有 TLS、没有
//! 重定向、没有 cookie jar、没有 gzip —— 需要的只是一个能精确控制超时与连接生死的
//! TCP 客户端。放课窗口里"一个请求挂住"就是致命的，所以这里刻意保留了两条路：
//!
//! * [`Client`]：带 keep-alive 的连接，给只读请求用（预检、复核可以慢慢来）；
//! * 一次性连接：给写请求用（一发独占一条连接，挂住的只会拖死它自己）。
//!
//! 与原版 `http.client` 对齐的几处细节：
//!  * 每次请求都会发 `Accept-Encoding: identity`（http.client 的默认行为）。
//!    不显式要求 identity 的话，服务器可能回 gzip，而这里不做解压。
//!  * body 的读超时**不是错误**：拿到多少算多少，状态码照常返回（原版就是这样，
//!    否则放课瞬间一个挂住的响应会被当成"网络故障"丢掉信息）。
//!  * 没有 Content-Length 也没有 chunked 时，立刻返回已读到的部分，不等 EOF。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::json::Json;

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// 头名大小写不敏感（HTTP 头本来就大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == want)
            .map(|(_, v)| v.as_str())
    }

    /// 所有 `Set-Cookie`（保持顺序）—— 表头可能被服务器拆成多条。
    pub fn set_cookies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("set-cookie") {
                out.push(v.clone());
            }
        }
        out
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// 解析失败给空对象（对应原版到处写的 `except ValueError: payload = {}`）。
    pub fn json(&self) -> Json {
        crate::json::parse_or_empty(&self.text())
    }
}

// ---------------------------------------------------------------------------
// 字节级工具
// ---------------------------------------------------------------------------

/// 连接（带 TCP_NODELAY：放课瞬间那几发小报文，等 Nagle 攒包没有意义）。
pub fn connect(host: &str, port: u16, timeout: f64) -> std::io::Result<TcpStream> {
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "域名解析不出地址")
    })?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs_f64(timeout.max(0.05)))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// 把请求头 + body 写出去。
fn write_request(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: f64,
) -> std::io::Result<()> {
    let mut head = String::with_capacity(256);
    head.push_str(method);
    head.push(' ');
    head.push_str(path);
    head.push_str(" HTTP/1.1\r\n");
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");

    stream.set_write_timeout(Some(Duration::from_secs_f64(timeout.max(0.05))))?;
    stream.write_all(head.as_bytes())?;
    if let Some(b) = body {
        stream.write_all(b)?;
    }
    stream.flush()?;
    let _ = stream.set_write_timeout(None);
    Ok(())
}

/// 发一个请求并读回响应（连接由调用方给，用完好关）。
pub fn exchange(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout: f64,
) -> std::io::Result<Response> {
    write_request(stream, method, path, headers, body, timeout)?;
    read_response(stream, timeout)
}

/// 读一个完整的 HTTP 响应。状态行读不到才算失败；body 读超时按"拿到多少算多少"处理。
pub fn read_response(stream: &mut TcpStream, timeout: f64) -> std::io::Result<Response> {
    Ok(read_response_full(stream, timeout)?.0)
}

/// 同上，另外告诉调用方"这个 body 是不是完整读完了"。
///
/// keep-alive 连接必须知道这件事：body 读了一半就超时的话，剩下的字节会留在
/// 内核缓冲里，下一条复用这条连接的请求会把它们当成响应头 —— 于是拿到垃圾。
/// 拿不完整就应当把连接丢掉重连。
fn read_response_full(stream: &mut TcpStream, timeout: f64) -> std::io::Result<(Response, bool)> {
    stream.set_read_timeout(Some(Duration::from_secs_f64(timeout.max(0.05))))?;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 8192];

    let empty = |status: u16| {
        (
            Response {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            },
            false,
        )
    };

    // 先把响应头收全
    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        match stream.read(&mut tmp) {
            Ok(0) => return Ok(empty(0)),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            // 头都没收全就超时/被断 —— 当成"没有响应"（原版 read_http 返回 0）
            Err(e) if is_timeout(&e) => return Ok(empty(0)),
            Err(e) => return Err(e),
        }
        if buf.len() > 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "响应头超过 1MB",
            ));
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut rest = buf[head_end + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let get = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };

    let (body, complete) = if get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        let mut raw = std::mem::take(&mut rest);
        loop {
            let (body, complete) = dechunk(&raw);
            if complete {
                break (body, true);
            }
            match stream.read(&mut tmp) {
                Ok(0) => break (body, false),
                Ok(n) => raw.extend_from_slice(&tmp[..n]),
                Err(e) if is_timeout(&e) => break (body, false),
                Err(e) => return Err(e),
            }
            if raw.len() > 64 * 1024 * 1024 {
                break (body, false);
            }
        }
    } else if let Some(len) = get("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        let mut body = rest;
        let mut done = body.len() >= len;
        while body.len() < len {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    body.extend_from_slice(&tmp[..n]);
                    done = body.len() >= len;
                }
                Err(e) if is_timeout(&e) => break,
                Err(e) => return Err(e),
            }
        }
        body.truncate(len.min(body.len()));
        (body, done)
    } else {
        // 既没有长度也不是分块：把额外读到的那点当作全部（原版也是这样）
        (std::mem::take(&mut rest), true)
    };

    Ok((
        Response {
            status,
            headers,
            body,
        },
        complete,
    ))
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 解 HTTP 分块编码（原版 `_dechunk`）。第二个返回值 = 是否见到了终止块。
fn dechunk(blob: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(blob.len());
    let mut rest = blob;
    loop {
        let Some(pos) = find(rest, b"\r\n") else {
            // 分块头还没收全
            return (out, false);
        };
        let line = &rest[..pos];
        let hex_part = line.split(|&c| c == b';').next().unwrap_or(&[]);
        let text = String::from_utf8_lossy(hex_part);
        let Ok(size) = usize::from_str_radix(text.trim(), 16) else {
            return (out, false); // 畸形的分块头：不算收完
        };
        if size == 0 {
            // 终止块。后面可能还有 trailer 头，见到空行才算真的结束。
            let tail = &rest[pos + 2..];
            let done = tail.is_empty() || tail == b"\r\n" || find(tail, b"\r\n\r\n").is_some();
            return (out, done);
        }
        let start = pos + 2;
        if start + size + 2 > rest.len() {
            out.extend_from_slice(&rest[start..]);
            return (out, false);
        }
        out.extend_from_slice(&rest[start..start + size]);
        rest = &rest[start + size + 2..]; // 跳过分块后紧跟的 CRLF
    }
}

// ---------------------------------------------------------------------------
// keep-alive 连接
// ---------------------------------------------------------------------------

/// 一条可以重复使用的连接（坏了就丢，下一次请求自动重连）。
pub struct Client {
    host: String,
    port: u16,
    stream: Option<TcpStream>,
    connect_timeout: f64,
}

impl Client {
    pub fn new(host: &str, port: u16, connect_timeout: f64) -> Client {
        Client {
            host: host.to_string(),
            port,
            stream: None,
            connect_timeout,
        }
    }

    /// 丢掉当前连接（下次请求会重新建连）。
    pub fn drop_conn(&mut self) {
        self.stream = None;
    }

    /// 请求 + 自动重连重试（`attempts` 是**总**尝试次数，与原版一致）。
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: f64,
        attempts: usize,
    ) -> std::io::Result<Response> {
        let mut last: Option<std::io::Error> = None;
        for _ in 0..attempts.max(1) {
            if self.stream.is_none() {
                match connect(&self.host, self.port, self.connect_timeout) {
                    Ok(s) => self.stream = Some(s),
                    Err(e) => {
                        last = Some(e);
                        continue;
                    }
                }
            }
            let stream = self.stream.as_mut().expect("刚建好的连接");
            let outcome = write_request(stream, method, path, headers, body, timeout)
                .and_then(|()| read_response_full(stream, timeout));
            match outcome {
                Ok((resp, complete)) => {
                    // 半截响应（头没读全 / body 读超时）在**只读路径**上要当成一次失败尝试
                    // 再发一次 —— 这与 Python 一致（那边的 socket 超时会抛异常，被
                    // `except Exception` 接住后关连接重发）。注意首发/写请求走的是
                    // `read_response`/`exchange`，不经过这里，所以绝不会重复提交。
                    if !complete || resp.status == 0 {
                        self.stream = None; // 残字节可能还在内核缓冲里，这条连接不能再用
                        last = Some(std::io::Error::other("响应不完整（超时或连接被断）"));
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    self.stream = None; // 连接坏了，下一轮重连
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("请求失败")))
    }
}

// ---------------------------------------------------------------------------
// URL 编码（对齐 urllib.parse）
// ---------------------------------------------------------------------------

const ALWAYS_SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-~";

/// `urllib.parse.quote_plus`：空格变 `+`，其余非安全字节按 UTF-8 逐字节转义。
pub fn quote_plus(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.as_bytes() {
        if ALWAYS_SAFE.contains(b) {
            out.push(*b as char);
        } else if *b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `urllib.parse.quote`：空格变 `%20`；`safe` 里的字符不转义（默认只加 `/`）。
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.as_bytes() {
        if ALWAYS_SAFE.contains(b) || safe.as_bytes().contains(b) {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `urllib.parse.urlencode`：按给定顺序拼成 `k=v&k=v`。
pub fn urlencode(pairs: &[(&str, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&quote_plus(k));
        out.push('=');
        out.push_str(&quote_plus(v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_matches_python() {
        // 与 urllib.parse.urlencode({"addParam": '{"data":{"a":"中 文"}}'}) 逐字符一致
        let got = urlencode(&[("addParam", r#"{"data":{"a":"中 文"}}"#.to_string())]);
        assert_eq!(
            got,
            "addParam=%7B%22data%22%3A%7B%22a%22%3A%22%E4%B8%AD+%E6%96%87%22%7D%7D"
        );
        // 安全字符：字母数字 + _.-~ 不转义；空格 → +
        assert_eq!(quote_plus("a-b_c.d~e f"), "a-b_c.d~e+f");
        assert_eq!(quote_plus("=/&?"), "%3D%2F%26%3F");
    }

    #[test]
    fn quote_keeps_safe() {
        assert_eq!(quote("a/b c", "/"), "a/b%20c");
        assert_eq!(quote("vt-1", "/"), "vt-1");
    }

    #[test]
    fn dechunk_basic() {
        assert_eq!(
            dechunk(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n"),
            (b"Wikipedia".to_vec(), true)
        );
        assert_eq!(dechunk(b"0\r\n\r\n"), (Vec::new(), true));
        // 终止块带 trailer 头也算收完
        assert_eq!(
            dechunk(b"2\r\nhi\r\n0\r\nX: y\r\n\r\n"),
            (b"hi".to_vec(), true)
        );
        // 还没收全：不算完整，但已解出的数据要留着
        assert_eq!(
            dechunk(b"4\r\nWiki\r\n5\r\nped"),
            (b"Wikiped".to_vec(), false)
        );
        assert_eq!(dechunk(b"4\r\nWiki\r\n"), (b"Wiki".to_vec(), false));
        // 畸形输入不能 panic，也不能丢数据
        assert_eq!(dechunk(b"zz\r\noops"), (Vec::new(), false));
    }

    #[test]
    fn parses_status_and_headers() {
        // 造一个内存流的替身不方便，这里只测分块与头部小工具
        let r = Response {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Set-Cookie".into(), "route=a; Path=/".into()),
                ("set-cookie".into(), "insert_cookie=b".into()),
            ],
            body: b"{\"code\":\"1\"}".to_vec(),
        };
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.set_cookies().len(), 2);
        assert_eq!(r.json().text("code"), "1");
        assert_eq!(r.text(), "{\"code\":\"1\"}");
    }
}
