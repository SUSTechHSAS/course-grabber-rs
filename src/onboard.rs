//! 开荒：把"用户手里已有的东西"变成一份能跑的配置。
//!
//! 这一层存在的理由很直白 —— config.json 里那些键，普通用户**不可能知道填什么**：
//! 接口路径、密码加密密钥、教学班 ID、冲突组……它们要么是教务系统的内部实现细节，
//! 要么得先登进系统才看得到。但用户手里有两样东西是现成的：
//!
//! 1. **浏览器地址栏里那条选课页的 URL** —— 域名、端口、部署上下文、页面路径全在里面；
//! 2. **学号 + 密码** —— 有了它就能登进去把课程目录拉出来，候选教学班自然就有了。
//!
//! 于是这里提供三件事，都不需要用户理解任何接口：
//!
//! * [`derive`] / [`apply`]：从 URL 推出"学校"那一节，并把缺的键按内编的参考配置补齐；
//! * [`probe`]：只读地探一下这些路径是否真的存在（不登录、不提交），用来验证上面的推导；
//! * [`fetch_candidates`]：用学号密码登录，把目标课程的教学班列出来（含冲突组）。
//!
//! 关于"针对某所学校"这件事：仓库里没有任何学校的名字，但 `config.example.json`
//! 里那套接口路径就是参考实现的真实形状（用户平时正是 `cp` 它来起步的，
//! [`fill_reference`] 只是替他做这件事）。所以这里能做的是**推导 + 验证**，
//! 而不是内置一份"某校专用数据"。

use std::sync::Arc;

use crate::auth::{Credentials, Login, LoginSession};
use crate::captcha::Solver;
use crate::clock;
use crate::config::{self, Endpoints};
use crate::grab::School;
use crate::httpc;
use crate::json::{self, Json};
use crate::log;
use crate::timeutil;

/// 一条提示的严重程度（界面按它上色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Err,
}

impl Level {
    /// 纯文本前缀（界面和日志都用它）。
    pub fn mark(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warn => "⚠",
            Level::Err => "✗",
        }
    }
}

/// 界面与开荒逻辑共用的一句话。
pub type Note = (Level, String);

fn ok(text: impl Into<String>) -> Note {
    (Level::Ok, text.into())
}

fn warn(text: impl Into<String>) -> Note {
    (Level::Warn, text.into())
}

fn err(text: impl Into<String>) -> Note {
    (Level::Err, text.into())
}

// ==========================================================================
// 一、从"用户手里的那点信息"推出学校配置
// ==========================================================================
//
// 用户可能粘进来的东西有好几种（实测过的形状）：
//
//     jw.example.edu.cn                                              ← 只有域名
//     http://jw.example.edu.cn/                                      ← 域名 + 根路径
//     http://jw.example.edu.cn/xs/course/app/*default/index.do ← 选课页
//     http://…/xs/course/app/*default/grablessons.do?token=… ← 带 token 的选课页
//
// 前两种没有路径信息，但**服务器会告诉我们**：`GET /` 实测 302 到
// `/xs/course/app/*default/index.do`。所以"只粘域名"也能配好 —— 一次往返的事。

/// 粘贴内容解析出来的零件（纯字符串处理，不联网）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    pub host: String,
    pub port: i64,
    /// 路径（已去掉 query 与 fragment）
    pub path: String,
    /// URL 里带的 token（只用来告诉用户"看到了但没存"）
    pub token: Option<String>,
    /// 只给了域名（没有可用的页面路径）—— 需要联网去问
    pub bare: bool,
}

/// 从一条选课页 URL 推出学校那一节（纯函数，不联网）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub scheme: String,
    pub host: String,
    pub port: i64,
    /// 业务页路径（`…/*default/grablessons.do` 这种），也是 login_page 的值
    pub path: String,
    /// 接口前缀。认不出就是 None（那就不动用户原来的值）
    pub base_path: Option<String>,
    /// 登录请求用的 Referer 页。认不出就是 None
    pub page_path: Option<String>,
    pub token: Option<String>,
    pub notes: Vec<String>,
    pub warns: Vec<String>,
}

/// 解析粘贴进来的东西。**任何一个都能吃**：光域名、域名+根路径、完整选课页 URL。
pub fn parse_input(input: &str) -> Result<UrlParts, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("还没有填内容 —— 把选课页的网址粘进来，只写域名也行".to_string());
    }
    if raw.contains(char::is_whitespace) {
        return Err("这段内容里有空格 —— 大概只复制到一半".to_string());
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        // 有些浏览器/粘贴板不给 scheme，按 http 处理
        None => ("http".to_string(), raw),
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!("只认 http / https 的网址，拿到的是「{scheme}」"));
    }

    let rest = rest.split('#').next().unwrap_or("");
    let cut = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..cut];
    let tail = &rest[cut..];

    // user:pass@host —— 学校地址里不会有，但粘贴板里可能有别的
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port_in_url) = split_authority(authority)?;
    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = port_in_url.unwrap_or(default_port);

    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, q),
        None => (tail, ""),
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    let token = query_param(query, "token").filter(|t| !t.is_empty());

    Ok(UrlParts {
        scheme,
        host,
        port,
        bare: !path.trim_matches('/').contains('/') && !path.contains("*default"),
        path,
        token,
    })
}

/// 从一条**页面路径**推出学校那一段（纯函数）。
///
/// 关键的一条（拿真实系统量出来的）：接口前缀 = `*default` 前面那一整段上下文。
/// 比如页面是 `/xs/course/app/*default/index.do`，那接口就在
/// `/xs/course/app/elective/volunteer.do` —— **不是** 再拼一段 `/api`。
/// 光看 `config.example.json` 会推错（那份是脱敏过的，`/course-system/api` 那个
/// `api` 是脱敏时换上去的），所以这里以真实系统为准。
pub fn derive_from_path(parts: &UrlParts, path: &str) -> Derived {
    let segs: Vec<&str> = path.split('/').collect();
    let (base_path, page_path) = match segs.iter().position(|s| *s == "*default") {
        Some(i) => {
            // `*default` 之前就是应用上下文；挂在根上时上下文是空串
            let context = segs[..i].join("/");
            (
                Some(context.clone()),
                Some(format!("{context}/*default/index.do")),
            )
        }
        None => {
            // 认不出路径：只填域名端口，"剩下的要你自己填"由 discover() 那边说清楚
            (None, None)
        }
    };

    let mut d = Derived {
        scheme: parts.scheme.clone(),
        host: parts.host.clone(),
        port: parts.port,
        path: path.to_string(),
        base_path,
        page_path,
        token: parts.token.clone(),
        notes: Vec::new(),
        warns: Vec::new(),
    };
    if d.page_path.is_some() {
        d.notes
            .push("登录页路径取同目录下的 `*default/index.do`".to_string());
    }
    if let Some(t) = &d.token {
        d.notes.push(format!(
            "网址里带着 token（{}），**没有**写进配置 —— 会话由自动登录负责，\
             你粘的 token 过一会儿就失效，写进去反而是个坑。",
            crate::grab::short(t, 8)
        ));
    }
    d
}

// --------------------------------------------------------------------------
// 自动发现：不联网认不出路径时，去问服务器
// --------------------------------------------------------------------------

const DISCOVER_TIMEOUT: f64 = 6.0;
/// 每一类课最多翻多少页（每页 50）。20 页 = 1000 门课，够用了，也是防呆。
const MAX_PAGES: usize = 20;
/// 全部类型加起来最多留多少行 —— 界面要滚得动，也不能把内存吃光。
const MAX_ROWS: usize = 4000;
/// 跟随重定向的上限 —— 够走完"根路径 → 应用页"这种一跳，也不至于抓着一个环不放。
const MAX_HOPS: usize = 4;

/// "只粘了域名"时一次往返问到的东西。
#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub derived: Derived,
    /// 实测选出来的对时页（`None` = 程序默认的 `/` 就够用）
    pub time_path: Option<String>,
    /// 探测过程中服务器下发的 Cookie 名（去重、保持出现顺序）
    pub cookie_names: Vec<String>,
    pub notes: Vec<Note>,
}

/// 一次只读的"问服务器"：跟随根路径的重定向找到应用上下文，再实测出接口前缀与对时页。
///
/// 全程只发 GET、不登录、不提交。拿到的每一条都写进 notes，用户能看到"凭什么这么填"。
pub fn discover(input: &str, log: &dyn Fn(&str)) -> Result<Discovered, String> {
    let parts = parse_input(input)?;
    let mut notes: Vec<Note> = Vec::new();
    let mut f = Fetcher::new(&parts.host, parts.port as u16);

    let mut path = parts.path.clone();
    if !path.contains("*default") {
        log(&format!(
            "只拿到地址（{}），去问服务器要页面路径…",
            parts.host
        ));
        match resolve_app_path(&mut f, &path, &mut notes) {
            Ok(Some(p)) => {
                path = p;
                notes.push(ok(format!(
                    "服务器把 {} 指到了 {path} —— 页面路径就是从这儿来的",
                    parts.path
                )));
            }
            Ok(None) => {
                notes.push(warn(
                    "服务器没有把这条地址指到选课页，返回内容里也没有 `*default` 路径。\
                     换成浏览器里选课页的**完整网址**（地址栏 Ctrl-L 全选、Ctrl-C）最稳。",
                ));
            }
            Err(e) => {
                return Err(format!(
                    "连不上 {}:{}（{e}）—— 检查域名有没有写错、要不要先连校园网/VPN",
                    parts.host, parts.port
                ))
            }
        }
    }

    let mut d = derive_from_path(&parts, &path);

    // 接口前缀：先按"上下文就是前缀"试一个真实的只读接口。这个接口是登录前就有的
    // （登录流程自己也要先取这张图），所以拿它验证最合适 —— 200 且是图片就说明前缀对了。
    if let Some(base) = d.base_path.clone() {
        let probe_path = format!("{base}/student/vcode/image.do");
        match f.get(&probe_path) {
            Ok(r) if r.status == 200 && is_image(&r) => {
                notes.push(ok(format!(
                    "接口前缀验证过：GET {probe_path} → 200 {}（验证码图片接口）",
                    r.header("Content-Type").unwrap_or("-")
                )));
            }
            Ok(r) if r.status != 404 => {
                notes.push(warn(format!(
                    "接口前缀看着对：GET {probe_path} → {}（不是 404）。\
                     真正能不能用，跑一次只读预检就知道了",
                    r.status
                )));
            }
            _ => {
                // 试一下带 `/api` 的写法（有些学校把接口单独挂在一个子上下文里）
                let alt = format!("{base}/api");
                let alt_path = format!("{alt}/student/vcode/image.do");
                match f.get(&alt_path) {
                    Ok(r) if r.status == 200 && is_image(&r) => {
                        notes.push(ok(format!(
                            "接口前缀要加一段 `/api`：GET {alt_path} → 200，已按这个填"
                        )));
                        d.base_path = Some(alt);
                    }
                    _ => notes.push(warn(format!(
                        "接口前缀没能验证：GET {probe_path} 取不到验证码图片。\
                         先按 `{base}` 填着，跑只读预检时如果接口全 404，\
                         那就是这个前缀不对（浏览器 F12 → Network 里看一眼真实地址）"
                    ))),
                }
            }
        }
    }

    // 对时页：必须是**真的带 Date 头**的页面。这台学校的根路径是另一台服务器
    // （IIS）接的，302 没带 Date —— 所以"能不能对时"得实测，不能想当然用 `/`。
    let time_path = pick_time_path(&mut f, &d, &mut notes);

    let cookie_names = f.cookie_names();
    if !cookie_names.is_empty() {
        notes.push(ok(format!(
            "过程中服务器下发了这些 Cookie：{}",
            cookie_names.join("、")
        )));
    }

    if parts.scheme == "https" {
        d.warns.push(
            "这条网址是 https。本工具走的是自己实现的明文 HTTP，**不支持 TLS** ——\
             如果学校只提供 https，这个工具用不了。有些学校 http/https 都开，\
             可以试试验证，但别指望。"
                .to_string(),
        );
    }
    if parts.port != if parts.scheme == "https" { 443 } else { 80 } {
        d.notes.push(format!("端口按网址里的写法取 {}", parts.port));
    }

    Ok(Discovered {
        derived: d,
        time_path,
        cookie_names,
        notes,
    })
}

/// 跟随重定向，直到看见一条含 `*default` 的路径（或走完上限）。
fn resolve_app_path(
    f: &mut Fetcher,
    start: &str,
    notes: &mut Vec<Note>,
) -> Result<Option<String>, String> {
    let mut path = start.to_string();
    for _ in 0..MAX_HOPS {
        let r = f.get(&path)?;
        notes.push(ok(format!("GET {path} → {}", r.status)));
        if r.status >= 300 && r.status < 400 {
            if let Some(loc) = r.header("Location") {
                let next = location_path(loc);
                if next.contains("*default") {
                    return Ok(Some(next));
                }
                if next != path {
                    path = next;
                    continue;
                }
            }
        }
        // 不是在跳转：看看返回的内容里有没有 `*default/…`
        if let Some(found) = find_default_path(&r.text()) {
            return Ok(Some(found));
        }
        return Ok(None);
    }
    Ok(None)
}

/// 从 `Location`（可能是绝对 URL、也可能是 `/x/y.do`）里取出路径。
fn location_path(loc: &str) -> String {
    let loc = loc.trim();
    let after_scheme = match loc.split_once("://") {
        Some((_, rest)) => {
            let cut = rest.find(['/', '?']).unwrap_or(rest.len());
            &rest[cut..]
        }
        None => loc,
    };
    let p = after_scheme.split(['?', '#']).next().unwrap_or("/");
    if p.is_empty() {
        "/".to_string()
    } else {
        p.to_string()
    }
}

/// 在一段 HTML/JS 里找 `…/*default/…` 那种路径（服务器把地址写在页面里时用得上）。
fn find_default_path(text: &str) -> Option<String> {
    let at = text.find("*default/")?;
    // 往前回溯到路径开头：允许字母数字和 `/ . _ -`，最多 120 个字符
    let head = &text[..at];
    let mut start = at;
    for (i, c) in head.char_indices().rev() {
        if c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-') {
            start = i;
        } else {
            break;
        }
        if at - start > 120 {
            break;
        }
    }
    // 再往后取到路径结束（`*default/xxx.do` 那种，遇到引号/空白/尖括号就停）
    let tail = &text[at..];
    let end = tail
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | '\\' | '#'))
        .unwrap_or(tail.len());
    let found = &text[start..at + end];
    if found.starts_with('/') && found.len() > "*default/".len() {
        Some(found.to_string())
    } else {
        None
    }
}

/// 实测出哪一页真的带 `Date` 头（对时要靠它）。
fn pick_time_path(f: &mut Fetcher, d: &Derived, notes: &mut Vec<Note>) -> Option<String> {
    let mut candidates: Vec<String> = vec!["/".to_string()];
    if let Some(ctx) = &d.base_path {
        if let Some(first) = ctx.trim_start_matches('/').split('/').next() {
            if !first.is_empty() {
                candidates.push(format!("/{first}/"));
            }
        }
        if !ctx.is_empty() {
            candidates.push(format!("{}/", ctx.trim_end_matches('/')));
        }
    }
    if let Some(p) = &d.page_path {
        candidates.push(p.clone());
    }
    let mut tried: Vec<String> = Vec::new();
    for c in candidates {
        if tried.contains(&c) {
            continue;
        }
        tried.push(c.clone());
        let r = match f.get(&c) {
            Ok(r) => r,
            Err(e) => {
                notes.push(warn(format!("{c} 请求失败（跳过）：{e}")));
                continue;
            }
        };
        match r.header("Date") {
            Some(raw) => {
                let ok_date = timeutil::parse_http_date(raw);
                if ok_date.is_some() {
                    notes.push(ok(if c == "/" {
                        "对时页用默认的 `/`（实测它带 Date 头）".to_string()
                    } else {
                        format!("对时页取 {c} —— 实测只有它带 Date 头，对时要靠它")
                    }));
                    return if c == "/" { None } else { Some(c) };
                }
            }
            None => {
                notes.push(ok(format!("{c} 没有 Date 头（跳过，不能拿它对时）")));
            }
        }
    }
    notes.push(warn(
        "没找到带 Date 头的页面 —— 对时那一步会失效（放课瞄准会糙一些，但抢课照常）。\
         想修的话：找浏览器里任意一个返回 200 的页面，把它的路径填进 school.time_path",
    ));
    None
}

fn is_image(r: &httpc::Response) -> bool {
    r.header("Content-Type")
        .map(|c| c.starts_with("image/"))
        .unwrap_or(false)
        // 有些服务器不给 Content-Type，那就看魔数
        || r.body.starts_with(&[0xff, 0xd8, 0xff])
}

/// 只读 GET 的小工具：顺路把服务器下发的 Cookie 名记下来。
struct Fetcher {
    client: httpc::Client,
    host: String,
    port: u16,
    cookies: Vec<String>,
}

impl Fetcher {
    fn new(host: &str, port: u16) -> Fetcher {
        Fetcher {
            client: httpc::Client::new(host, port, 2.0),
            host: host.to_string(),
            port,
            cookies: Vec::new(),
        }
    }

    fn cookie_names(&self) -> Vec<String> {
        self.cookies.clone()
    }

    fn get(&mut self, path: &str) -> Result<httpc::Response, String> {
        let host_header = if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let headers = vec![
            ("Host".to_string(), host_header),
            (
                "User-Agent".to_string(),
                format!("course-grabber/{} (config wizard)", config::VERSION),
            ),
            ("Accept".to_string(), "*/*".to_string()),
            ("Accept-Encoding".to_string(), "identity".to_string()),
            ("Cache-Control".to_string(), "no-cache".to_string()),
        ];
        // attempts=2：服务器把 keep-alive 连接关掉是常事，**一次失败不能当结论** ——
        // 探"这个接口前缀存不存在"时把连接层的抖动读成"不存在"，会推出错的前缀。
        let resp = self
            .client
            .request("GET", path, &headers, None, DISCOVER_TIMEOUT, 2)
            .map_err(|e| e.to_string())?;
        for raw in resp.set_cookies() {
            if let Some(name) = crate::auth::cookie_name(&raw) {
                if !self.cookies.contains(&name) {
                    self.cookies.push(name);
                }
            }
        }
        Ok(resp)
    }
}

/// `host[:port]`，端口非法就报错。
fn split_authority(authority: &str) -> Result<(String, Option<i64>), String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 字面量（学校里不会有，但别把它解析成半个域名）
        match rest.split_once(']') {
            Some((h, tail)) => (
                h.to_string(),
                tail.strip_prefix(':').and_then(|p| p.parse::<i64>().ok()),
            ),
            None => return Err("IPv6 地址少了右方括号".to_string()),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => match p.parse::<i64>() {
                Ok(n) => (h.to_string(), Some(n)),
                Err(_) => return Err(format!("端口「{p}」不是数字")),
            },
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty() {
        return Err("这段内容里没有域名".to_string());
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ':')
    {
        return Err(format!("域名「{host}」看着不像域名"));
    }
    if let Some(p) = port {
        if !(1..=65535).contains(&p) {
            return Err(format!("端口 {p} 不在 1~65535 里"));
        }
    }
    Ok((host, port))
}

/// 从 query 里取一个参数（只看第一个，够用了；不做 `+`→空格的转换是故意的：
/// token 一定是百分号编码的，转换反而会改坏它）。
fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == name {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 把推导结果写进配置，并把缺的键按参考实现补齐。返回"做了什么"的清单。
pub fn apply(raw: &mut Json, d: &Derived, time_path: Option<&str>) -> Vec<Note> {
    let mut out = Vec::new();

    raw.ensure_obj("school")
        .set_key("host", Json::str(d.host.clone()));
    out.push(ok(format!("school.host = {}", d.host)));
    raw.ensure_obj("school").set_key("port", Json::Int(d.port));
    out.push(ok(format!("school.port = {}", d.port)));

    match &d.base_path {
        Some(b) => {
            raw.ensure_obj("school")
                .set_key("base_path", Json::str(b.clone()));
            out.push(ok(format!("school.base_path = {b}")));
        }
        None => out.push(warn(
            "接口前缀没认出来，保留你原来的值 —— 记得手填 school.base_path",
        )),
    }
    match &d.page_path {
        Some(p) => {
            raw.ensure_obj("school")
                .set_key("page_path", Json::str(p.clone()));
            out.push(ok(format!("school.page_path = {p}")));
        }
        None => out.push(warn("登录页路径没认出来，保留原值")),
    }
    // login_page 程序不读（老配置遗留键）。已经存在就跟着更新，免得看着自相矛盾。
    if raw
        .object("school")
        .map(|s| s.get("login_page").is_some())
        .unwrap_or(false)
    {
        raw.ensure_obj("school")
            .set_key("login_page", Json::str(d.path.clone()));
        out.push(ok(format!(
            "school.login_page = {}（程序不读这个键，跟着改只为看着一致）",
            d.path
        )));
    }

    // Referer 这条**故意不填**：程序默认用 `{接口前缀}/*default/grablessons.do`，
    // 那是与原版逐字一致、实测能过的值；拿浏览器里那条页面 URL 去替它反而可能过不了。
    out.push(ok(
        "Referer 沿用程序默认（接口前缀 + `/*default/grablessons.do`），没动这一项",
    ));

    out.extend(fill_reference(raw));

    if let Some(t) = time_path {
        raw.ensure_obj("school")
            .set_key("time_path", Json::str(t.to_string()));
        out.push(ok(format!("school.time_path = {t}")));
    }

    for n in &d.notes {
        out.push(ok(n.clone()));
    }
    for w in &d.warns {
        out.push(warn(w.clone()));
    }
    out
}

/// "这套系统长什么样"的那些键：缺就按**内编的参考配置**补上，已有的一律不动。
///
/// 接口路径、密码加密密钥、课程类型这些，用户不可能自己知道，而它们就在仓库里
/// 那份公开的 `config.example.json` 里（用户平时正是 `cp` 它来起步）。
/// 这里只是把那句 `cp` 替他做了 —— 所以叫"参考实现"，不叫"内置某校数据"。
pub fn fill_reference(raw: &mut Json) -> Vec<Note> {
    let reference = json::parse_or_empty(config::CONFIG_EXAMPLE);
    let mut filled: Vec<String> = Vec::new();

    for (sec, keys) in [
        // host/port/base_path/page_path 由 URL（和实测）定，不在这里
        ("school", &["referer_path"][..]),
        ("cookies", &["session", "captcha"][..]),
        ("password", &["des_keys"][..]),
    ] {
        if reference.object(sec).is_none() {
            continue;
        }
        for k in keys {
            take_if_missing(raw, &reference, k, sec, &mut filled);
        }
    }
    // 目标课程：只补"系统通用"的键；keyword / candidates 是用户自己的数据，绝不凭空造
    config::normalize_courses(raw);
    let ref_course = reference
        .array("courses")
        .first()
        .cloned()
        .unwrap_or(Json::Null);
    for k in ["class_type", "is_major", "query_content"] {
        let has = raw
            .array("courses")
            .first()
            .map(|c| c.get(k).is_some())
            .unwrap_or(false);
        if has {
            continue;
        }
        if let Some(v) = ref_course.get(k).cloned() {
            if let Some(slot) = raw.ensure_arr("courses").first_mut() {
                slot.set_key(k, v);
                filled.push(format!("course.{k}"));
            }
        }
    }

    // paths 整段：少一个键程序就会在启动时报"配置不完整"
    if let Some(Json::Obj(pairs)) = reference.object("paths") {
        let keys: Vec<String> = pairs
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| !k.starts_with('_'))
            .collect();
        for k in keys {
            take_if_missing(raw, &reference, &k, "paths", &mut filled);
        }
    }

    let mut out = Vec::new();
    if filled.is_empty() {
        out.push(ok("该有的键都在，没有需要补的"));
    } else {
        out.push(warn(format!(
            "按参考实现补上了 {} 个缺失的键（它们描述的是这套系统长什么样，不是你学校特有的）：{}",
            filled.len(),
            filled.join("、")
        )));
        out.push(ok("已经写着的键一律没动 —— 你自己填过的值永远优先"));
    }
    out
}

/// `raw[sec][key]` 缺就照 `reference` 补上。返回是否补了。
fn take_if_missing(
    raw: &mut Json,
    reference: &Json,
    key: &str,
    sec: &str,
    filled: &mut Vec<String>,
) -> bool {
    let Some(src) = reference.object(sec).and_then(|s| s.get(key)).cloned() else {
        return false;
    };
    if raw.ensure_obj(sec).get(key).is_some() {
        return false;
    }
    raw.ensure_obj(sec).set_key(key, src);
    filled.push(format!("{sec}.{key}"));
    true
}

// ==========================================================================
// 二、只读自检：这些路径真的存在吗
// ==========================================================================

const PROBE_TIMEOUT: f64 = 5.0;

/// 只读地探一遍：不登录、不提交、不改任何状态。
///
/// 关键是**先问一个肯定不存在的路径**当对照 —— 有些教务系统对 404 也回 200
/// （一个 HTML 错误页），没有这个对照，"页面存在"的判断就全是假的。
pub fn probe(ep: &Endpoints) -> Vec<Note> {
    let mut out: Vec<Note> = Vec::new();
    let mut client = httpc::Client::new(&ep.host, ep.port, ep.connect_timeout.max(1.0));
    emit(
        &mut out,
        ok(format!(
            "只读探测 http://{}:{}（不登录、不提交）",
            ep.host, ep.port
        )),
    );

    // 对照：先问一个肯定不存在的路径。有些教务系统对 404 也回 200（一个 HTML 错误页），
    // 没有这个基准，"页面存在"的判断就全是假的。
    let control_path = format!("/__course_grabber_probe_{}", std::process::id());
    let control = match get(&mut client, ep, &control_path) {
        Ok(r) => Some(r.status),
        Err(e) => {
            emit(
                &mut out,
                err(format!("连不上 {}:{} —— {e}", ep.host, ep.port)),
            );
            emit(
                &mut out,
                warn("先确认这几件事：域名抄对了没有、要不要先连校园网/VPN、端口是不是 80。"),
            );
            return out;
        }
    };
    if let Some(c) = control {
        emit(
            &mut out,
            ok(format!(
                "对照：不存在的路径 {control_path} 回 {c}{}",
                if c == 404 {
                    "（好：这台服务器分得清有没有）"
                } else {
                    "（注意：它对不存在的路径不回 404，下面的判断会打折扣）"
                }
            )),
        );
    }

    // 站点根 + Date 头：顺便报一下服务器时钟 —— 抢课最怕本机时间偏
    match get(&mut client, ep, &ep.time_path) {
        Ok(r) => {
            emit(&mut out, ok(format!("GET {} → {}", ep.time_path, r.status)));
            if let Some(d) = r.header("Date").and_then(timeutil::parse_http_date) {
                let skew = d as f64 - timeutil::unix_now();
                let tone = if skew.abs() <= 2.0 {
                    Level::Ok
                } else {
                    Level::Warn
                };
                emit(
                    &mut out,
                    (
                        tone,
                        format!(
                            "服务器时钟与本机相差 {skew:+.1} 秒{}",
                            if skew.abs() > 2.0 {
                                "（不必自己改本机时间 —— 程序会按 Date 头对时；                                  但差得太多说明网络也不太好）"
                            } else {
                                ""
                            }
                        ),
                    ),
                );
            }
        }
        Err(e) => emit(&mut out, err(format!("GET {} 失败：{e}", ep.time_path))),
    }

    let mut items: Vec<(&str, String)> =
        vec![("登录页（登录请求的 Referer）", ep.page_path.clone())];
    for (name, path) in [
        ("登录接口", ep.path("login")),
        ("验证码图片接口", ep.path("vcode_image")),
        ("课程目录接口", ep.path("program")),
    ] {
        items.push((name, path));
    }

    let mut bad = 0usize;
    for (name, path) in &items {
        match get(&mut client, ep, path) {
            Ok(r) => {
                let (tone, verdict) = judge(r.status, control);
                if tone == Level::Err {
                    bad += 1;
                }
                let kind = r.header("Content-Type").unwrap_or("-").to_string();
                emit(
                    &mut out,
                    (
                        tone,
                        format!(
                            "GET {path} → {} {kind} {} 字节 · {name}：{verdict}",
                            r.status,
                            r.body.len()
                        ),
                    ),
                );
            }
            Err(e) => {
                bad += 1;
                emit(&mut out, err(format!("GET {path} 失败 · {name}：{e}")));
            }
        }
    }

    emit(
        &mut out,
        if bad == 0 {
            warn(
                "看起来对得上。正式跑之前还是先跑一次 `course-grabber`（不加 --live）做只读预检，\
                 那一步会拿会话去问所有接口。",
            )
        } else {
            warn(format!(
                "有 {bad} 条路径不对。接口前缀现在是「{}」：\
                 浏览器里按 F12 → Network，随便点一条选课相关的请求，看它地址里 \
                 `/api/` 前面那一段是什么，把 school.base_path 改成一样的。",
                ep.base_path
            ))
        },
    );
    out
}

/// 记一行：既进结果清单，也实时写成日志（TUI 里能看到它一行行长出来）。
fn emit(out: &mut Vec<Note>, n: Note) {
    log::log(&format!("{} {}", n.0.mark(), n.1));
    out.push(n);
}

/// 一条探测结果好不好：拿"不存在的路径"当基准比。
fn judge(status: u16, control: Option<u16>) -> (Level, &'static str) {
    let Some(c) = control else {
        return (Level::Warn, "没有对照，说不准");
    };
    if status == 404 {
        return (Level::Err, "404 —— 这条路径不存在");
    }
    // 服务器对不存在的路径也回同一个状态（常见于把 200 当成"页面"回的框架），
    // 那就没有任何信息量，别硬说"存在"
    if status == c {
        return (Level::Warn, "和「不存在的路径」回同一个状态，判断不了");
    }
    match status {
        200 => (Level::Ok, "200，存在"),
        301 | 302 | 303 | 307 | 308 => (Level::Ok, "重定向（多半是要求登录），路径是存在的"),
        401 | 403 => (Level::Ok, "要求权限，路径是存在的"),
        405 => (Level::Ok, "405（这个方法不允许），路径是存在的"),
        _ => (Level::Warn, "有响应，但状态码不常见"),
    }
}

fn get(client: &mut httpc::Client, ep: &Endpoints, path: &str) -> Result<httpc::Response, String> {
    let ua = if ep.ua.is_empty() {
        format!("course-grabber/{}", config::VERSION)
    } else {
        ep.ua.clone()
    };
    let headers = vec![
        ("Host".to_string(), ep.host_header()),
        ("User-Agent".to_string(), ua),
        ("Accept".to_string(), "*/*".to_string()),
        ("Accept-Encoding".to_string(), "identity".to_string()),
    ];
    client
        .request(
            "GET",
            path,
            &headers,
            None,
            PROBE_TIMEOUT.min(ep.read_timeout.max(1.0)),
            1,
        )
        .map_err(|e| e.to_string())
}

// ==========================================================================
// 三、用凭据登录，把候选教学班拉下来
// ==========================================================================

/// 这套系统的**选课类型**（请求里的 `teachingClassType`）。
///
/// 这不是"某校专用数据"，而是这套教务系统的协议常量 —— 和 `paths` 里那些
/// `{base}/elective/volunteer.do` 同一个性质，脱敏后的示例配置里也带着 `FAWKC`。
/// 它的用处只有一个：用户配的那一类里查不到课时，**换别的类型再问一次**，
/// 好告诉他"这门课其实是方案内课程"，而不是让他对着"换个关键词试试"发愣。
///
/// 注意 `TJKC`（本班推荐）走的是另一个接口（`recommendedCourse.do`），这里不带它；
/// `FXKC`（辅修）有些账号会被学校直接拒。
pub const COURSE_TYPES: &[(&str, &str)] = &[
    ("FANKC", "方案内课程"),
    ("FAWKC", "方案外课程"),
    ("XGXK", "校公选课"),
    ("TYKC", "体育课程"),
    ("MOOC", "慕课"),
];

/// `FANKC` → `方案内课程`；不认识的代码原样返回。
pub fn type_name(code: &str) -> String {
    let c = code.trim();
    COURSE_TYPES
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(c))
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| c.to_string())
}

/// `FANKC` → `方案内课程（FANKC）`；不认识的代码原样返回。
pub fn type_label(code: &str) -> String {
    let c = code.trim();
    match COURSE_TYPES.iter().find(|(k, _)| k.eq_ignore_ascii_case(c)) {
        Some((k, name)) => format!("{name}（{k}）"),
        None if c.is_empty() => "（没填课程类型）".to_string(),
        None => c.to_string(),
    }
}

/// 拉回来的一个教学班。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub tc_id: String,
    /// 这门课属于哪一类（`teachingClassType`）—— 提交选课时要带对
    pub tc_type: String,
    /// 课程名（界面上筛选用）
    pub course_name: String,
    pub index: String,
    pub teacher: String,
    pub place: String,
    /// 冲突组（星期-节次），从上课地点里推出来的；推不出来就是空串
    pub group: String,
    pub credit: String,
    pub is_full: String,
    pub is_conflict: String,
}

impl Found {
    /// 写进 `course.candidates` 用的可读标签。
    pub fn label(&self) -> String {
        let mut s = String::new();
        if !self.index.is_empty() {
            s.push_str(self.index.trim());
            if !s.ends_with('班') {
                s.push('班');
            }
            s.push(' ');
        }
        if !self.teacher.is_empty() {
            s.push_str(self.teacher.trim());
            s.push(' ');
        }
        if !self.group.is_empty() {
            s.push_str(&self.group);
        }
        let s = s.trim().to_string();
        if s.is_empty() {
            self.tc_id.clone()
        } else {
            s
        }
    }

    /// 界面上那一行列出来的样子：**类型 + 课程名放在最前面** ——
    /// 同一门课在方案内 / 方案外 / 校公选下都可能开、名字还一样，不标出来没法挑。
    pub fn display(&self) -> String {
        format!(
            "{:<10} {:<14} {:>3}  {:<7} {:<11} {:<18} 满={} 冲突={} {}",
            type_name(&self.tc_type),
            crate::grab::short(&self.course_name, 14),
            self.index,
            self.teacher,
            self.group,
            crate::grab::short(&self.place, 18),
            if self.is_full.is_empty() {
                "-"
            } else {
                &self.is_full
            },
            if self.is_conflict.is_empty() {
                "-"
            } else {
                &self.is_conflict
            },
            self.tc_id
        )
    }

    /// 这一行是不是匹配筛选词（课程名 / 教师 / 教学班号 / 类型名都能搜）。
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let n = needle.to_lowercase();
        [
            self.course_name.as_str(),
            self.teacher.as_str(),
            self.tc_id.as_str(),
            type_name(&self.tc_type).as_str(),
            self.index.as_str(),
        ]
        .iter()
        .any(|s| s.to_lowercase().contains(&n))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub student_name: String,
    pub batch_code: String,
    pub batch_name: String,
    pub campus: String,
    pub rows: Vec<Found>,
    /// 这次真正用上的会话 Cookie 名单（可能比配置里列的多 —— 见下）
    pub session_cookies: Vec<String>,
    /// 这一次拉到的类型清单：`(代码, 名字, 个数)`，按 COURSE_TYPES 顺序
    pub types: Vec<(String, String, usize)>,
}

/// 登录 → 读学生状态（拿批次与校区）→ 查课程目录 → 挑出含 `keyword` 的教学班。
///
/// 全程只读：登录本身不改任何选课状态，`program.do` 是查询接口。
/// 进度用 `log::log` 汇报（TUI 会把它接进自己的日志面板）。
pub fn fetch_catalog(
    ep: &Arc<Endpoints>,
    creds: &Credentials,
    solver: &dyn Solver,
    captcha_attempts: i64,
    gap: f64,
) -> Result<Fetched, String> {
    let mut ep = Arc::clone(ep);
    log::log(&format!(
        "登录 {}（学号 {}）…",
        ep.host,
        crate::auth::mask_id(&creds.student_id)
    ));
    let mut session = login_once(&ep, creds, solver, captcha_attempts, gap)?;
    log::log(&format!("      登录成功：{}", session.name));

    // 服务器下发的会话 Cookie 可能比配置里列的多。配置里漏一个，登录态就带不全
    // （脱敏后的示例配置就少了 `_WEU` 这种）—— 现场发现就补上，并重登一次用上新名单。
    let widened = widen_session_cookies(&ep.session_cookies, &session.observed_cookies);
    if widened != ep.session_cookies {
        log::log(&format!(
            "      服务器还下发了 {}，配置里没列全 —— 补上后重新登录一次",
            session
                .observed_cookies
                .iter()
                .filter(|n| !ep.session_cookies.contains(n))
                .cloned()
                .collect::<Vec<_>>()
                .join("、")
        ));
        ep = Arc::new(Endpoints {
            session_cookies: widened.clone(),
            ..(*ep).clone()
        });
        session = login_once(&ep, creds, solver, captcha_attempts, gap)?;
    }

    let referer = LoginSession::build_referer(&ep, &session.token);
    let school = School::new(
        Arc::clone(&ep),
        &session.token,
        &session.cookie,
        &referer,
        4.0,
        None,
    );
    school.set_code(&creds.student_id);

    log::log("读学生状态（拿当前批次与校区）…");
    let payload = school
        .student(&creds.student_id, false)
        .map_err(|e| format!("读学生状态失败：{}", e.message()))?;
    let data = payload.object("data").cloned().unwrap_or(Json::Null);
    let batch_info = data.object("electiveBatch").cloned().unwrap_or(Json::Null);
    let batch_code = batch_info.text("code");
    if batch_code.trim().is_empty() {
        return Err("学校没返回选课批次 —— 现在多半不是选课时间（批次是空的）。\
             配置可以先存着，等选了课的时间再回来拉课程目录。"
            .to_string());
    }
    let campus = {
        let c = data.text("campus");
        if c.trim().is_empty() {
            "01".to_string()
        } else {
            c
        }
    };
    log::log(&format!(
        "      批次 {} / {} / {}{}",
        batch_info.text("name"),
        batch_info.text("typeName"),
        batch_info.text("tacticName"),
        if batch_info.text("name").contains("预选") {
            "  ⚠ 批次名含「预选」：预选是抽签，抢课没意义"
        } else {
            ""
        }
    ));

    // **把每一类课整类拉下来**，而不是让用户先猜关键词。
    // 同一门课在方案内/方案外/校公选下都可能开、名字还一样，只搜名字根本分不清；
    // 拉全量之后由界面负责筛选和展示类型。
    log::log("拉课程目录（方案内 / 方案外 / 校公选 / 体育 / 慕课，各自翻页拉全）…");
    let mut rows: Vec<Found> = Vec::new();
    let mut types: Vec<(String, String, usize)> = Vec::new();
    for (code, name) in COURSE_TYPES {
        let before = rows.len();
        let mut page = 0i64;
        let mut courses_seen = 0usize;
        loop {
            let got = school
                .catalog_page(
                    &creds.student_id,
                    &batch_code,
                    &campus,
                    "",
                    code,
                    crate::grab::query_template_for(code),
                    page,
                )
                .map_err(|e| format!("查课程目录失败（{name}）: {}", e.message()))?;
            let (courses, total) = (got.courses, got.total);
            for r in got.rows {
                rows.push(found_of(r, code));
            }
            courses_seen += courses;
            page += 1;
            // 停下来：这一页没课了 / 已经够 totalCount 门课了 / 这一页不满（服务端没给
            // totalCount 时只能靠这个判断）/ 页数到顶
            if courses == 0
                || (total >= 0 && courses_seen as i64 >= total)
                || courses < crate::grab::PAGE_SIZE as usize
                || page as usize >= MAX_PAGES
            {
                break;
            }
        }
        let got_here = rows.len() - before;
        log::log(&format!(
            "      {name}（{code}）：{got_here} 个教学班{}",
            if page > 1 {
                format!("（{page} 页）")
            } else {
                String::new()
            }
        ));
        if got_here > 0 {
            types.push(((*code).to_string(), (*name).to_string(), got_here));
        }
        if rows.len() >= MAX_ROWS {
            log::log(&format!("      （已经够多了，先拉到这里：{MAX_ROWS} 条）"));
            break;
        }
    }

    if rows.is_empty() {
        return Err("这几类课一门都没有 —— 现在多半不是选课时间（目录是空的）。\
             配置可以先存着，等选了课的时间再回来拉。"
            .to_string());
    }

    Ok(Fetched {
        student_name: session.name,
        batch_code,
        batch_name: batch_info.text("name"),
        campus,
        rows,
        session_cookies: widened,
        types,
    })
}

fn found_of(r: crate::grab::CatalogRow, tc_type: &str) -> Found {
    let group = clock::time_group(&r.place);
    Found {
        tc_id: r.tc_id,
        tc_type: tc_type.to_string(),
        course_name: r.course_name,
        index: r.index,
        teacher: r.teacher,
        place: r.place,
        group,
        credit: r.credit,
        is_full: r.is_full,
        is_conflict: r.is_conflict,
    }
}

fn login_once(
    ep: &Arc<Endpoints>,
    creds: &Credentials,
    solver: &dyn Solver,
    captcha_attempts: i64,
    gap: f64,
) -> Result<LoginSession, String> {
    Login::new(creds, solver, ep, captcha_attempts, gap, ep.read_timeout)
        .login()
        .map_err(|e| format!("自动登录失败：{}", e.message()))
}

/// 配置里那份会话 Cookie 名单，加上登录时实测到、但名单里没有的那些。
///
/// 顺序保持"配置里写的优先，新发现的后排"——`set_cookie_values` 是按这个顺序拼
/// Cookie 头的，稳定的顺序让两次请求的字节更容易对齐（真实学校的 WAF 对这个敏感）。
fn widen_session_cookies(configured: &[String], observed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = configured.to_vec();
    for n in observed {
        if !out.iter().any(|x| x == n) {
            out.push(n.clone());
        }
    }
    out
}

/// 把勾选的教学班写进第 `ci` 门课的 `candidates`（整段替换），返回写入条数。
///
/// `keyword` 传空串表示"不动那门课的 keyword"；传了就写进去（抢课时的白名单标签用它），
/// 并在那门课还没有名字时顺手把名字也填上。
pub fn write_candidates(raw: &mut Json, ci: usize, picked: &[&Found], keyword: &str) -> usize {
    let mut arr: Vec<Json> = Vec::with_capacity(picked.len());
    for f in picked {
        arr.push(Json::obj(vec![
            ("id", Json::str(f.tc_id.clone())),
            ("label", Json::str(f.label())),
            ("group", Json::str(f.group.clone())),
            // 类型写进每个候选项：一次可能同时挑方案内 + 方案外，
            // 提交时每一发都要带对类别（config.rs 的 Candidate.tc_type）
            ("type", Json::str(f.tc_type.clone())),
        ]));
    }
    let n = arr.len();
    // 老配置（只有 `course`）先归一成 `courses`，否则这里会写进一个没人读的键
    crate::config::normalize_courses(raw);
    let courses = raw.ensure_arr("courses");
    if ci >= courses.len() {
        return 0;
    }
    let course = &mut courses[ci];
    course.set_key("candidates", Json::Arr(arr));
    if !keyword.trim().is_empty() {
        course.set_key("keyword", Json::str(keyword.trim()));
        if course.text("name").trim().is_empty() {
            course.set_key("name", Json::str(keyword.trim()));
        }
    }
    // 这门课挑的候选里出现了哪几类，就把 course.class_type 定成第一类 ——
    // 目录查询要用它（一门口径不对就会"查不到这门课"）。每个候选自己还带着 type，
    // 所以跨类型挑选时提交报文照样是对的。
    if let Some(first) = picked.iter().find(|f| !f.tc_type.is_empty()) {
        if course.text("class_type").trim().is_empty() {
            course.set_key("class_type", Json::str(first.tc_type.clone()));
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// 用户实际会粘进来的四种形状（前两种是"只有域名"，靠服务器告诉我们路径）。
    fn derives_the_four_real_shapes() {
        let cases = [
            // 只有域名
            ("jw.example.edu.cn", "/xs/course/app"),
            // 域名 + 根路径
            ("http://jw.example.edu.cn/", "/xs/course/app"),
            // 选课页（这次脱敏前项目的真实地址）
            (
                "http://jw.example.edu.cn/xs/course/app/*default/index.do",
                "/xs/course/app",
            ),
            // 浏览器地址栏里那条（带 token，用户最可能粘的就是这条）
            (
                "http://jw.example.edu.cn/xs/course/app/*default/grablessons.do?token=0f9a1c2e-1111-4222-8333-444455556666",
                "/xs/course/app",
            ),
        ];
        for (input, want_base) in cases {
            let parts = parse_input(input).expect(input);
            assert_eq!(parts.host, "jw.example.edu.cn", "{input}");
            assert_eq!(parts.port, 80, "{input}");
            if input.contains("*default") {
                let d = derive_from_path(&parts, &parts.path);
                assert_eq!(d.base_path.as_deref(), Some(want_base), "{input}");
                assert_eq!(
                    d.page_path.as_deref(),
                    Some("/xs/course/app/*default/index.do"),
                    "{input}"
                );
            } else {
                // 只有域名：解析阶段就该问服务器要路径
                assert!(parts.bare, "{input} 应当被认成「只有域名」");
            }
            // https / 带端口也照样能解析
        }
        let https =
            parse_input("https://jw.example.edu.cn/xs/course/app/*default/index.do").unwrap();
        assert_eq!(https.port, 443);
        let port = parse_input("http://jw.example.edu.cn:8080/a/sys/a/*default/index.do").unwrap();
        assert_eq!(port.port, 8080);
    }

    /// 接口前缀 = `*default` 前面那一整段上下文（**不是** 再拼一个 `/api`）。
    ///
    /// 这条是拿真实系统量出来的：`config.example.json` 里那个 `/course-system/api`
    /// 是脱敏时换上去的，照着它会推成 `/xs/course/app/api` —— 全 404。
    #[test]
    fn base_path_is_the_context_not_context_plus_api() {
        let parts =
            parse_input("http://jw.example.edu.cn/xs/course/app/*default/grablessons.do?token=t")
                .unwrap();
        let d = derive_from_path(&parts, &parts.path);
        assert_eq!(d.base_path.as_deref(), Some("/xs/course/app"));
        assert_eq!(d.token.as_deref(), Some("t"));
        assert!(d.notes.iter().any(|n| n.contains("token")), "{:?}", d.notes);
        // 挂在根上的应用（上下文是空串）
        let parts = parse_input("http://x.example.edu.cn/*default/index.do").unwrap();
        let d = derive_from_path(&parts, &parts.path);
        assert_eq!(d.base_path.as_deref(), Some(""));
        assert_eq!(d.page_path.as_deref(), Some("/*default/index.do"));
        // 认不出 `*default`：只填域名端口，并明说
        let parts = parse_input("http://x.example.edu.cn/jwglxt/xtgl/login_slogin.html").unwrap();
        let d = derive_from_path(&parts, &parts.path);
        assert!(d.base_path.is_none() && d.page_path.is_none());
        assert_eq!(d.host, "x.example.edu.cn");
    }

    #[test]
    fn rejects_junk() {
        assert!(parse_input("").is_err());
        assert!(parse_input("   ").is_err());
        assert!(parse_input("ftp://x.example.edu.cn/a").is_err());
        assert!(parse_input("http://x.example.edu.cn:abc/a").is_err());
        assert!(parse_input("http:///a.do").is_err());
        assert!(parse_input("http://x.example.edu.cn:99999/a").is_err());
        assert!(parse_input("http://x.example.edu.cn/a b.do").is_err());
        assert!(parse_input("http://x .edu.cn/a").is_err());
    }

    #[test]
    fn location_and_embedded_path_parsing() {
        // Location 可能是绝对 URL，也可能是相对路径
        assert_eq!(
            location_path("http://jw.example.edu.cn/xs/course/app/*default/index.do"),
            "/xs/course/app/*default/index.do"
        );
        assert_eq!(location_path("/a/b.do?x=1"), "/a/b.do");
        assert_eq!(location_path("b.do"), "b.do");
        assert_eq!(location_path(""), "/");

        // 服务器把路径写在页面里时也要捞得出来
        assert_eq!(
            find_default_path(r#"<a href="/xs/course/app/*default/index.do">x</a>"#).as_deref(),
            Some("/xs/course/app/*default/index.do")
        );
        assert_eq!(
            find_default_path("var u='/a/sys/b/*default/grablessons.do'").as_deref(),
            Some("/a/sys/b/*default/grablessons.do")
        );
        assert!(find_default_path("没有这种东西").is_none());
        // 别把相对片段当成绝对路径
        assert!(find_default_path("x/*default/y.do").is_none());
    }

    #[test]
    fn apply_writes_the_school_section_and_fills_the_rest() {
        let mut raw =
            json::parse(r#"{"school":{"host":"course.example.edu.cn","login_page":"/x"}}"#)
                .unwrap();
        let parts =
            parse_input("http://jw.example.edu.cn/course-system/*default/grablessons.do?token=t")
                .unwrap();
        let d = derive_from_path(&parts, &parts.path);
        let notes = apply(&mut raw, &d, Some("/course-system/"));

        assert_eq!(
            raw.object("school").unwrap().text("host"),
            "jw.example.edu.cn"
        );
        assert_eq!(
            raw.object("school").unwrap().get("port").unwrap().as_i64(),
            Some(80)
        );
        // 接口前缀 = 页面路径里的上下文（真实系统就是这么长的：/xs/course/app）
        assert_eq!(
            raw.object("school").unwrap().text("base_path"),
            "/course-system"
        );
        // 对时页是实测挑出来的那一页
        assert_eq!(
            raw.object("school").unwrap().text("time_path"),
            "/course-system/"
        );
        assert_eq!(
            raw.object("school").unwrap().text("page_path"),
            "/course-system/*default/index.do"
        );
        // login_page 是死键，但既然文件里有就跟着改，别自相矛盾
        assert_eq!(
            raw.object("school").unwrap().text("login_page"),
            "/course-system/*default/grablessons.do"
        );
        // 缺的键按参考实现补齐
        assert!(raw.object("paths").is_some());
        assert_eq!(raw.object("paths").unwrap().array("volunteer").len(), 0);
        assert_eq!(
            raw.object("paths").unwrap().text("student"),
            "{base}/student/{code}.do"
        );
        assert_eq!(raw.object("password").unwrap().array("des_keys").len(), 3);
        assert_eq!(raw.array("courses")[0].text("class_type"), "FAWKC");
        // 用户的数据不会被凭空造出来
        assert!(raw.array("courses")[0].get("candidates").is_none());
        assert!(raw.array("courses")[0].get("keyword").is_none());
        // Referer 故意不填
        assert_eq!(raw.object("school").unwrap().text("referer_path"), "");
        assert!(notes
            .iter()
            .any(|(l, t)| *l == Level::Warn && t.contains("参考实现")));
    }

    #[test]
    fn apply_never_overwrites_what_the_user_wrote() {
        let mut raw = json::parse(
            r#"{"school":{"host":"old","base_path":"/mine/api"},
                "paths":{"volunteer":"/mine/volunteer.do"},
                "password":{"des_keys":["k1"]},
                "course":{"keyword":"我自己的课","candidates":[{"id":"1","label":"x","group":"g"}]}}"#,
        )
        .unwrap();
        let parts = parse_input("http://new.host/course-system/*default/grablessons.do").unwrap();
        let d = derive_from_path(&parts, &parts.path);
        apply(&mut raw, &d, None);

        // 网址能决定的四项：覆盖（接口前缀 = 页面路径里的上下文，把用户原来的值换掉）
        assert_eq!(raw.object("school").unwrap().text("host"), "new.host");
        assert_eq!(
            raw.object("school").unwrap().text("base_path"),
            "/course-system"
        );
        // 用户写过的键：一个字都不动
        assert_eq!(
            raw.object("paths").unwrap().text("volunteer"),
            "/mine/volunteer.do"
        );
        assert_eq!(raw.object("password").unwrap().array("des_keys").len(), 1);
        // 用户那门课一个字都没动 —— 只是被搬进了 courses 数组（界面/写回只认它）
        let course = raw.array("courses")[0].clone();
        assert_eq!(course.text("keyword"), "我自己的课");
        assert_eq!(course.array("candidates").len(), 1);
        assert!(
            raw.get("course").is_none(),
            "旧键搬家后该清掉，免得两份并存"
        );
        // 但缺的键还是补上了
        assert_eq!(
            raw.object("paths").unwrap().text("capacity"),
            "{base}/elective/teachingclass/capacity.do"
        );
    }

    #[test]
    fn fill_reference_skips_comments_and_is_idempotent() {
        let mut raw = json::parse(r#"{}"#).unwrap();
        let first = fill_reference(&mut raw);
        assert!(first.iter().any(|(l, _)| *l == Level::Warn));
        let after = raw.to_compact();
        // 说明性的 _comment 键不该被搬进来（那是模板的注释，不是配置）
        assert!(!after.contains("_comment"), "{after}");
        // 再补一次什么都不该发生
        let second = fill_reference(&mut raw);
        assert!(second.iter().all(|(l, _)| *l == Level::Ok), "{second:?}");
        assert_eq!(raw.to_compact(), after);
    }

    #[test]
    fn candidate_labels_and_write_back() {
        let f = Found {
            tc_id: "000000000000000000000001".to_string(),
            tc_type: "FANKC".to_string(),
            course_name: "高等数学".to_string(),
            index: "01".to_string(),
            teacher: "张三".to_string(),
            place: "星期一第3-5节 一教101".to_string(),
            group: clock::time_group("星期一第3-5节 一教101"),
            credit: "3.0".to_string(),
            is_full: "0".to_string(),
            is_conflict: "0".to_string(),
        };
        assert_eq!(f.group, "星期一-3-5");
        assert_eq!(f.label(), "01班 张三 星期一-3-5");

        // 老格式（只有 course）也要能写进去 —— 写回前会先归一成 courses
        let mut raw = json::parse(r#"{"course":{"keyword":"旧"}}"#).unwrap();
        let n = write_candidates(&mut raw, 0, &[&f], "高等数学");
        assert_eq!(n, 1);
        assert!(raw.get("course").is_none(), "旧键该被清掉");
        let course = raw.array("courses")[0].clone();
        let cands = course.array("candidates").to_vec();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].text("id"), "000000000000000000000001");
        assert_eq!(cands[0].text("group"), "星期一-3-5");
        assert_eq!(course.text("keyword"), "高等数学");
        assert_eq!(course.text("name"), "高等数学", "没名字就拿关键词当名字");
        assert_eq!(course.text("class_type"), "FANKC", "类别空着就跟着候选填");
    }

    #[test]
    fn judge_uses_the_control_path() {
        assert_eq!(judge(200, Some(404)).0, Level::Ok);
        assert_eq!(judge(404, Some(404)).0, Level::Err);
        assert_eq!(judge(302, Some(404)).0, Level::Ok);
        assert_eq!(judge(405, Some(404)).0, Level::Ok);
        assert_eq!(judge(200, Some(200)).0, Level::Warn);
        assert_eq!(judge(500, Some(404)).0, Level::Warn);
        assert_eq!(judge(200, None).0, Level::Warn);
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("abc"), "abc");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(
            query_param("a=1&token=x%3Dy", "token").as_deref(),
            Some("x=y")
        );
        assert!(query_param("a=1", "token").is_none());
    }
}
