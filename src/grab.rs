//! 教务系统抢课 · 放课窗口精准首发（对应原版 `grab.py`）
//!
//! 对接的是常见的"志愿制"选课系统：提交志愿 → 服务端异步处理 → 已选课程列表是唯一真值。
//! 所有学校相关的地址、路径、Cookie 名、密码加密密钥都在 config.json 里，本文件只负责逻辑。
//!
//! 用到的接口（名字是逻辑名，实际路径在 config.json 的 paths 里配置）:
//!     volunteer   POST  提交选课志愿          body: addParam={"data":{...}}   header: token
//!     capacity    POST  查教学班余量
//!     result      POST  已选课程（唯一真值）
//!     status      POST  异步处理状态
//!     sysparam    POST  服务器毫秒时间戳
//!     program     POST  课程目录（用来建候选白名单）
//!     student     POST  学生信息（批次、校区、学分上下限）
//!
//! 安全设计（硬约束，写在代码里）:
//!     1. 本文件不存在任何退选/删除志愿的代码路径，全文不含 `operationType":"2"`。
//!        没有任何函数能移除已选课程。
//!     2. 默认只做只读预检 + 计时演练；必须显式传 --live 才会调用选课提交接口。
//!     3. 预检发现目标教学班已在"已选课程"里 -> 立即退出，不做任何提交。
//!     4. 只对预检阶段从课程目录解析出的候选教学班白名单提交，绝不提交白名单以外的。
//!     5. 首发默认打组内前 3 个候选、彼此错开（--single 可退回只打第一个）；
//!        所有写请求共用同一份"每滚动窗口 N 发"的额度（WritePacer）。
//!     6. 自动登录只用本机凭据文件里的学号+密码；一旦服务端回"登录名或密码不正确"，
//!        立刻熔断整条自动登录链路，绝不用错误密码反复试探（避免账号被锁）。

use std::collections::HashSet;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use crate::auth::{self, Credentials, LoginSession, ReloginManager};
use crate::captcha::{Captcha, Solver};
use crate::cli::Args;
use crate::clock::{self, DateSample};
use crate::config::{self, Candidate, CourseCfg, Endpoints};
use crate::httpc::{self, Response};
use crate::json::{self, Json};
use crate::log::{self, info};
use crate::pacer::WritePacer;
use crate::timeutil;

// --------------------------------------------------------------------------
// 服务端返回文案分类（中文教务系统常见措辞 + 实测响应；换学校可按需增删）
// --------------------------------------------------------------------------
const SUCCESS_WORDS: [&str; 3] = ["添加选课志愿成功", "添加选课成功", "选课成功"];
const ALREADY_WORDS: [&str; 7] = [
    "已经选过",
    "已选过",
    "已经选择",
    "重复选课",
    "已存在",
    "已选该课程",
    "已经选课",
];
const FULL_WORDS: [&str; 11] = [
    "超过课容量",
    "该课程超过课容量",
    "课容量已满",
    "课程容量已满",
    "超过课程容量",
    "选课人数已满",
    "课程人数已满",
    "教学班容量已满",
    "该教学班已满",
    "容量已满",
    "人数已满",
];
const CONFLICT_WORDS: [&str; 4] = ["时间冲突", "上课时间冲突", "与已选课程时间冲突", "课程冲突"];
const WINDOW_WORDS: [&str; 14] = [
    "当前时间不在选课开放时间范围内",
    "不在选课开放时间",
    "未在选课开放时间",
    "不在开放时间范围内",
    "不在选课时间",
    "不在补选时间",
    "非选课时间",
    "未到选课时间",
    "选课时间未到",
    "选课时间已过",
    "选课尚未开始",
    "未开放",
    "已结束",
    "已截止",
];
const BUSY_WORDS: [&str; 14] = [
    "系统繁忙",
    "服务繁忙",
    "请稍后再试",
    "请求频繁",
    "操作频繁",
    "网络繁忙",
    // 实测：20:00 放课瞬间学校会对高频请求直接限流
    "请求过快",
    "请求太快",
    "请求过于频繁",
    "访问过快",
    "访问过于频繁",
    "提交过快",
    "提交过于频繁",
    "too many requests",
];
/// 学校在放课前后会短暂重排数据，此时任何提交都会被拒，属于可重试
const INIT_WORDS: [&str; 8] = [
    "系统正在初始化",
    "正在初始化",
    "系统初始化",
    "请稍候",
    "请稍后",
    "系统维护",
    "正在处理",
    "系统升级",
];
const TERMINAL_WORDS: [&str; 11] = [
    "超过学分",
    "学分已满",
    "学分已达上限",
    "学分已达到上限",
    "选课门数已达上限",
    "选课门数已达到上限",
    "志愿数已达上限",
    "志愿数已达到上限",
    "不是选课对象",
    "无权限",
    "不能选课",
];
const EXPIRED_WORDS: [&str; 9] = [
    "登录超时",
    "未登录",
    "请重新登录",
    "登录已过期",
    "未登录用户",
    "登录状态已过期",
    // 实测：volunteer.do 被限流后，学校会把会话作废，之后每一发都返回这句。
    // 它不含"未登录"字样，之前靠巧合才被归到过期，必须显式登记。
    "请求数据与登录者身份不一致",
    "身份不一致",
    "非法请求",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// code=1，学校已受理，进入异步处理
    Submitted,
    /// 已选过 / 重复选课 -> 需复核
    Duplicate,
    /// 容量满 -> 换下一个候选
    Full,
    /// 时间冲突 -> 该候选终止
    Conflict,
    /// 学分/权限等终态失败 -> 该候选终止
    Terminal,
    /// 未到放课时间 -> 继续等
    WindowClosed,
    /// 系统繁忙 -> 退避重试
    Busy,
    /// 网关 5xx / 超时 / HTML 错误页 -> 服务器过载，保持节奏重试
    Overload,
    /// 会话过期 -> 停止，要求重新提供会话
    Expired,
    Unknown,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Submitted => "SUBMITTED",
            Verdict::Duplicate => "DUPLICATE",
            Verdict::Full => "FULL",
            Verdict::Conflict => "CONFLICT",
            Verdict::Terminal => "TERMINAL",
            Verdict::WindowClosed => "WINDOW_CLOSED",
            Verdict::Busy => "BUSY",
            Verdict::Overload => "OVERLOAD",
            Verdict::Expired => "EXPIRED",
            Verdict::Unknown => "UNKNOWN",
        }
    }
}

pub fn debug_writes() -> bool {
    std::env::var("GRAB_DEBUG_WRITES")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Python 的 `not payload`：null / 空对象 / 空数组都算"没解析出东西"。
fn falsy(v: &Json) -> bool {
    match v {
        Json::Null => true,
        Json::Bool(b) => !*b,
        Json::Int(i) => *i == 0,
        Json::Float(f) => *f == 0.0,
        Json::Str(s) => s.is_empty(),
        Json::Arr(a) => a.is_empty(),
        Json::Obj(o) => o.is_empty(),
    }
}

/// 会话是不是过期了。判定顺序与原版一致。
pub fn is_expired(payload: &Json, status: u16, text: &str) -> bool {
    if matches!(status, 301 | 302 | 401 | 403) {
        return true;
    }
    if matches!(payload.text("code").as_str(), "302" | "401" | "403") {
        return true;
    }
    let low = text.to_lowercase();
    if low.contains("student/check/login") {
        return true;
    }
    low.contains("vtoken") && low.contains("loginpwd")
}

/// 把学校返回归类成一个动作。
pub fn classify(payload: &Json, status: u16, text: &str) -> (Verdict, String) {
    let msg = payload.text("msg").trim().to_string();
    let code = payload.text("code");
    let hay = format!("{msg}\n{text}");

    // 限流 / 初始化必须最先判：实测放课瞬间学校会返回
    // 「请求过快，请登录后再试」和「选课系统正在初始化,请稍候...」，
    // 前者带「登录」字样、且常常伴随 code=302。若按会话过期处理会直接放弃整个任务，
    // 而它们其实都是"退避后重试"就能过的。
    if BUSY_WORDS.iter().any(|w| hay.contains(w)) || INIT_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::Busy, msg);
    }

    // 放课瞬间服务器被打爆时，写请求根本回不来或者被网关拦掉。这既不是业务失败，
    // 也不是会话过期 —— 必须单独归类，否则会被当成 UNKNOWN 而丢掉信息。
    if status >= 500 {
        return (
            Verdict::Overload,
            if msg.is_empty() {
                format!("HTTP {status}（网关错误）")
            } else {
                msg
            },
        );
    }
    if status == 0 {
        // 我们自己在写请求失败时把原始错误塞进了 {"error": "..."}，这里带上它 ——
        // "超时"和"连接被断"是两种完全不同的故障，日志里得能分清。
        let detail = payload.text("error");
        let out = if !msg.is_empty() {
            msg
        } else if detail.is_empty() {
            "没有拿到响应（超时/连接被断）".to_string()
        } else {
            format!("没有拿到响应: {detail}")
        };
        return (Verdict::Overload, out);
    }
    let low = text.trim_start().to_lowercase();
    if falsy(payload) && low.starts_with('<') {
        return (Verdict::Overload, "网关返回了 HTML 错误页".to_string());
    }

    if is_expired(payload, status, text) {
        return (
            Verdict::Expired,
            if msg.is_empty() {
                "登录已过期".to_string()
            } else {
                msg
            },
        );
    }
    if code == "1" || SUCCESS_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::Submitted, msg);
    }
    if ALREADY_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::Duplicate, msg);
    }
    if CONFLICT_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::Conflict, msg);
    }
    if FULL_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::Full, msg);
    }
    if WINDOW_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::WindowClosed, msg);
    }
    if TERMINAL_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::Terminal, msg);
    }
    if EXPIRED_WORDS.iter().any(|w| hay.contains(w)) {
        return (Verdict::Expired, msg);
    }
    let fallback: String = text.chars().take(80).collect();
    (
        Verdict::Unknown,
        if msg.is_empty() { fallback } else { msg },
    )
}

// ==========================================================================
// 学校会话
// ==========================================================================

#[derive(Debug)]
pub enum SchoolError {
    /// 学校明确说会话没了（只读请求会据此自动重登）
    Expired(String),
    Other(String),
}

impl SchoolError {
    pub fn message(&self) -> &str {
        match self {
            SchoolError::Expired(m) | SchoolError::Other(m) => m,
        }
    }
}

impl std::fmt::Display for SchoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

#[derive(Clone)]
struct Session {
    token: String,
    cookie: String,
    referer: String,
}

/// 一次写请求的结果（对应原版的 4 元组）。
#[derive(Clone)]
pub struct WriteResult {
    pub payload: Json,
    pub status: u16,
    pub text: String,
    pub ms: f64,
}

impl WriteResult {
    fn from_error(e: &str) -> WriteResult {
        // 注意要把它**解析成 payload**：classify 靠 payload 里的 error 字段
        // 把"超时"和"连接被断"分开，光塞进 text 是看不到的。
        let text = format!("{{\"error\":{}}}", Json::str(e).to_compact());
        WriteResult {
            payload: json::parse_or_empty(&text),
            status: 0,
            text,
            ms: 0.0,
        }
    }
}

/// 极简学校客户端。提交路径与只读路径分开，避免误用。
///
/// 会话恢复（`auth` 不为 None 时）：只读请求一旦被学校判为"会话过期"，
/// 就自动重新登录一次并重放该请求；提交请求**不**自动重放（见 `json_post`）。
pub struct School {
    pub ep: Arc<Endpoints>,
    session: Mutex<Session>,
    /// 学号，预检阶段填入
    code: Mutex<String>,
    pub auth: Option<Mutex<ReloginManager>>,
    /// 写请求节拍器（首发与重试共用）。只在 --live 时装上。
    pacer: OnceLock<Arc<WritePacer>>,
    pub write_timeout: f64,
    pub read_timeout: f64,
    /// 本次运行成功自动重登录的次数
    relogins: Mutex<u32>,
    client: Mutex<httpc::Client>,
    recover_lock: Mutex<()>,
    recovering: AtomicBool,
}

impl School {
    pub fn new(
        ep: Arc<Endpoints>,
        token: &str,
        cookie: &str,
        referer: &str,
        write_timeout: f64,
        auth: Option<ReloginManager>,
    ) -> Arc<School> {
        let read_timeout = ep.read_timeout;
        Arc::new(School {
            // 建连超时用 read_timeout（15s）：原版是 http.client.HTTPConnection(timeout=15)，
            // 那个 15 同时管建连与读写。connect_timeout（1.5s）只用于首发那几发的预热建连。
            client: Mutex::new(httpc::Client::new(&ep.host, ep.port, ep.read_timeout)),
            ep,
            session: Mutex::new(Session {
                token: token.to_string(),
                cookie: cookie.to_string(),
                referer: referer.to_string(),
            }),
            code: Mutex::new(String::new()),
            auth: auth.map(Mutex::new),
            pacer: OnceLock::new(),
            write_timeout,
            read_timeout,
            relogins: Mutex::new(0),
            recover_lock: Mutex::new(()),
            recovering: AtomicBool::new(false),
        })
    }

    pub fn set_pacer(&self, pacer: Arc<WritePacer>) {
        let _ = self.pacer.set(pacer);
    }

    fn pacer(&self) -> Option<&Arc<WritePacer>> {
        self.pacer.get()
    }

    fn session(&self) -> Session {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn code(&self) -> String {
        self.code.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_code(&self, code: &str) {
        *self.code.lock().unwrap_or_else(|e| e.into_inner()) = code.to_string();
    }

    /// 直接换一套 Cookie。
    pub fn use_cookie(&self, cookie: &str) {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cookie = cookie.to_string();
    }

    /// 换用一次自动登录拿到的新会话：token、Cookie、Referer 全部跟着换。
    pub fn adopt(&self, session: &LoginSession) {
        let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
        s.token = session.token.clone();
        s.cookie = session.cookie.clone();
        s.referer = session.referer.clone();
    }

    pub fn token(&self) -> String {
        self.session().token
    }

    pub fn relogins(&self) -> u32 {
        *self.relogins.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn auth_fatal(&self) -> Option<String> {
        self.auth
            .as_ref()
            .and_then(|a| a.lock().unwrap_or_else(|e| e.into_inner()).fatal.clone())
    }

    pub fn auth_why_not(&self) -> String {
        self.auth
            .as_ref()
            .map(|a| a.lock().unwrap_or_else(|e| e.into_inner()).why_not())
            .unwrap_or_default()
    }

    /// 放宽自动重登录的次数上限（放课窗口里用：窗口内不因为"试了几次"就放弃）。
    pub fn relax_relogin_limit(&self, at_least: i64) {
        if let Some(a) = self.auth.as_ref() {
            a.lock()
                .unwrap_or_else(|e| e.into_inner())
                .relax_limit(at_least);
        }
    }

    /// 只读接口确认会话是否有效。返回 (是否有效, 原始返回)。
    pub fn probe(&self) -> (bool, Json) {
        let path = format!(
            "{}?timestamp={}",
            self.ep.path_code("student", &self.code()),
            (timeutil::unix_now() * 1000.0) as i64
        );
        match self.json_post(&path, &[], false, 2, None) {
            Ok((payload, status, text)) => {
                let ok = !is_expired(&payload, status, &text) && payload.text("code") == "1";
                (ok, payload)
            }
            Err(e) => (
                false,
                json::parse_or_empty(&format!(
                    "{{\"msg\":{}}}",
                    Json::str(format!("网络错误: {e}")).to_compact()
                )),
            ),
        }
    }

    /// 会话死了就自动登回来。返回 true = 现在这套会话是好的。
    ///
    /// 进入时先自己复核一遍：并发路径（复核线程 + 重试线程）可能同时发现会话过期，
    /// 第二个进来的会发现"已经好了"，就不会再打一次登录接口。
    pub fn recover(&self, reason: &str) -> bool {
        if self.auth.is_none() || self.code().is_empty() {
            return false;
        }
        let _guard = self.recover_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (ok, _payload) = self.probe();
        if ok {
            return true;
        }
        self.recovering.store(true, Ordering::SeqCst);
        let session = {
            let mut mgr = self
                .auth
                .as_ref()
                .expect("上面判过")
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            mgr.relogin(reason)
        };
        let result = match session {
            None => {
                self.recovering.store(false, Ordering::SeqCst);
                false
            }
            Some(session) => {
                self.adopt(&session);
                self.recovering.store(false, Ordering::SeqCst);
                let (ok, payload) = self.probe();
                if ok {
                    true
                } else {
                    info(&format!("[auth] 新会话复验未通过: {}", payload.text("msg")));
                    false
                }
            }
        };
        if result {
            let n = {
                let mut n = self.relogins.lock().unwrap_or_else(|e| e.into_inner());
                *n += 1;
                *n
            };
            let token = self.token();
            info(&format!(
                "[auth] ✓ 会话就绪（本次运行第 {n} 次自动登录），新 token {}…{}",
                short(&token, 8),
                tail(&token, 4)
            ));
        }
        result
    }

    /// 业务接口的请求头（顺序与原版一致；Accept-Encoding 是 http.client 的默认行为，
    /// 不发的话服务器可能回 gzip，而我们不做解压）。
    fn headers(&self) -> Vec<(String, String)> {
        let s = self.session();
        vec![
            ("Host".to_string(), self.ep.host_header()),
            ("Accept-Encoding".to_string(), "identity".to_string()),
            (
                "Accept".to_string(),
                "application/json, text/javascript, */*; q=0.01".to_string(),
            ),
            (
                "Accept-Language".to_string(),
                "zh-CN,zh;q=0.9,en;q=0.8".to_string(),
            ),
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded; charset=UTF-8".to_string(),
            ),
            ("Cookie".to_string(), s.cookie),
            ("Origin".to_string(), self.ep.base_url.clone()),
            ("Referer".to_string(), s.referer),
            ("User-Agent".to_string(), self.ep.ua.clone()),
            ("X-Requested-With".to_string(), "XMLHttpRequest".to_string()),
            ("token".to_string(), s.token),
        ]
    }

    /// GET 一个轻量页面，返回 (服务器整秒, 本机中点, RTT)。只读。
    ///
    /// 只用来读 HTTP Date 头做时钟对齐。
    pub fn date_sample(&self) -> Option<DateSample> {
        let headers = vec![
            ("Host".to_string(), self.ep.host_header()),
            ("User-Agent".to_string(), self.ep.ua.clone()),
            ("Accept".to_string(), "*/*".to_string()),
            ("Accept-Encoding".to_string(), "identity".to_string()),
            ("Cache-Control".to_string(), "no-cache".to_string()),
            ("Pragma".to_string(), "no-cache".to_string()),
        ];
        let mut client = self.client.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..2 {
            // **必须用墙钟**：`mid` 要拿去减 Date 头里的 epoch（见 clock::server_offset），
            // 用单调钟（进程启动起算的小数）量纲对不上，网格投票会一个格都投不到，
            // 结果永远是"区间未收敛"、对时形同虚设。
            let t0 = timeutil::unix_now();
            let path = format!("{}?_={}", self.ep.time_path, (t0 * 1000.0) as i64);
            match client.request("GET", &path, &headers, None, 15.0, 1) {
                Ok(resp) => {
                    let t1 = timeutil::unix_now();
                    let raw = resp.header("Date")?;
                    let stamp = timeutil::parse_http_date(raw)?;
                    return Some(DateSample {
                        sec: stamp,
                        mid: (t0 + t1) / 2.0,
                        rtt: t1 - t0,
                    });
                }
                // 连接坏了就重连一次
                Err(_) => client.drop_conn(),
            }
        }
        None
    }

    /// 带 keep-alive 的 POST。见 `post_body`。
    pub fn post(
        &self,
        path: &str,
        form: &[(&str, String)],
        attempts: usize,
        timeout: f64,
    ) -> std::io::Result<Response> {
        self.post_body(path, &httpc::urlencode(form), attempts, timeout)
    }

    /// 已经是编码好的报文体时用它（写请求的报文体要复用于多处）。
    pub fn post_body(
        &self,
        path: &str,
        body: &str,
        attempts: usize,
        timeout: f64,
    ) -> std::io::Result<Response> {
        let headers = self.headers();
        let mut client = self.client.lock().unwrap_or_else(|e| e.into_inner());
        client.request(
            "POST",
            path,
            &headers,
            Some(body.as_bytes()),
            timeout,
            attempts,
        )
    }

    /// POST + 解析 JSON（只读路径：被学校判"会话过期"时自动重登录一次并重放请求）。
    pub fn json_post(
        &self,
        path: &str,
        form: &[(&str, String)],
        recover: bool,
        attempts: usize,
        timeout: Option<f64>,
    ) -> std::io::Result<(Json, u16, String)> {
        let t = timeout.unwrap_or(self.read_timeout);
        let resp = self.post(path, form, attempts, t)?;
        let text = resp.text();
        let mut payload = json::parse_or_empty(&text);
        let mut status = resp.status;
        let mut text = text;
        if recover
            && !self.recovering.load(Ordering::SeqCst)
            && self.auth.is_some()
            && is_expired(&payload, status, &text)
        {
            let name = path
                .split('/')
                .next_back()
                .unwrap_or(path)
                .split('?')
                .next()
                .unwrap_or(path)
                .to_string();
            info(&format!(
                "[auth] 只读接口 {name} 报会话过期，尝试自动重登录…"
            ));
            if self.recover(&format!("{name} 报会话过期")) {
                let resp = self.post(path, form, attempts, t)?;
                let t2 = resp.text();
                payload = json::parse_or_empty(&t2);
                status = resp.status;
                text = t2;
            }
        }
        Ok((payload, status, text))
    }

    /// 写请求专用的 POST + 解析 JSON。
    ///
    /// **绝不自动重放** —— 一发选课请求要么发一次，要么不发；"过期"的判定与后续动作
    /// 留给重试循环显式处理，避免重复提交。
    fn json_post_write(
        &self,
        path: &str,
        body: &str,
        timeout: Option<f64>,
    ) -> std::io::Result<(Json, u16, String)> {
        let t = timeout.unwrap_or(self.write_timeout);
        let resp = self.post_body(path, body, 1, t)?;
        let text = resp.text();
        let payload = json::parse_or_empty(&text);
        Ok((payload, resp.status, text))
    }

    // ---- 只读 ----

    pub fn student(&self, code: &str, recover: bool) -> Result<Json, SchoolError> {
        let path = format!(
            "{}?timestamp={}",
            self.ep.path_code("student", code),
            (timeutil::unix_now() * 1000.0) as i64
        );
        let (payload, _s, _t) = self
            .json_post(&path, &[], recover, 2, None)
            .map_err(|e| SchoolError::Other(e.to_string()))?;
        Ok(payload)
    }

    /// 已选课程（唯一真值）：返回教学班 ID 集合。
    pub fn enrolled_ids(&self) -> Result<HashSet<String>, SchoolError> {
        let path = format!(
            "{}?timestamp={}&studentCode={}",
            self.ep.path("result"),
            (timeutil::unix_now() * 1000.0) as i64,
            self.code()
        );
        let (payload, status, text) = self
            .json_post(&path, &[], true, 2, None)
            .map_err(|e| SchoolError::Other(e.to_string()))?;
        if is_expired(&payload, status, &text) {
            return Err(SchoolError::Expired(
                "courseResult 返回登录过期".to_string(),
            ));
        }
        if payload.text("code") != "1" {
            return Err(SchoolError::Other(format!(
                "已选课程查询失败: {}",
                payload.text("msg")
            )));
        }
        let mut out = HashSet::new();
        for row in payload.array("dataList") {
            let id = row.text("teachingClassID").trim().to_string();
            if !id.is_empty() {
                out.insert(id);
            }
        }
        Ok(out)
    }

    pub fn capacity(&self, tc_id: &str, batch: &str) -> Result<Json, SchoolError> {
        let (payload, status, text) = self
            .json_post(
                &self.ep.path("capacity"),
                &[
                    ("teachingClassId", tc_id.to_string()),
                    ("batchCode", batch.to_string()),
                ],
                true,
                2,
                None,
            )
            .map_err(|e| SchoolError::Other(e.to_string()))?;
        if is_expired(&payload, status, &text) {
            return Err(SchoolError::Expired("capacity 返回登录过期".to_string()));
        }
        Ok(payload.object("data").cloned().unwrap_or(Json::Null))
    }

    /// 从学校课程目录解析候选教学班（只读，用于建白名单）。
    ///
    /// `class_type` 就是请求里的 `teachingClassType`：**它决定服务器回哪一类课**
    /// （方案内 / 方案外 / 校公选 / 体育 / 慕课）。配置里那个 `course.class_type`
    /// 是默认值；配置界面会在"你配的那类里查不到"时换别的类型再问一次，
    /// 好告诉用户"这门课其实属于方案内"。
    /// 目录的一页。
    ///
    /// `page` 从 0 开始（前端就是这么传的）。分页要看的两个数是**课程条数**：
    /// `totalCount` 是课程数，而 `dataList` 每一项带多个教学班 —— 拿教学班数去比会永远翻不完。
    #[allow(clippy::too_many_arguments)]
    pub fn catalog_page(
        &self,
        code: &str,
        batch: &str,
        campus: &str,
        keyword: &str,
        class_type: &str,
        query_content: &str,
        page: i64,
    ) -> Result<CatalogPage, SchoolError> {
        let setting = Json::obj(vec![
            (
                "data",
                Json::obj(vec![
                    ("studentCode", Json::str(code)),
                    ("campus", Json::str(campus)),
                    ("electiveBatchCode", Json::str(batch)),
                    ("isMajor", Json::str(self.ep.is_major.clone())),
                    ("teachingClassType", Json::str(class_type.to_string())),
                    ("checkConflict", Json::str("2")),
                    ("checkCapacity", Json::str("2")),
                    (
                        "queryContent",
                        Json::str(query_content.replace("{keyword}", keyword)),
                    ),
                ]),
            ),
            ("pageSize", Json::str(PAGE_SIZE.to_string())),
            ("pageNumber", Json::str(page.to_string())),
            ("order", Json::str("")),
            ("orderBy", Json::str("courseNumber")),
        ]);
        let (payload, _s, _t) = self
            .json_post(
                &self.ep.path("program"),
                &[("querySetting", setting.to_compact())],
                true,
                2,
                None,
            )
            .map_err(|e| SchoolError::Other(e.to_string()))?;

        let total = payload
            .get("totalCount")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        let code = payload.text("code");
        let msg = payload.text("msg");
        let courses = payload.array("dataList").len();
        let mut out = Vec::new();
        for course in payload.array("dataList") {
            // 关键词过滤留在客户端做（服务端那个 queryContent 里还有 MOOC:2 之类的开关，
            // 交给配置去拼；界面要的是"整类课"，自己在结果里筛更可控）
            let name = course.text("courseName");
            if !keyword.is_empty() && !name.contains(keyword) {
                continue;
            }
            for tc in course.array("tcList") {
                out.push(CatalogRow {
                    tc_id: tc.text("teachingClassID"),
                    index: tc.text("courseIndex"),
                    teacher: tc.text("teacherName"),
                    place: tc.text("teachingPlace"),
                    course_number: course.text("courseNumber"),
                    credit: non_empty(course.text("credit"), "0"),
                    is_full: tc.text("isFull"),
                    is_conflict: tc.text("isConflict"),
                    course_name: name.clone(),
                });
            }
        }
        Ok(CatalogPage {
            rows: out,
            courses,
            total,
            code,
            msg,
        })
    }

    /// 写请求的报文体（首发、重试、错开发都共用这一份，字段与前端提交的一致）。
    pub fn submit_body(
        &self,
        tc_id: &str,
        code: &str,
        batch: &str,
        campus: &str,
        tc_type: Option<&str>,
        is_major: Option<&str>,
    ) -> String {
        let add = Json::obj(vec![(
            "data",
            Json::obj(vec![
                ("operationType", Json::str("1")),
                ("studentCode", Json::str(code)),
                ("electiveBatchCode", Json::str(batch)),
                ("teachingClassId", Json::str(tc_id)),
                // 每门课可以各自是方案内/方案外，所以 isMajor 随候选走
                (
                    "isMajor",
                    Json::str(match is_major.filter(|s| !s.is_empty()) {
                        Some(m) => m.to_string(),
                        None => self.ep.is_major.clone(),
                    }),
                ),
                ("campus", Json::str(campus)),
                (
                    "teachingClassType",
                    Json::str(match tc_type.filter(|s| !s.is_empty()) {
                        Some(t) => t.to_string(),
                        None => self.ep.class_type.clone(),
                    }),
                ),
            ]),
        )]);
        httpc::urlencode(&[("addParam", add.to_compact())])
    }

    /// 唯一会改变学校状态的调用（走 keep-alive 连接）。
    ///
    /// 发出去之前先过 WritePacer：超过"每滚动窗口 N 发"的硬上限时在这里等，
    /// 而不是让学校回一句"请求过快"把整个会话打死。
    #[allow(clippy::too_many_arguments)] // 报文要的就是这些字段，包一层结构体反而更难对
    pub fn submit(
        &self,
        code: &str,
        batch: &str,
        tc_id: &str,
        campus: &str,
        tc_type: Option<&str>,
        is_major: Option<&str>,
        timeout: Option<f64>,
    ) -> WriteResult {
        if let Some(p) = self.pacer() {
            p.acquire();
        }
        let body = self.submit_body(tc_id, code, batch, campus, tc_type, is_major);
        let t0 = timeutil::mono_now();
        let outcome = self.json_post_write(&self.ep.path("volunteer"), &body, timeout);
        let ms = (timeutil::mono_now() - t0) * 1000.0;
        let result = match outcome {
            Ok((payload, status, text)) => WriteResult {
                payload,
                status,
                text,
                ms,
            },
            Err(e) => WriteResult {
                ms,
                ..WriteResult::from_error(&e.to_string())
            },
        };
        self.debug_write("写", tc_id, &result);
        result
    }

    /// 一发写请求独占一条**全新短连接**。
    ///
    /// 给"多班错开同时发"用：过载时一条挂住的连接只会拖死它自己，
    /// 不会让同轮其它候选跟着一起等（实测这是放课窗口最大的时间黑洞）。
    /// 连接用完就关 —— 放课窗口里省下的重连时间远不如"不被拖住"值钱。
    #[allow(clippy::too_many_arguments)]
    pub fn submit_fresh(
        &self,
        code: &str,
        batch: &str,
        tc_id: &str,
        campus: &str,
        tc_type: Option<&str>,
        is_major: Option<&str>,
        timeout: Option<f64>,
    ) -> WriteResult {
        if let Some(p) = self.pacer() {
            p.acquire();
        }
        let t = timeout.unwrap_or(self.write_timeout);
        let body = self.submit_body(tc_id, code, batch, campus, tc_type, is_major);
        let headers = self.headers();
        let t0 = timeutil::mono_now();
        let outcome = (|| -> std::io::Result<Response> {
            // 建连超时与读写同一个值：原版这里是
            // http.client.HTTPConnection(HOST, PORT, timeout=timeout or write_timeout)
            let mut stream = httpc::connect(&self.ep.host, self.ep.port, t)?;
            httpc::exchange(
                &mut stream,
                "POST",
                &self.ep.path("volunteer"),
                &headers,
                Some(body.as_bytes()),
                t,
            )
        })();
        let ms = (timeutil::mono_now() - t0) * 1000.0;
        let result = match outcome {
            Ok(resp) => {
                let text = resp.text();
                let payload = json::parse_or_empty(&text);
                WriteResult {
                    payload,
                    status: resp.status,
                    text,
                    ms,
                }
            }
            Err(e) => WriteResult {
                ms,
                ..WriteResult::from_error(&e.to_string())
            },
        };
        self.debug_write("写(短连)", tc_id, &result);
        result
    }

    fn debug_write(&self, tag: &str, tc_id: &str, result: &WriteResult) {
        if !debug_writes() {
            return;
        }
        // 调试输出绝不能影响提交路径本身（原版踩过一次：snapshot 缺失把整轮炸掉，
        // 重试循环把每一发都当成"提交异常"，白扔了 12 发额度）。
        let (verdict, _msg) = classify(&result.payload, result.status, &result.text);
        info(&format!(
            "[dbg] {tag} {} 于 {} → {} {:.0}ms  窗口内 {} 发  {}",
            tail(tc_id, 3),
            timeutil::civil(timeutil::unix_now()).hms_ms(),
            verdict.name(),
            result.ms,
            self.pacer().map(|p| p.snapshot()).unwrap_or(0),
            short(&result.text, 60)
        ));
    }

    /// 把整个 HTTP 请求预序列化成字节，放课瞬间直接 write（对应原版 `build_wire`）。
    pub fn build_wire(
        &self,
        code: &str,
        batch: &str,
        tc_id: &str,
        campus: &str,
        tc_type: Option<&str>,
        is_major: Option<&str>,
    ) -> Vec<u8> {
        let body = self.submit_body(tc_id, code, batch, campus, tc_type, is_major);
        let s = self.session();
        let head = [
            format!("POST {} HTTP/1.1", self.ep.path("volunteer")),
            format!("Host: {}", self.ep.host),
            format!("User-Agent: {}", self.ep.ua),
            "Accept: application/json, text/javascript, */*; q=0.01".to_string(),
            "Accept-Language: zh-CN,zh;q=0.9,en;q=0.8".to_string(),
            "Content-Type: application/x-www-form-urlencoded; charset=UTF-8".to_string(),
            format!("Content-Length: {}", body.len()),
            format!("Origin: {}", self.ep.base_url),
            format!("Referer: {}", s.referer),
            format!("Cookie: {}", s.cookie),
            format!("token: {}", s.token),
            "X-Requested-With: XMLHttpRequest".to_string(),
            "Connection: keep-alive".to_string(),
        ]
        .join("\r\n");
        let mut wire = head.into_bytes();
        wire.extend_from_slice(b"\r\n\r\n");
        wire.extend_from_slice(body.as_bytes());
        wire
    }
}

/// 目录查询 `queryContent` 的模板：**按类别**决定，跟"哪门课"无关。
///
/// 慕课要带 `MOOC:2,` 那个开关才查得到（config.example.json 里也是这么写的），
/// 其余类别就是关键词本身。界面"每一类都拉"时绝不能用某门课的 query_content 去套 ——
/// 那门课要是慕课，别的类别就一个都查不出来了（2026-09-27：用户那门线代的
/// query_content 就是 `MOOC:2,{keyword}`，于是预检查目录一直是 0 条）。
pub fn query_template_for(class_type: &str) -> &'static str {
    if class_type.eq_ignore_ascii_case("MOOC") {
        "MOOC:2,{keyword}"
    } else {
        "{keyword}"
    }
}

/// 目录查询每页取多少条（前端也是 50）。
pub const PAGE_SIZE: i64 = 50;

/// 目录的一页。
pub struct CatalogPage {
    pub rows: Vec<CatalogRow>,
    /// 这一页返回了几门课（`dataList` 的长度）。**不是**教学班数 ——
    /// 判断"还有没有下一页"只能看这个，服务端给的 `totalCount` 也是课程数。
    pub courses: usize,
    /// 这类课一共几门；服务端没给就是 -1
    pub total: i64,
    /// 服务端这次的 code / msg。**空结果必须能分辨**："这门课确实没开"
    /// 和"查询被拒/参数不对"在日志里长得一模一样，光看"目录未返回结果"没法查。
    pub code: String,
    pub msg: String,
}

pub struct CatalogRow {
    pub tc_id: String,
    pub index: String,
    pub teacher: String,
    pub place: String,
    pub course_number: String,
    pub credit: String,
    pub is_full: String,
    pub is_conflict: String,
    /// 课程名（界面上按名字筛/显示要用）
    pub course_name: String,
}

fn non_empty(s: String, fallback: &str) -> String {
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

pub fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn tail(s: &str, n: usize) -> String {
    s.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

// ==========================================================================
// 共享状态
// ==========================================================================

/// 收到中止信号后置位（信号处理函数里只能做这一件事）。
static SIGNAL_STOP: AtomicBool = AtomicBool::new(false);
static SIGNAL_NUMBER: AtomicI32 = AtomicI32::new(0);

pub struct State {
    inner: Mutex<StateInner>,
    local_stop: AtomicBool,
}

struct StateInner {
    done: bool,
    /// 每门课抢到的教学班（下标 = `Target.course_idx`）。全都有值 = 抢齐。
    acquired: Vec<Option<String>>,
    /// 已经中过的冲突组（星期-节次）。同一时段最多中一个，跨课程也一样。
    groups_won: HashSet<String>,
    /// 并行首发同时选上了多个（异常，需人工退课）
    multi: Vec<String>,
    /// 该候选已明确失败，别再打
    blocked: HashSet<String>,
}

impl State {
    /// 单课程（自检与老调用方用这个）。
    pub fn new() -> Arc<State> {
        State::for_courses(1)
    }

    /// 多课程：`acquired` 要为每门课留一格，"抢齐了没有"要拿它比。
    pub fn for_courses(courses: usize) -> Arc<State> {
        Arc::new(State {
            inner: Mutex::new(StateInner {
                done: false,
                acquired: vec![None; courses.max(1)],
                groups_won: HashSet::new(),
                multi: Vec::new(),
                blocked: HashSet::new(),
            }),
            local_stop: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StateInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn block(&self, tc: &str) {
        self.lock().blocked.insert(tc.to_string());
    }

    pub fn blocked_ids(&self) -> HashSet<String> {
        self.lock().blocked.clone()
    }

    pub fn finish(&self, tc: &str) {
        let mut inner = self.lock();
        inner.done = true;
        inner.acquired[0] = Some(tc.to_string());
    }

    /// 确认抢到了 `tc`。返回"所有课程是否都抢齐了"。
    ///
    /// 多课程时这里同时落两条**不变量**（它们是"不双选"的全部依据，轮转调度本身
    /// 不再承担这个职责）：
    /// * 每门课最多中一个 —— 同一门课中两个教学班也是双选；
    /// * 每个冲突组（星期-节次）最多中一个 —— 跨课程的时间冲突就落在同一组里。
    pub fn acquire(&self, course_idx: usize, tc: &str, group: &str) -> bool {
        let mut inner = self.lock();
        if course_idx < inner.acquired.len() {
            inner.acquired[course_idx] = Some(tc.to_string());
        }
        if !group.is_empty() {
            inner.groups_won.insert(group.to_string());
        }
        let all = inner.acquired.iter().all(|a| a.is_some());
        inner.done = all || !inner.multi.is_empty();
        all
    }

    /// 某门课已经抢到了哪个教学班。
    pub fn acquired(&self) -> Vec<Option<String>> {
        self.lock().acquired.clone()
    }

    pub fn course_won(&self, course_idx: usize) -> bool {
        self.lock()
            .acquired
            .get(course_idx)
            .map(|a| a.is_some())
            .unwrap_or(false)
    }

    /// 这个冲突组是不是已经有候选被抢到了 —— 有的话整组都不该再碰。
    pub fn group_won(&self, group: &str) -> bool {
        !group.is_empty() && self.lock().groups_won.contains(group)
    }

    /// 只标记"结束"，不设 confirmed（对应原版 `state.done = True`）。
    pub fn finish_quiet(&self) {
        self.lock().done = true;
    }

    pub fn is_done(&self) -> bool {
        self.lock().done
    }

    pub fn confirmed(&self) -> Option<String> {
        self.lock().acquired.iter().find_map(|a| a.clone())
    }

    pub fn multi(&self) -> Vec<String> {
        self.lock().multi.clone()
    }

    pub fn set_multi(&self, ids: Vec<String>) {
        let mut inner = self.lock();
        inner.done = true;
        inner.multi = ids;
    }

    /// 是否已经要求停止。两个来源：进程收到的中止信号（`install_stop_handler`），
    /// 以及将来可能有的"跑到点了自己收尾"（`local_stop`）。
    pub fn stopped(&self) -> bool {
        self.local_stop.load(Ordering::SeqCst) || SIGNAL_STOP.load(Ordering::SeqCst)
    }
}

/// Ctrl-C / kill 变成"优雅停止"：不发新请求、收尾、打印结果。
///
/// 第一次收到信号只是请求停止（消息由轮询到标志的循环打印出来）；如果卡住了，
/// 再按一次直接 `_exit`。
///
/// 与 Python 的差别：原版在信号处理函数里直接 `print`，Rust 里那样做有风险
/// （处理函数可能打断正在持有 stdout 锁的线程 → 死锁），所以这里只置一个原子标志，
/// 打印交给 `interruptible_sleep` 与各轮循环。
#[cfg(unix)]
pub fn install_stop_handler() {
    extern "C" fn handler(sig: i32) {
        if SIGNAL_STOP.swap(true, Ordering::SeqCst) {
            // 第二次：立刻退出（unlink 与 _exit 都是异步信号安全的）
            crate::lock::remove_lock_in_signal_handler();
            unsafe { libc::_exit(130) };
        }
        SIGNAL_NUMBER.store(sig, Ordering::SeqCst);
    }
    let handler = handler as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

/// Windows 上不装优雅停止的处理函数：CRT 那套 signal 的语义和 unix 不一样
/// （Ctrl-C 走的是控制台事件），而且要处理得干净就得上 SetConsoleCtrlHandler，
/// 那需要另一套 win32 绑定。这里的取舍是：**Windows 上 Ctrl-C 直接结束进程**，
/// 行为与"再按一次 Ctrl-C"一致 —— 不假装优雅，也不引入平台相关的依赖。
#[cfg(not(unix))]
pub fn install_stop_handler() {}

/// 只读阶段（还没进实战）收到 Ctrl-C：打印一句就退出 130。
///
/// 与 Python 一致：那边是 KeyboardInterrupt 冒到 `if __name__` 里，
/// 打印「已手动中止。」然后 `sys.exit(130)`。进入实战之后，
/// `install_stop_handler` 会把处理器换成"优雅停止"那一套。
#[cfg(unix)]
pub fn install_interrupt_handler() {
    extern "C" fn handler(_sig: i32) {
        // 信号处理函数里只能做异步信号安全的事：unlink / write / _exit 都是
        crate::lock::remove_lock_in_signal_handler();
        let msg = "\n已手动中止。\n";
        unsafe {
            libc::write(2, msg.as_ptr().cast(), msg.len());
            libc::_exit(130);
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
    }
}

/// Windows 上没有这套（见 `install_stop_handler` 的说明）。
#[cfg(not(unix))]
pub fn install_interrupt_handler() {}

/// 如果收到过中止信号，打印一次"正在收尾"的提示。
fn note_stop_once(state: &State) {
    static NOTED: AtomicBool = AtomicBool::new(false);
    if state.stopped() && !NOTED.swap(true, Ordering::SeqCst) {
        let sig = SIGNAL_NUMBER.load(Ordering::SeqCst);
        info(&format!(
            "\n收到中止信号（{sig}）：不再发新的写请求，正在收尾并打印结果…（再按一次 Ctrl-C 立即退出）"
        ));
    }
}

/// 可被打断的 sleep：手动停止后最多再等 `step` 秒就返回。
pub fn interruptible_sleep(seconds: f64, state: Option<&State>, step: f64) {
    let end = timeutil::mono_now() + seconds.max(0.0);
    loop {
        if let Some(s) = state {
            if s.stopped() {
                note_stop_once(s);
                return;
            }
        }
        let left = end - timeutil::mono_now();
        if left <= 0.0 {
            return;
        }
        std::thread::sleep(Duration::from_secs_f64(step.min(left).max(0.0)));
    }
}

/// 一个可等待的开关：用来把多发写请求串成"上一发响应回来再发下一发"，
/// 也用来让主线程等待重试循环收尾。
pub struct Gate {
    open_flag: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    pub fn new() -> Arc<Gate> {
        Arc::new(Gate {
            open_flag: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    pub fn open(&self) {
        let mut o = self.open_flag.lock().unwrap_or_else(|e| e.into_inner());
        *o = true;
        self.cv.notify_all();
    }

    pub fn is_open(&self) -> bool {
        *self.open_flag.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait(&self, timeout: f64) {
        let o = self.open_flag.lock().unwrap_or_else(|e| e.into_inner());
        if *o {
            return;
        }
        let _ = self
            .cv
            .wait_timeout_while(o, Duration::from_secs_f64(timeout.max(0.0)), |open| !*open);
    }
}

// ==========================================================================
// 首发：预热 TCP + 自旋到点 + 一次性写出
// ==========================================================================

/// 单发首发的结局。
struct Shot {
    idx: usize,
    tc_id: String,
    status: u16,
    body: Vec<u8>,
    ms: f64,
}

/// 预热 TCP，自旋到点，把整包写出去。
///
/// 建连刻意推迟到 T-3s：空转几十秒的 TCP 可能已被服务端或中间设备回收，
/// 那样首发会打在一条死连接上。3 秒既在 keep-alive 超时之内，又足够完成握手。
///
/// `gate`/`done` 用来把多发串成"上一发响应回来再发下一发"：学校对同一个学生的写请求
/// 有并发互斥，上一发还在处理时打进去的请求只会拿到一个空 msg 的拒绝（实测，
/// 服务端处理 239ms 时固定 100ms 间隔的三发里有两发就是这种空回复）。
/// `gate_timeout` 是等待上限 —— 服务器挂住时，最多多等这么久就照发下一发。
struct SniperJob {
    ep: Arc<Endpoints>,
    wire: Vec<u8>,
    tc_id: String,
    fire_at: f64,
    /// 全局的放课时刻。`fire_at` 是这一发自己的计划时刻（= 放课时刻 + i×stagger），
    /// 日志里两个偏移要分开报 —— 只报相对自己的那个，会把"错开 100ms"读成
    /// "晚了 161ms 才出手"（2026-09-26 复盘时就差点看错）。
    ref_at: f64,
    state: Arc<State>,
    out: mpsc::Sender<Shot>,
    idx: usize,
    write_timeout: f64,
    /// 写请求节拍器：首发也要记账（与重试循环共用同一份额度）
    pacer: Option<Arc<WritePacer>>,
    gate: Arc<Gate>,
    done: Arc<Gate>,
    gate_timeout: f64,
}

fn sniper(job: SniperJob) {
    let SniperJob {
        ep,
        wire,
        tc_id,
        fire_at,
        ref_at,
        state,
        out,
        idx,
        write_timeout,
        pacer,
        gate,
        done,
        gate_timeout,
    } = job;
    let finish = |shot: Shot| {
        let _ = out.send(shot);
        done.open();
    };

    // 等到上一发回来（或超出上限）。等待上限 = 距离自己该出手还剩的时间 + 宽限。
    gate.wait((fire_at - timeutil::unix_now()).max(0.0) + gate_timeout);

    // 先睡到 T-3s
    loop {
        let left = (fire_at - 3.0) - timeutil::unix_now();
        if left <= 0.0 {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(left.min(0.25)));
    }

    // 建连：放课瞬间服务器可能因为过载丢 SYN / backlog 打满，建连失败不能让这一发
    // 直接没了 —— 重试到点为止（每次 1.5s 超时）。这是首发最容易被忽略的失效点。
    let mut sock: TcpStream = loop {
        match httpc::connect(&ep.host, ep.port, 1.5) {
            Ok(s) => break s,
            Err(e) => {
                if timeutil::unix_now() >= fire_at {
                    info(&format!("  首发#{idx} 建连失败，重试循环会兜底: {e}"));
                    finish(Shot {
                        idx,
                        tc_id,
                        status: 0,
                        body: format!(
                            "{{\"error\":{}}}",
                            Json::str(format!("建连失败: {e}")).to_compact()
                        )
                        .into_bytes(),
                        ms: 0.0,
                    });
                    return;
                }
                info(&format!("  首发#{idx} 建连失败（{e}），重试到点…"));
                std::thread::sleep(Duration::from_secs_f64(0.05));
            }
        }
    };
    let t_conn = timeutil::unix_now();

    // 自旋等待，避免 sleep 的调度抖动
    while fire_at - timeutil::unix_now() > 0.0 {
        let left = fire_at - timeutil::unix_now();
        if left > 0.002 {
            std::thread::sleep(Duration::from_secs_f64(left - 0.001));
        }
    }

    // 到点后再取写名额：首发这几发是第一个窗口的额度，正常情况下是零等待。
    // 这一步只是保证"任何路径都不会凑出第 N+1 发"，不会拖慢首发。
    if state.stopped() {
        info(&format!("  首发#{idx} 已取消（手动停止）"));
        finish(Shot {
            idx,
            tc_id,
            status: 0,
            body: format!("{{\"error\":{}}}", Json::str("已手动停止").to_compact()).into_bytes(),
            ms: 0.0,
        });
        return;
    }
    // 到点之后才取写名额：首发这几发是第一个窗口的额度，正常情况下是零等待。
    // 这一步看着多余，其实是**不能省**的 —— 首发与重试循环共用同一份额度，
    // 首发不记账的话，重试循环会以为窗口是空的，于是凑出第 N+1 发被限流。
    if let Some(p) = &pacer {
        p.acquire();
    }
    let t0_mono = timeutil::mono_now();
    let t0_unix = timeutil::unix_now();
    let result = (|| -> std::io::Result<(u16, Vec<u8>)> {
        let _ = sock.set_write_timeout(Some(Duration::from_secs_f64(write_timeout.max(0.05))));
        sock.write_all(&wire)?;
        let resp = httpc::read_response(&mut sock, write_timeout)?;
        Ok((resp.status, resp.body))
    })();
    match result {
        Ok((status, body)) => {
            // 耗时用单调钟；"相对出手时刻"要用墙钟（fire_at 是墙钟）—— 两者不能混算
            let ms = (timeutil::mono_now() - t0_mono) * 1000.0;
            info(&format!(
                "  首发#{idx} 建连于 T{:+.2}s，写出于 T{:+.1}ms（比自己的计划时刻晚 {:.0}ms）",
                t_conn - ref_at,
                (t0_unix - ref_at) * 1000.0,
                (t0_unix - fire_at) * 1000.0
            ));
            finish(Shot {
                idx,
                tc_id,
                status,
                body,
                ms,
            });
        }
        Err(e) => {
            info(&format!("  首发#{idx} 失败（重试循环会兜底）: {e}"));
            finish(Shot {
                idx,
                tc_id,
                status: 0,
                body: format!("{{\"error\":{}}}", Json::str(e.to_string()).to_compact())
                    .into_bytes(),
                ms: 0.0,
            });
        }
    }
}

// ==========================================================================
// 复核与重试
// ==========================================================================

/// 放课窗口内的自动重登录上限：**窗口只按墙钟（`--window`）收口，不看次数。**
///
/// `--relogin-max` 的原意是"凭据错时别把账号刷到被锁"，但真正的凭据错误走的是
/// `BadCredentials` 熔断，不受这里影响；而 2026-09-25 / 09-26 连续两晚的死法都是
/// "试了几次就收工、把整个窗口扔掉"。所以窗口里把它放到实际上不设限。
/// 账户安全由两道更硬的约束兜着：退避封顶 8 秒（不是猛冲），以及窗口本身的时长。
pub const WINDOW_RELOGIN_BUDGET: i64 = 1000;

/// 确认会话是不是真的死了。
///
/// 学校限流会返回「请求过快，请登录后再试」并把会话作废，这种"过期"有时是临时的。
/// 实测教训：19:59:59.22 一发被判过期就终止了整个任务，比放课早 780ms 就自杀了。
/// 所以过期必须用只读接口隔几秒复核，真死了才放弃。
///
/// 这里刻意用 recover=false 关掉自动重登录：本函数只负责回答"现在到底死没死"，
/// 救不救、怎么救由调用方决定（重试循环里那段有完整日志）。
///
/// 复核节奏看有没有自动登录兜底：有的话压到 0.4/0.8 秒 —— 放课窗口只有几十秒，
/// 而限流造成的"假过期"实测是**永久**的（之后每一发都是"身份不一致"），
/// 所以不需要在这里反复等；即使误判"死"，`recover()` 进去还会再复核一次，
/// 真活着就什么都不会发生，代价只是一次只读请求。
fn session_dead(school: &School, code: &str) -> bool {
    let delays: &[f64] = if school.auth.is_some() {
        &[0.4, 0.8]
    } else {
        &[1.0, 2.0, 4.0]
    };
    for delay in delays {
        std::thread::sleep(Duration::from_secs_f64(*delay));
        let Ok(payload) = school.student(code, false) else {
            continue; // 网络问题不代表会话死了
        };
        if payload.text("code") == "1" {
            return false;
        }
    }
    true
}

/// 只读检查某个教学班是否还有非主选空位。
///
/// 返回 `Some(true)`=有空位、`Some(false)`=已满、`None`=查不到（查不到就照打，
/// 不要因为读失败而漏掉机会）。你在这门课属于非主选对象（isMainSelectObject=0），
/// 所以只看 nonMain 那一栏。
///
/// **0/0 一律当成"查不到"。** 2026-09-25 20:00 的教训：放课瞬间系统会进入
/// 「正在初始化」，这段时间容量接口返回 total=0/used=0 —— 那是"没有数据"，
/// 不是"没有空位"。旧版把它算成 0-0>0=False（已满），于是脚本在整个放课窗口里
/// 安静地轮询了 56 秒，一发写请求都没发出去。
fn has_slot(school: &School, tc_id: &str, batch: &str) -> Option<bool> {
    let cap = school.capacity(tc_id, batch).ok()?;
    let total = cap.get("nonMainClassCapacity")?.as_f64()? as i64;
    let used = cap.get("nonMainElectiveNumber")?.as_f64()? as i64;
    if total <= 0 {
        // 0=接口还没数据（初始化中），不是"满"
        return None;
    }
    Some(total - used > 0)
}

/// 唯一真值：已选课程列表里出现完整 teachingClassID。
fn confirm(school: &School, tc_id: &str, attempts: usize, gap: f64) -> Result<bool, SchoolError> {
    for i in 1..=attempts {
        match school.enrolled_ids() {
            Ok(ids) => {
                if ids.contains(tc_id) {
                    return Ok(true);
                }
            }
            Err(e @ SchoolError::Expired(_)) => return Err(e),
            Err(e) => info(&format!("复核第 {i}/{attempts} 次异常: {}", e.message())),
        }
        if i < attempts {
            std::thread::sleep(Duration::from_secs_f64(gap));
        }
    }
    Ok(false)
}

/// 按学校前端逻辑轮询 studentstatus.do：code 1=成功, -1=失败, 其他=处理中。
fn process_ok(school: &School, code: &str, tries: usize) -> String {
    for _ in 0..tries {
        match school.json_post(
            &school.ep.path("status"),
            &[("studentCode", code.to_string())],
            true,
            2,
            None,
        ) {
            Ok((payload, _s, _t)) => {
                let c = payload.text("code");
                if c == "1" {
                    return "ok".to_string();
                }
                if c == "-1" {
                    return format!("fail:{}", payload.text("msg"));
                }
            }
            Err(_) => info("  studentstatus 查询异常，稍后重试"),
        }
        std::thread::sleep(Duration::from_secs_f64(0.4));
    }
    "pending".to_string()
}

/// 学校受理后：异步状态 + 已选课程列表双重复核。
fn settle(school: &Arc<School>, code: &str, t: &Target, state: &Arc<State>) {
    let tc_id = &t.tc;
    info(&format!("  学校已受理 {tc_id}，开始严格复核…"));
    let st = process_ok(school, code, 6);
    info(&format!("  studentstatus: {st}"));
    for attempt in 1..=2 {
        match confirm(school, tc_id, 6, 0.4) {
            Ok(true) => {
                info(&format!("✓✓✓ 已由学校已选课程列表确认选中: {tc_id}"));
                // 两条不变量一起落：这门课不再抢、这个时段（冲突组）整组封住
                let all = state.acquire(t.course_idx, tc_id, &t.group);
                let got = state.acquired();
                let n = got.iter().filter(|a| a.is_some()).count();
                if all {
                    info(&format!("✓ 全部 {n} 门目标课程都抢到了"));
                } else {
                    info(&format!(
                        "  已抢到 {n}/{} 门，继续轮转剩下的候选…",
                        got.len()
                    ));
                }
                return;
            }
            Ok(false) => {
                info(&format!("  ⚠ 已选列表暂未出现 {tc_id}，继续尝试"));
                return;
            }
            Err(_) => {
                if attempt == 1 && school.auth.is_some() && school.recover("复核时会话过期")
                {
                    info("  会话已恢复，重新复核…");
                    continue;
                }
                log::log("✗ 复核时会话过期且无法自动恢复，请重新登录后重跑（或修好凭据文件）");
                state.finish_quiet();
                return;
            }
        }
    }
}

/// 并行首发后的双选检测。
///
/// 并发的两个冲突请求有可能同时通过学校的冲突检查再各自落库（TOCTOU 竞态）。
/// 这里只检测、只报警、只停机 —— 绝不自动退课，退课必须由人来做决定。
fn audit_parallel(
    school: &Arc<School>,
    candidates: &[Target],
    state: &Arc<State>,
    before: &HashSet<String>,
) -> Vec<String> {
    let now_ids = match school.enrolled_ids() {
        Ok(ids) => ids,
        Err(e) => {
            info(&format!(
                "  双选检测跳过（已选列表读取失败）: {}",
                e.message()
            ));
            return Vec::new();
        }
    };
    let label = |tc: &str| -> String {
        candidates
            .iter()
            .find(|t| t.tc == tc)
            .map(|t| t.label.clone())
            .unwrap_or_default()
    };
    let got: Vec<Target> = candidates
        .iter()
        .filter(|t| now_ids.contains(&t.tc) && !before.contains(&t.tc))
        .cloned()
        .collect();
    // 多课程时"中了两门不同的课"是**正常**的，不该报警。要报警的只有那两条不变量
    // 被破：同一门课中了两个教学班，或者同一个时段（冲突组）中了两门 ——
    // 并发的首发请求有可能同时通过学校的冲突检查再各自落库（TOCTOU 竞态）。
    let mut dup: Vec<String> = Vec::new();
    let mut seen_course: HashSet<usize> = HashSet::new();
    let mut seen_group: HashSet<String> = HashSet::new();
    for t in &got {
        let same_course = !seen_course.insert(t.course_idx);
        let same_group = !t.group.is_empty() && !seen_group.insert(t.group.clone());
        if same_course || same_group {
            dup.push(t.tc.clone());
        }
    }
    for t in &got {
        info(&format!(
            "✓✓✓ 已由学校已选课程列表确认选中: {}  {}",
            t.tc,
            label(&t.tc)
        ));
        state.acquire(t.course_idx, &t.tc, &t.group);
    }
    if !dup.is_empty() {
        log::blank();
        log::rule('!');
        log::log("  ⚠ 检测到同一门课 / 同一时段同时选上了多个教学班：");
        for tc in &dup {
            log::log(&format!("      {tc}  {}", label(tc)));
        }
        log::log("  脚本不会替你退课。请自己打开学校页面，退掉你不要的那个。");
        log::log("  注意：退课后再选可能触发学校的「退选再选」限制，请一次想清楚。");
        log::rule('!');
        state.set_multi(dup.clone());
    }
    got.iter().map(|t| t.tc.clone()).collect()
}

// ==========================================================================
// 重试循环
// ==========================================================================

/// 本次要打的一个候选：教学班 + 用哪一类（`teachingClassType`）提交。
///
/// 类型细化到候选这一级，是因为用户可能同时勾了不同类别的课（方案内一门 + 方案外一门），
/// 而**提交报文里的类型必须和那门课的类别一致**，否则学校不认。
/// `tc_type` 为 `None` 时用配置里的 `course.class_type`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub tc: String,
    pub label: String,
    pub tc_type: Option<String>,
    /// 这个候选属于第几门目标课程（0 起）。抢到之后"这门课就不再抢了"靠它分辨。
    pub course_idx: usize,
    /// 冲突组（星期-节次），**规范化之后**的组名 —— 和 `Group.name` 是同一个串。
    ///
    /// 存一份在候选上是为了"每个冲突组最多中一个"这条不变量：确认选中时要能立刻
    /// 知道该把哪个组标记成已满足，而不必反查 groups 表。
    pub group: String,
    /// 提交报文里的 `isMajor`。`None` = 用所属课程的 `is_major`。
    /// 多课程时两门课可能一个方案内一个方案外，所以要能各自带。
    pub is_major: Option<String>,
}

impl Target {
    pub fn new(tc: impl Into<String>, label: impl Into<String>) -> Target {
        Target {
            tc: tc.into(),
            label: label.into(),
            tc_type: None,
            course_idx: 0,
            group: String::new(),
            is_major: None,
        }
    }

    /// 提交这份报文时用的类型；`None` = 交给 `Endpoints.class_type`。
    pub fn wire_type(&self) -> Option<&str> {
        self.tc_type.as_deref().filter(|s| !s.is_empty())
    }

    /// 提交这份报文时用的 `isMajor`；`None` = 交给 `Endpoints.is_major`。
    pub fn wire_is_major(&self) -> Option<&str> {
        self.is_major.as_deref().filter(|s| !s.is_empty())
    }
}

#[derive(Clone)]
pub struct Group {
    pub name: String,
    pub members: Vec<Target>,
}

/// 一轮最多 N 个班：各自独立短连接，前一发**响应回来**（或等满 overlap_wait）再发下一发。
///
/// 一轮最多 N 个班并发发（`pub` 是为了自检能直接量它的时序）。
///
/// 从 Python 的 `_fire_round` 移过来，关键是"**等这一发的响应回来**再发下一发"，
/// 而不是"每发固定睡一会儿"。两条约束同时满足：
///
/// * 不被挂住的请求拖死 —— 每发最多等 `overlap_wait` 秒就先发下一发，
///   不会像单连接顺序发那样，一个超时把整轮拖成 3×timeout（15s 年代是 45 秒）；
/// * 不撞学校的并发互斥 —— 实测同一学生的写请求若上一发还在处理，
///   新的一发会拿到 `code=2` 且 msg 为空的空回复，等于白打（服务端处理 239ms 时，
///   固定 100ms 间隔的三发里有两发就是这样）。等响应回来再发就没有这个问题。
pub fn fire_round(
    school: &Arc<School>,
    code: &str,
    batch: &str,
    campus: &str,
    targets: &[Target],
    timeout: Option<f64>,
    overlap_wait: f64,
) -> Vec<(String, WriteResult)> {
    let mut collected: Vec<(String, WriteResult)> = Vec::new();
    let mut started = 0usize;
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel::<(String, WriteResult)>();
        for t in targets {
            let tx = tx.clone();
            let t = t.clone();
            scope.spawn(move || {
                let res = school.submit_fresh(code, batch, &t.tc, campus, t.wire_type(), t.wire_is_major(), timeout);
                let _ = tx.send((t.tc.clone(), res));
            });
            started += 1;
            // 等这一发的**响应**回来（最多等 overlap_wait 秒）——注意是"等响应"，
            // 不是"固定睡这么久"：学校对同一个学生的写请求有并发互斥，太早打进去
            // 只会拿到空 msg 的拒绝；而固定睡会把整轮拉长（每发都睡 0.35s 的话，
            // 三发要 1.05 秒才轮完，白扔几百毫秒的窗口）。
            if let Ok(pair) = rx.recv_timeout(Duration::from_secs_f64(overlap_wait.max(0.0))) {
                collected.push(pair);
            }
        }
        drop(tx);
        // 剩下的等齐：每发自己的 socket 超时已经兜住了，这里给足预算
        let budget = timeout.unwrap_or(4.0) + 5.0;
        let deadline = timeutil::mono_now() + budget;
        while collected.len() < started {
            let left = deadline - timeutil::mono_now();
            if left <= 0.0 {
                break;
            }
            match rx.recv_timeout(Duration::from_secs_f64(left.min(0.25))) {
                Ok(pair) => collected.push(pair),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    collected
}

/// 算一遍"现在还能打的候选"，按 组 → 组内 的顺序。
///
/// 每一轮轮转开始时现算，于是三类候选会自动消失：刚被判死的、刚抢到的、
/// 以及"这门课已经中了"或"这个时段（冲突组）已经中了"的。
fn build_order(groups: &[Group], state: &State) -> Vec<(usize, usize)> {
    let blocked = state.blocked_ids();
    let mut out = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        if state.group_won(&g.name) {
            continue; // 这个时段已经中了一个，整组都不能再碰
        }
        for (mi, t) in g.members.iter().enumerate() {
            if blocked.contains(&t.tc) || state.course_won(t.course_idx) {
                continue;
            }
            out.push((gi, mi));
        }
    }
    out
}

/// 首发之后的持续尝试 —— **轮转调度器**。
///
/// 每个时间片只读**一个**候选的容量：读到有空位就立刻出手，读到已满就什么都不做，
/// 读不到（初始化中）就照打一发探针写。读取总量因此是常数（`--poll` 个/秒），
/// 不会随课程数变多而爆炸；写请求则保持"低频"，只在真有空位（或读不到）时才发。
///
/// 组间**轮转**，不再是"换过去不回头"：一次走完所有还能打的候选，再从头来。
/// 安全性不靠调度保证，靠 `State` 里那两条不变量 —— 每门课最多中一个、
/// 每个冲突组（星期-节次）最多中一个；确认选中的那一刻就把组标记成已满足。
/// （旧写法是单向往后换组 + `outage_since <= 0` 守着换组时机，2026-09-27 那晚
/// 把第二组永久挡住了，整晚一次没碰 —— 见 README。）
///
/// `--forever` 时窗口是无穷：一直轮转，直到抢齐 / Ctrl-C / 没有能打的候选。
#[allow(clippy::too_many_arguments)]
pub fn retry_loop(
    school: Arc<School>,
    code: String,
    batch: String,
    campus: String,
    groups: Vec<Group>,
    state: Arc<State>,
    args: Arc<Args>,
    fire_at: f64,
    overlap_wait: f64,
    finished: Arc<Gate>,
) {
    // 首发已经用掉几发额度，重试循环要让开一个窗口。
    // 真正的硬保证在 WritePacer 里（首发与这里共用同一份额度）；
    // 这个 start 只是让日志与节奏好看，并且避免第一轮一开始就撞在节拍器上干等。
    // 常驻：**立刻**开始轮转，不等放课时刻（可能提前几小时启动，等下去就是白等）
    let start = if args.forever {
        timeutil::unix_now()
    } else {
        fire_at + args.interval
    };
    while timeutil::unix_now() < start && !state.is_done() && !state.stopped() {
        std::thread::sleep(Duration::from_secs_f64(
            (start - timeutil::unix_now()).clamp(0.0, 0.05),
        ));
    }
    let burst_until = fire_at + args.burst;
    // `--forever`：不设上限，一直轮转下去
    let deadline = if args.forever {
        f64::INFINITY
    } else {
        fire_at + args.window
    };
    // 系统初始化期间谁提交都没用 —— 这段时间只做"探针写"，并记住它有多久
    let mut outage_since = 0.0f64;
    // "候选全满、只在只读轮询"时多久打一行心跳（秒）
    const IDLE_HEARTBEAT: f64 = 30.0;
    let mut last_idle_at = fire_at;
    let mut sent = 0usize;
    // 退避量：0 = 不额外等（正常节奏就是一个时间片 `poll_gap`）。
    // 它是**加在** --poll 之上的，不能拿 --interval 当初值 —— 否则 --poll 再高
    // 也会被 1 秒的地板压住（2026-09-27 写这条常驻测试时当场抓到）。
    let mut pace = 0.0f64;
    // 一个时间片的长度：每秒读 `--poll` 个候选
    let poll_gap = 1.0 / args.poll.max(0.1);
    // 学校连续回"不在选课开放时间范围内"几次就认为**轮次已经关了**。
    // 常驻是好几天的事，轮次一关（比如选课期 2026-10-10 17:30 截止）再挂着就是纯空转。
    // 取 3 次是为了躲开一次性的抖动。
    const CLOSED_STREAK: usize = 3;
    let mut closed_streak = 0usize;

    let mut order: Vec<(usize, usize)> = Vec::new();
    let mut oi = 0usize;
    loop {
        if state.is_done() || state.stopped() || timeutil::unix_now() >= deadline {
            break;
        }
        if order.is_empty() || oi >= order.len() {
            order = build_order(&groups, &state);
            oi = 0;
            if order.is_empty() {
                // 能打的都没了：要么全被判了终态，要么每门课都已经抢到
                info("  没有还能提交的候选了（全被拒或已抢齐），收尾");
                break;
            }
            let names: Vec<&str> = order.iter().map(|(g, _)| groups[*g].name.as_str()).collect();
            info(&format!("  新一轮轮转: {names:?}"));
        }
        let (gi, mi) = order[oi];
        oi += 1;
        let group = &groups[gi];
        let target = group.members[mi].clone();
        // 这一轮排到他了，但**排完之后**可能刚中了一个（同一门课或同一个时段），
        // 也可能刚被判死 —— 所以每一片都要重新确认，不能只信开头算的那份顺序。
        // 漏了这一步就是双选：2026-09-27 加"抢到一门继续抢下一门"时，
        // 正是这条测试当场抓出来的。
        if state.group_won(&target.group)
            || state.course_won(target.course_idx)
            || state.blocked_ids().contains(&target.tc)
        {
            continue;
        }
        {
            // 爆发期只在放课时刻**附近**才算：常驻可能提前几小时启动，
            // 不设下界的话整个下午都在"爆发"（不读容量、每片盲打 3 发）。
            let now = timeutil::unix_now();
            // 常驻**永远不算爆发**：爆发期是"放课瞬间不读容量、每片盲打 3 发"，
            // 跟首发那 3 发是同一件事的另一半。2026-09-27 实测：`--forever` 在过点后
            // 启动时 fire_at = 现在，于是开头 5 秒算爆发 —— 半秒内白扔 3 发写请求
            // （每一发都只会被回「超过课容量」），跟"低频写"是矛盾的。
            let in_burst = !args.forever && now >= fire_at - 0.5 && now < burst_until;
            let base_gap = if in_burst { args.interval } else { poll_gap };
            let mut round_gap = base_gap.max(pace); // 被限流过就按退避后的节奏走
            let t_round = timeutil::unix_now();

            // 这一片要不要出手：
            //   读到有空位 → 立刻打（"高频只读、低频写、读到就快写"）
            //   读到已满   → 什么都不做，下一片看下一个候选
            //   读不到     → 初始化中/容量接口不可信 ⇒ 照打一发探针，不因为读失败漏机会
            let mut picks: Vec<Target> = Vec::new();
            let mut capacity_alive = false;
            if in_burst {
                // 放课瞬间（`--burst` 秒内）保持原样：不读容量，直接用提交当探针 ——
                // 抢的就是那几百毫秒，连扫最多 3 个候选一起打。之后一律回到
                // "一个时间片一个候选"。
                let from = oi.saturating_sub(1);
                let upto = (from + 3).min(order.len());
                for (g2, m2) in &order[from..upto] {
                    picks.push(groups[*g2].members[*m2].clone());
                }
                oi = upto;
            } else {
                match has_slot(&school, &target.tc, &batch) {
                    Some(true) => {
                        capacity_alive = true;
                        picks.push(target.clone());
                    }
                    Some(false) => capacity_alive = true,
                    // 0/0 之类的"读不到"：照打，不因为读失败漏机会
                    None => picks.push(target.clone()),
                }
            }
            if capacity_alive && outage_since > 0.0 {
                // **第二冲突组整晚一发没试**的根因就在这儿（2026-09-27 实战）。
                //
                // 以前"停机结束"只认写请求的判决（Full / 正常回复），而系统恢复之后
                // 候选全是满员 —— 脚本压根不发写请求，于是 outage_since 一直挂着；
                // 那时候换组条件里有一句 `outage_since <= 0`，本身就是为"停机期间
                // 别计时"设的，结果变成**永远不换组**：那晚 306/307/308/309 那一组
                // 轮询了 9 分钟，而 318 组（4 个非主选名额，是唯一名额多一倍的一组）
                // 从头到尾没被碰过，"进入冲突组 星期五-3-5" 打印出来的时刻正好是
                // 窗口截止那一刻。
                //
                // 容量接口能读出真实数字（不管有没有空位），同样说明系统回来了，认它。
                let back = timeutil::unix_now() - outage_since;
                info(&format!("  ✓ 服务恢复（容量接口已能读到真实数据，停机 {back:.0}s）"));
                outage_since = 0.0;
            }
            if outage_since > 0.0 && !picks.is_empty() {
                // 服务端在初始化：多打没意义（每发都会被拒），但必须保持试探而且要快 ——
                // 恢复的那一瞬间才是位子真正可抢的时刻。停机期间改成每 0.6 秒打 1 发：
                // 反应快一倍，写请求反而更少。
                picks.truncate(1);
                round_gap = 0.6;
            }
            if picks.is_empty() {
                // 满员 ⇒ 安静地只读轮询，等谁退课（设计如此：不把账号打成风控）。
                // 但**不能真的安静** —— 2026-09-27 那晚这里静了 9 分钟（20:01:11 →
                // 20:10:00），盯着屏幕只看到一片空白，分不清是"在等"还是"已经死了"。
                let now = timeutil::unix_now();
                if now - last_idle_at >= IDLE_HEARTBEAT {
                    info(&format!(
                        "  候选全满，继续只读轮询等空位… 已等 {:.0}s（{} 个候选在轮转）",
                        now - fire_at,
                        order.len()
                    ));
                    last_idle_at = now;
                }
                let gap = round_gap - (now - t_round);
                interruptible_sleep(gap, Some(&state), 0.2);
                continue;
            }
            // 真要发写了，心跳重新计时（下次安静下来时再打一行）
            last_idle_at = timeutil::unix_now();

            // 爆发期的写超时压到 2 秒：服务器过载时挂住的请求必须尽快放弃，
            // 否则"等超时"本身就吃掉了窗口。正常的写请求 RTT 是 60~200ms，
            // 2 秒足够宽松；万一真被误弃，那一发在服务端仍可能落库，
            // 复核时 courseResult 里会看到，重复提交也只会回"已经选过"。
            let wt = if in_burst {
                args.write_timeout.min(2.0)
            } else {
                args.write_timeout
            };

            // 爆发期并发错开发（谁也拖不死谁）；爆发期之后回归一连接顺序发 ——
            // 那时是"挂着捡漏"，慢一点无所谓，串行还能把双选的可能性再压一档。
            let res: Vec<(String, WriteResult)> = if in_burst && picks.len() > 1 {
                fire_round(
                    &school,
                    &code,
                    &batch,
                    &campus,
                    &picks,
                    Some(wt),
                    overlap_wait,
                )
            } else {
                picks
                    .iter()
                    .map(|t| {
                        (
                            t.tc.clone(),
                            school.submit(&code, &batch, &t.tc, &campus, t.wire_type(), t.wire_is_major(), Some(wt)),
                        )
                    })
                    .collect()
            };

            for t in &picks {
                let (tc, lab) = (&t.tc, &t.label);
                if state.is_done() || state.stopped() {
                    break;
                }
                let got = res
                    .iter()
                    .find(|(t, _)| t == tc)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_else(|| WriteResult::from_error("这一发没有回结果"));
                let (verdict, msg) = classify(&got.payload, got.status, &got.text);
                sent += 1;
                match verdict {
                    Verdict::Busy => {
                        // 学校说"请求过快"就别硬顶，否则会被踢掉会话
                        // 退避上限压到 1.5s：被限流要收敛，但放课后这几秒
                        // 不能一路退到几秒一发，否则等于放弃窗口。
                        pace = (pace.max(poll_gap) * 2.0).min(1.5);
                        if outage_since <= 0.0 {
                            outage_since = timeutil::unix_now();
                            info("  ⚠ 服务端进入初始化/限流状态（这段时间不计入换组倒计时）");
                        }
                        info(&format!(
                            "  被限流/初始化中（{}），退避到 {:.0}ms",
                            short(&msg, 30),
                            pace * 1000.0
                        ));
                    }
                    Verdict::Overload => {
                        // 服务器过载（5xx / 超时 / HTML 错误页）：放课窗口就那么几秒，
                        // 不能像被限流那样一路退避，保持窗口节奏继续打。
                        info(&format!(
                            "  服务器过载（{}），保持节奏继续",
                            short(&msg, 34)
                        ));
                    }
                    Verdict::Full | Verdict::WindowClosed | Verdict::Unknown => {
                        pace *= 0.7;
                        if pace < poll_gap {
                            pace = 0.0; // 退得够小了，回到正常节奏
                        }
                        if outage_since > 0.0 {
                            let back = timeutil::unix_now() - outage_since;
                            info(&format!("  ✓ 服务恢复（写请求已经回正常业务判决，停机 {back:.0}s）"));
                            outage_since = 0.0;
                        }
                        if verdict == Verdict::WindowClosed {
                            closed_streak += 1;
                            if closed_streak >= CLOSED_STREAK && args.forever {
                                log::blank();
                                log::rule('!');
                                log::log(
                                    "  ✗ 学校连续回「不在选课开放时间范围内」—— 这一轮选课看起来已经结束了",
                                );
                                log::log("    收工（常驻不会为一个关掉的轮次空转）。下一个轮次原样重跑即可。");
                                log::rule('!');
                                state.finish_quiet();
                                finished.open();
                                return;
                            }
                        } else {
                            closed_streak = 0;
                        }
                    }
                    Verdict::Submitted | Verdict::Duplicate => {
                        info(&format!(
                            "  {tc} → {} ({:.0}ms) {}",
                            verdict.name(),
                            got.ms,
                            short(&msg, 50)
                        ));
                        settle(&school, &code, t, &state);
                        if state.is_done() {
                            // 与 Python 一致：这里打完计数就收尾返回，不再走下面那段
                            // （否则日志里会出现两遍"本轮共提交 N 次"）
                            info(&format!("  本轮共提交 {sent} 次"));
                            finished.open();
                            return;
                        }
                    }
                    Verdict::Conflict | Verdict::Terminal => {
                        info(&format!(
                            "  {tc} {} → {}: {}  移出候选",
                            short(lab, 20),
                            verdict.name(),
                            short(&msg, 50)
                        ));
                        state.block(tc);
                    }
                    Verdict::Expired => {
                        // 单发判过期不能信：限流伪装的过期会让脚本在放课瞬间自杀。
                        info(&format!(
                            "  疑似过期（{}），用只读接口复核会话…",
                            short(&msg, 30)
                        ));
                        if !session_dead(&school, &code) {
                            info("  会话其实还活着（刚才那发是限流造成的假过期），继续");
                            pace = (pace.max(poll_gap) * 2.0).min(1.5);
                            continue;
                        }
                        // 真死了：这里决定"救不救得回来"，而**绝不能**因此放弃窗口。
                        //
                        // 2026-09-25 和 09-26 两次 20:00 实战都死在这一段：旧版只调用
                        // **一次** recover()，失败就 return —— 整个 90 秒窗口在第 4 秒
                        // 被放弃（09-26 的日志：20:00:04.355 最后一次登录被拒，进程随即退出，
                        // 而学校 20:00:20 左右就能重新登录了）。
                        //
                        // 学校在放课瞬间是把整个选课子系统推倒重排：会话全作废、认证接口
                        // 短暂返回 #E2140600091（这条不消耗重登录配额）。所以现在的做法是
                        // **一直试到窗口结束**：这段时间不写（写出去也必被拒），只做一件事
                        // —— 登回来，然后立刻接着抢。
                        let mut fatal_why = String::new();
                        if school.auth.is_some() {
                            // 窗口里不该因为"试了几次"就停手；墙钟（deadline）才是收口条件。
                            school.relax_relogin_limit(WINDOW_RELOGIN_BUDGET);
                            let mut backoff = 0.5f64;
                            let mut got = false;
                            while timeutil::unix_now() < deadline
                                && !state.stopped()
                                && !state.is_done()
                            {
                                if school.recover("被学校踢掉会话（放课瞬间，会一直试到窗口结束）")
                                {
                                    got = true;
                                    break;
                                }
                                // 只有"凭据本身有问题"才值得停手。**次数上限不算** ——
                                // `--relogin-max` 的原意是凭据错时别把账号刷到被锁，
                                // 而窗口里每一次失败通常只是学校在重排数据或验证码没认出来，
                                // 停手等于把整个窗口扔掉。收口只认墙钟。
                                fatal_why = school.auth_fatal().unwrap_or_default();
                                if !fatal_why.is_empty() {
                                    break;
                                }
                                let left = (deadline - timeutil::unix_now()).max(0.0);
                                info(&format!(
                                    "  还没登回来，{:.1}s 后再试（窗口还剩 {:.0}s）",
                                    backoff.min(left),
                                    left
                                ));
                                interruptible_sleep(backoff, Some(&state), 0.2);
                                // 退避封顶 8 秒：既要快（学校约 20 秒就恢复），
                                // 也不能在 600 秒的窗口里把登录接口打成风控目标 ——
                                // 0.5/1/2/4/8 五次累计 15.5 秒就跨过那次真实停机了。
                                backoff = (backoff * 2.0).min(8.0);
                            }
                            if got {
                                info("  ✓ 会话已登回来，立刻接着抢");
                                pace = 0.0; // 新会话，节奏从头开始
                                continue;
                            }
                        }
                        log::blank();
                        log::rule('!');
                        log::log(
                            "  ✗ 直到窗口结束都没能把会话登回来（多半是被限流踢掉，或学校在放课时重排了数据）",
                        );
                        if school.auth.is_some() {
                            let why = school.auth_why_not();
                            log::log(&format!(
                                "  自动重登录也没能救回来：{}",
                                if !fatal_why.is_empty() {
                                    fatal_why
                                } else if why.is_empty() {
                                    "见上面 [auth] 日志".to_string()
                                } else {
                                    why
                                }
                            ));
                        }
                        log::log("  重新登录选课系统，然后原样重跑本脚本 ——");
                        log::log("  过了放课时刻也会立刻开打，不必等明天。");
                        log::rule('!');
                        state.finish_quiet();
                        // 这条路 Python 直接 return，不打"本轮共提交"那行
                        finished.open();
                        return;
                    }
                }
            }
            if !state.is_done() && !state.stopped() {
                // 按"整轮周期"睡，扣掉本轮已经花掉的时间 —— 目标就是每秒一轮、每轮 3 发。
                let gap = round_gap - (timeutil::unix_now() - t_round);
                interruptible_sleep(gap, Some(&state), 0.2);
            }
        }
    }
    if state.stopped() {
        info(&format!("  已手动停止，本轮共提交 {sent} 次"));
    } else {
        info(&format!("  本轮共提交 {sent} 次"));
    }
    finished.open();
}

// ==========================================================================
// 主流程
// ==========================================================================

fn summarize(
    state: &State,
    candidates: &[Target],
    courses: &[CourseCfg],
    relogins: u32,
    pacer: &WritePacer,
) -> u8 {
    let label = |tc: &str| -> String {
        candidates
            .iter()
            .find(|t| t.tc == tc)
            .map(|t| t.label.clone())
            .unwrap_or_default()
    };
    log::blank();
    log::rule('=');
    if state.stopped() {
        log::log("  （本次是你手动停下的：没抢到不代表窗口结束，随时可以原样重跑）");
    }
    if relogins > 0 {
        log::log(&format!(
            "  本次运行自动重登录 {relogins} 次（被学校限流踢掉后自行恢复）"
        ));
    }
    log::log(&format!(
        "  写请求节拍器：共放行 {} 发，其中 {} 次为守住「每滚动 {:.0}s 最多 {} 发」而等待",
        pacer.total.load(Ordering::Relaxed),
        pacer.waits.load(Ordering::Relaxed),
        pacer.window,
        pacer.per_window
    ));
    let multi = state.multi();
    if !multi.is_empty() {
        log::log("  结果：⚠ 同一门课/同一时段同时选上了多个教学班，需要你手动退掉多余的：");
        for tc in &multi {
            log::log(&format!("      {tc}  {}", label(tc)));
        }
        log::log("  脚本没有、也不会替你退课。请在放课后尽快自行处理。");
        log::rule('=');
        return 4;
    }

    // 逐门课报结果：多课程时"抢到两门中的一门"既不是失败也不是全功。
    let acquired = state.acquired();
    let got: Vec<&String> = acquired.iter().flatten().collect();
    let missing: Vec<String> = courses
        .iter()
        .enumerate()
        .filter(|(i, _)| acquired.get(*i).map(|a| a.is_none()).unwrap_or(true))
        .map(|(_, c)| format!("「{}」", c.display()))
        .collect();
    let total = courses.len().max(1);

    if got.is_empty() {
        log::log("  结果：本次运行未确认抢到任何目标教学班。");
        log::log("  已选课程列表里没有出现目标教学班 —— 可能就是没抢上，不是脚本出错。");
        log::log("  下一次放课可原样重跑；Cookie / 会话都会自动重新获取。");
        log::rule('=');
        return 1;
    }

    log::log(&format!(
        "  结果：抢到 {}/{} 门，共 {} 个教学班",
        total - missing.len(),
        total,
        got.len()
    ));
    for tc in &got {
        log::log(&format!("      {tc}  {}", label(tc)));
    }
    if !missing.is_empty() {
        log::log(&format!(
            "  还没抢到：{}（会话/凭据都不用换，原样重跑就行）",
            missing.join("、")
        ));
    } else {
        log::log("  ✓ 所有目标课程都到手了。");
    }
    log::log("  请立刻去学校页面核对；“已选课程”才是最终依据。");
    log::rule('=');
    if missing.is_empty() {
        0
    } else {
        // 抢到一部分：既不算失败也不算完成，给个独立退出码
        3
    }
}

/// 只读演练：跑到 T-3s 就停，报告计时精度，不提交。
///
/// 这段时间收到 Ctrl-C 会直接退出（见 `install_interrupt_handler`）——
/// 与 Python 里 KeyboardInterrupt 打断 `time.sleep` 的行为一致。
fn rehearsal(fire_at: f64) {
    log::blank();
    info("计时演练：将自旋到 T-3s 并测量调度精度（不提交任何请求）");
    while fire_at - timeutil::unix_now() > 3.0 {
        std::thread::sleep(Duration::from_secs_f64(
            (fire_at - timeutil::unix_now() - 3.0).clamp(0.05, 1.0),
        ));
    }
    let target = timeutil::mono_now() + (fire_at - timeutil::unix_now());
    let mut spins: u64 = 0;
    while timeutil::mono_now() < target {
        spins += 1;
        std::hint::spin_loop();
    }
    let err = (timeutil::mono_now() - target) * 1000.0;
    info(&format!(
        "      自旋 {spins} 圈，到点误差 {err:+.2} ms —— 这就是首发的时间精度"
    ));
}

/// T-30s 重新拿一次会话并复验（会话可能刚好过期）。
///
/// 这一步很关键：等到 19:59:30 才发现会话死了，还有 30 秒可以自动重登录，
/// 而不用等到放课瞬间才发现。
fn refresh_before_fire(school: &Arc<School>, fire_at: f64, state: &Arc<State>, forever: bool) {
    let left = fire_at - timeutil::unix_now();
    // 常驻可能提前几小时启动：绝对不能在这儿睡到 T-30s，那就等于整个下午什么都没干。
    // 会话由轮转循环的只读请求自己续着（读路径会自愈），这里只探一次。
    if left > 35.0 && !forever {
        if school.auth.is_some() {
            info(&format!(
                "等待放课（{:.1} 分钟）… T-30s 会重新取会话并复验",
                left / 60.0
            ));
        } else {
            info(&format!(
                "等待放课（{:.1} 分钟）… T-30s 会复验会话",
                left / 60.0
            ));
        }
        while fire_at - timeutil::unix_now() > 32.0 && !state.stopped() {
            let gap = (fire_at - timeutil::unix_now() - 32.0).min(5.0);
            interruptible_sleep(gap, Some(state), 0.5);
        }
    }
    let (mut ok, payload) = school.probe();
    if !ok && school.auth.is_some() {
        info(&format!(
            "T-30s：会话已失效（{}），立刻自动重登录…",
            payload.text("msg")
        ));
        if school.recover("T-30s 复验失败") {
            ok = school.probe().0;
        }
    }
    let token = school.token();
    if ok {
        info(&format!(
            "T-30s：会话复验通过（token {}…{}），连接就绪",
            short(&token, 8),
            tail(&token, 4)
        ));
    } else {
        info(&format!(
            "⚠ T-30s 会话复验失败: {}{}",
            payload.text("msg"),
            if school.auth.is_some() {
                "（自动登录也没救回来）"
            } else {
                "（请重新登录选课系统）"
            }
        ));
    }
}

/// 从 `--url` 里抠出 token（对应原版的 `[?&]token=([A-Za-z0-9\-]+)`）。
fn parse_token(url: &str) -> Option<String> {
    let bytes = url.as_bytes();
    for i in 0..bytes.len() {
        let at_boundary = i == 0 || bytes[i - 1] == b'?' || bytes[i - 1] == b'&';
        if at_boundary && url[i..].starts_with("token=") {
            let tok: String = url[i + 6..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if !tok.is_empty() {
                return Some(tok);
            }
        }
    }
    None
}

/// 严格区间的时钟对齐采样（对应原版 `server_offset(school, samples=25)`）。
fn server_offset(school: &Arc<School>, samples: usize) -> (f64, f64) {
    let mut got = Vec::new();
    for _ in 0..samples {
        if let Some(s) = school.date_sample() {
            got.push(s);
        }
        std::thread::sleep(Duration::from_secs_f64(0.02));
    }
    clock::server_offset(&got)
}

/// 把 `group_of` 里的某项设成新值（没有就追加）。
fn upsert(pairs: &mut Vec<(String, String)>, key: &str, value: &str) {
    match pairs.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = value.to_string(),
        None => pairs.push((key.to_string(), value.to_string())),
    }
}

pub fn main(mut args: Args) -> Result<u8, String> {
    // 只读阶段的 Ctrl-C：打一句人话然后退出 130（实战阶段会被优雅停止的处理器替换掉）
    install_interrupt_handler();

    // ---- 配置 ----
    let cfg = config::load(None).map_err(|e| e.to_string())?;
    let ep = Arc::new(cfg.endpoints().map_err(|e| e.to_string())?);

    // ---- token（有凭据时可省：登录成功后学校会发新 token）----
    let mut token = String::new();
    let mut referer = String::new();
    if let Some(url) = &args.url {
        match parse_token(url) {
            Some(t) => {
                token = t;
                referer = url.clone();
            }
            None => {
                log::log("✗ 无法从 --url 里解析出 token，请粘贴浏览器地址栏里的完整业务页链接");
                return Ok(2);
            }
        }
    }

    // `--keyword` 在多课程配置里的语义：**只抢名字匹配的课程**（过滤器），
    // 不再像单课程时代那样"覆盖那一门课的关键词"。
    let courses_used: Vec<CourseCfg> = match &args.keyword {
        Some(k) if !k.trim().is_empty() => {
            let hits: Vec<CourseCfg> = ep
                .courses
                .iter()
                .filter(|c| {
                    c.display().contains(k.as_str()) || c.keyword.contains(k.as_str())
                })
                .cloned()
                .collect();
            if hits.is_empty() {
                log::log(&format!("✗ --keyword {k} 没匹配到任何一门目标课程"));
                return Ok(2);
            }
            hits
        }
        _ => ep.courses.clone(),
    };

    log::rule('=');
    log::log("  教务系统抢课 · 放课窗口精准首发");
    log::rule('=');
    log::log(&format!(
        "  会话来源   : {}",
        if token.is_empty() {
            "自动登录（学号+密码）"
        } else {
            "--url 里的 token"
        }
    ));
    if !token.is_empty() {
        log::log(&format!(
            "  token      : {}…{}",
            short(&token, 8),
            tail(&token, 4)
        ));
    }
    log::log(&format!(
        "  目标课程   : {}",
        if courses_used.is_empty() {
            "（未指定，靠 --priority 指定教学班）".to_string()
        } else if courses_used.len() == 1 {
            courses_used[0].display().to_string()
        } else {
            format!(
                "{} 门 —— {}",
                courses_used.len(),
                courses_used
                    .iter()
                    .map(|c| c.display().to_string())
                    .collect::<Vec<_>>()
                    .join("、")
            )
        }
    ));
    log::log(&format!(
        "  首发时刻   : {}",
        if args.now {
            "立即".to_string()
        } else {
            format!("{} (配置时区)", args.at)
        }
    ));
    log::log(&format!(
        "  模式       : {}",
        if args.live {
            "⚠ LIVE 真实提交"
        } else {
            "只读预检（不加 --live 不会提交）"
        }
    ));
    for note in &args.notes {
        log::log(&format!("  ⚠ {note}"));
    }

    // ---- 写接口硬性限速（实测结论，见 README）----
    // 实测：每轮 3 发，间隔 0.8s 安全、0.6s 会被限流。硬下限取 0.8s。
    const MIN_WRITE_GAP: f64 = 0.80;
    let mut interval = args.interval;
    let mut slow = args.slow;
    let mut conns = args.conns;
    if interval < MIN_WRITE_GAP {
        log::log(&format!(
            "⚠ --interval {interval}s 太激进（实测 0.6s 会被限流并作废会话），已强制提升到 {MIN_WRITE_GAP}s"
        ));
        interval = MIN_WRITE_GAP;
    }
    if slow < interval {
        slow = interval;
    }
    // 实测：volunteer.do 的滑动窗口约「每滚动 1 秒最多 3 发」——
    // 连发 3 发没事（间隔 50ms 也行），第 4 发立刻限流并作废会话。
    if conns > 3 {
        log::log(&format!(
            "⚠ --conns {conns} 超过硬上限 3（实测第 4 发连发必被限流并作废会话），已压到 3"
        ));
        conns = 3;
    }
    // 命令行没显式指定时，这几项改用 config.json 里的值（原版 Python 把它们写死了，
    // 配置里那几个键其实是死的 —— 这里让它们活过来，默认值两边一致，所以无感）。
    if args.stagger == crate::cli::DEFAULT_STAGGER {
        args.stagger = ep.pacing.min_gap;
    }
    if args.write_timeout == crate::cli::DEFAULT_WRITE_TIMEOUT {
        args.write_timeout = ep.write_timeout;
    }
    if args.captcha_model.is_none() {
        args.captcha_model = ep.captcha_model_file.clone();
    }

    let args = Arc::new(Args {
        interval,
        slow,
        conns,
        ..args
    });

    if args.offline {
        // 多课程：**每门课各打一发样例**。只打第一门的话，另外两门的报文长什么样
        // 就完全看不到，而 `isMajor` / `teachingClassType` 正是最容易填错、也最难
        // 从报错里看出来的两个字段。
        log::log("\n--offline：不联网。下面是提交时会发送的请求体（每门课各一发样例）：");
        for (ci, c) in courses_used.iter().enumerate() {
            let pick = |suffix: &str| -> bool {
                c.candidates.iter().any(|x| x.id.ends_with(suffix))
            };
            let tc = args
                .priority
                .as_ref()
                .and_then(|p| {
                    p.split(',')
                        .map(|s| s.trim())
                        .find(|s| !s.is_empty() && pick(s))
                        .map(|s| s.to_string())
                })
                .or_else(|| c.candidates.first().map(|x| x.id.clone()))
                .unwrap_or_else(|| "<教学班ID>".to_string());
            let tc_type = c
                .candidates
                .iter()
                .find(|x| x.id == tc)
                .and_then(|x| x.tc_type.clone())
                .unwrap_or_else(|| c.class_type.clone());
            let demo = Json::obj(vec![(
                "data",
                Json::obj(vec![
                    ("operationType", Json::str("1")),
                    (
                        "studentCode",
                        Json::str(args.student.clone().unwrap_or_else(|| "<学号>".to_string())),
                    ),
                    ("electiveBatchCode", Json::str("<批次>")),
                    ("teachingClassId", Json::str(tc)),
                    ("isMajor", Json::str(c.is_major.clone())),
                    ("campus", Json::str("<校区>")),
                    ("teachingClassType", Json::str(tc_type)),
                ]),
            )]);
            if courses_used.len() > 1 {
                log::log(&format!("  ── {}. {} ──", ci + 1, c.display()));
            }
            log::log(&format!("  POST {}", ep.path("volunteer")));
            log::log(&format!("  addParam={}", demo.to_compact()));
        }
        return Ok(0);
    }

    // ---- 出手时刻的格式，联网之前先校验 ----
    // 原版是走到预检一半（对时之前）才因为 `datetime.replace(hour=25…)` 崩掉的，
    // 那时已经发过一串只读请求了。这种纯粹手滑的参数没必要耗网络，也不该甩 traceback。
    if !args.now {
        if let Err(e) = clock::parse_at(&args.at) {
            log::log(&format!("✗ {e}"));
            return Ok(1);
        }
    }

    // ---- 单实例锁（同一账号同时只能有一个有效会话）----
    let lock_path = config::here().join(".course-grabber.lock");
    let _lock = match crate::lock::InstanceLock::acquire(&lock_path, args.force) {
        Some(l) => l,
        None => return Ok(2),
    };

    // ---- 凭据（学号 + 密码，用于自动登录 / 被踢重登录）----
    let credentials_path = args
        .credentials
        .clone()
        .unwrap_or_else(|| ep.credentials_path.clone());
    let mut auth_ctx: Option<(Arc<Credentials>, Arc<dyn Solver>)> = None;
    if args.no_relogin {
        info("--no-relogin：只用 --cookie / --url 给的会话，不启用自动登录");
    } else {
        let creds = match auth::load_credentials(
            args.credentials.as_deref(),
            // 空串按"没给"处理（Python 那边 `args.password` 是 falsy）
            if args.password.as_deref().is_some_and(|p| !p.is_empty()) {
                args.student.as_deref()
            } else {
                None
            },
            args.password.as_deref(),
            &ep.credentials_path,
        ) {
            Ok(c) => c,
            Err(e) => {
                log::log(&format!("✗ 凭据不可用: {}", e.message()));
                return Ok(2);
            }
        };
        match creds {
            None => {
                info(&format!(
                    "没有凭据文件（{credentials_path}），自动登录不可用 —— 需要 --cookie 或 --url"
                ));
            }
            Some(creds) => {
                let min_margin = if args.captcha_min_margin != 0.0 {
                    args.captcha_min_margin
                } else {
                    ep.captcha_min_margin
                };
                let captcha = match Captcha::new(
                    args.captcha_model.as_deref(),
                    min_margin,
                    ep.captcha_width,
                    ep.captcha_height,
                ) {
                    Ok(c) => Arc::new(c),
                    Err(e) => {
                        log::log(&format!("✗ 验证码识别不可用: {e}"));
                        return Ok(2);
                    }
                };
                info(&format!(
                    "自动登录已就绪：学号 {}（凭据来源 {}）+ 识别模型 {}，最多 {} 次重登录",
                    creds.masked(),
                    creds.source,
                    captcha.describe(),
                    args.relogin_max.max(1)
                ));
                auth_ctx = Some((Arc::new(creds), captcha));
            }
        }
    }

    // ---- 学号 ----
    if token.is_empty() && auth_ctx.is_none() {
        log::log("✗ 既没有 --url（token），也没有可用的自动登录凭据 —— 至少要有一样");
        log::log(&format!(
            "  {}",
            auth::no_credentials_note(&credentials_path).replace('\n', "\n  ")
        ));
        return Ok(2);
    }
    let code = args
        .student
        .clone()
        .or_else(|| auth_ctx.as_ref().map(|(c, _)| c.student_id.clone()))
        .unwrap_or_default();
    if code.is_empty() {
        log::log("✗ 无法确定学号，请加 --student 2026xxxxxx 或写凭据文件");
        return Ok(2);
    }
    if let Some((creds, _)) = &auth_ctx {
        if code != creds.student_id {
            log::log(&format!(
                "✗ --student {code} 与凭据文件里的学号 {} 不一致，拒绝启动",
                creds.student_id
            ));
            return Ok(2);
        }
    }

    // ---- 首套会话：优先「凭据文件直接登录」，其次命令行给的 Cookie ----
    let relogin = auth_ctx.as_ref().map(|(creds, captcha)| {
        ReloginManager::new(
            Arc::clone(creds),
            Arc::clone(captcha),
            Arc::clone(&ep),
            args.relogin_max,
            // cooldown 固定 3.0：原版就是这个默认值，而且它不跟着 --relogin-gap 走
            //（两边都会再 min(cooldown, 1.0)，所以实际都是登录成功后歇 1 秒）
            3.0,
            args.captcha_attempts,
            args.relogin_gap,
        )
    });
    let school = School::new(
        Arc::clone(&ep),
        &token,
        "",
        &referer,
        args.write_timeout.max(0.5),
        relogin,
    );
    school.set_code(&code);
    log::log(&format!(
        "[{}] 学号: {}",
        timeutil::ts(),
        auth::mask_id(&code)
    ));

    if auth_ctx.is_some() && args.cookie.is_none() {
        info("自动登录：正在用学号+密码建立学校会话…");
        if !school.recover("启动") {
            if let Some(fatal) = school.auth_fatal() {
                log::log(&format!("✗ {fatal}"));
                log::log("  请核对凭据文件里的学号/密码后重跑。");
                return Ok(3);
            }
            log::log("✗ 自动登录失败（详见上面的 [auth] 日志）");
            log::log(&format!(
                "  {}",
                auth::no_credentials_note(&credentials_path).replace('\n', "\n  ")
            ));
            return Ok(3);
        }
    } else if let Some(cookie) = &args.cookie {
        school.use_cookie(cookie);
        info(&format!(
            "使用命令行提供的 Cookie（{} 字符）",
            cookie.chars().count()
        ));
    } else {
        log::log("✗ 没有可用的会话来源：既没有凭据能自动登录，也没有 --cookie / --url");
        log::log(&format!(
            "  {}",
            auth::no_credentials_note(&credentials_path).replace('\n', "\n  ")
        ));
        return Ok(2);
    }

    // ---- 只读预检 ----
    info(&format!(
        "预检 1/5：校验会话（{}）",
        ep.path_code("student", &code)
    ));
    let (mut ok, mut payload) = school.probe();
    if !ok && school.auth.is_some() {
        info(&format!(
            "      会话无效（{}），尝试自动登录恢复…",
            payload.text("msg")
        ));
        if school.recover("预检发现会话失效") {
            let r = school.probe();
            ok = r.0;
            payload = r.1;
        }
    }
    if !ok {
        log::log(&format!(
            "✗ 会话无效: code={} msg={}",
            payload.text("code"),
            payload.text("msg")
        ));
        if school.auth.is_some() {
            let why = school.auth_why_not();
            log::log(&format!(
                "  → 自动登录也没能救回来: {}",
                if why.is_empty() {
                    "见上面的 [auth] 日志".to_string()
                } else {
                    why
                }
            ));
        } else {
            log::log("  → 请重新登录选课系统，然后重跑本脚本；");
            log::log("    或者写一个凭据文件让脚本自己登录；");
        }
        return Ok(3);
    }
    let token_now = school.token();
    info(&format!(
        "      会话有效（token {}…{}，本次已自动登录 {} 次）",
        short(&token_now, 8),
        tail(&token_now, 4),
        school.relogins()
    ));

    // 目标时刻已经过了：剩下的预检里「对时/目录/容量」都只为「精确对点 + 挑班」服务，
    // 此刻直接开打才值钱 —— 对时一次要 2.5 秒，比整个首发还久。
    let late = clock::target_already_passed(&args.at, args.now, args.tomorrow);
    if late {
        log::log("      ⚡ 目标时刻已过：跳过对时 / 课程目录 / 容量快照，立刻开打");
        log::log("        （这三项只影响「瞄准精度」和「候选排序」，不影响能不能抢到）");
    }
    let data = payload.object("data").cloned().unwrap_or(Json::Null);
    let batch_info = data.object("electiveBatch").cloned().unwrap_or(Json::Null);
    let batch = batch_info.text("code");
    let campus = non_empty(data.text("campus"), "01");
    let limit = data.text("limitElective");
    info(&format!(
        "      批次: {} / {} / {}",
        batch_info.text("name"),
        batch_info.text("typeName"),
        batch_info.text("tacticName")
    ));
    info(&format!(
        "      开放: {} → {}",
        batch_info.text("beginTime"),
        batch_info.text("endTime")
    ));
    info(&format!(
        "      校区: {campus}   学分上下限: {}",
        if limit.is_empty() {
            "未返回"
        } else {
            &limit
        }
    ));
    if !batch_info.text("typeName").is_empty() && batch_info.text("name").contains("预选") {
        log::log("⚠ 当前批次名含“预选”：预选是抽签，抢课无意义。请确认现在是正选/复选/补选。");
    }

    info("预检 2/5：读取已选课程（不可触碰清单）");
    let enrolled: HashSet<String> = match school.enrolled_ids() {
        Ok(ids) => ids,
        Err(e) => {
            if late {
                // 过点了：读不到也照打，最终复核会兜住
                info(&format!("      读取失败（{}），继续开打", e.message()));
                HashSet::new()
            } else {
                log::log(&format!("✗ 读取已选课程失败: {}", e.message()));
                return Ok(2);
            }
        }
    };
    info(&format!(
        "      已选 {} 个教学班{}",
        enrolled.len(),
        if late {
            ""
        } else {
            "，全部只读、绝不改动"
        }
    ));
    if enrolled.is_empty() && !late {
        log::log("      ⚠ 已选课程列表返回 0 条。若你本来有已选课程，说明学校此刻正在重排数据");
        log::log("        （实测 20:00 放课前后会返回「选课系统正在初始化」），该接口暂时不可信：");
        log::log("        「已选中就跳过」的保护和最终复核都可能失效，请以学校页面为准。");
    }

    info("预检 3/5：从课程目录解析候选教学班");
    // 多课程：**每门课各查一次**目录（目录接口一次只认一个关键词 + 一个类别）。
    // 哪门课查到了就记下来 —— 白名单只对"查到过目录"的那几门课生效，
    // 否则某门课目录恰好为空时会把它的候选全删掉。
    let mut catalog: Vec<CatalogRow> = Vec::new();
    let mut catalog_courses: HashSet<usize> = HashSet::new();
    if !late {
        for (ci, c) in courses_used.iter().enumerate() {
            let kw = if c.keyword.is_empty() {
                c.name.clone()
            } else {
                c.keyword.clone()
            };
            // 用**这门课自己**的 query_content（各门课可以不同；慕课那条要带 MOOC:2 开关）
            match school.catalog_page(
                &code,
                &batch,
                &campus,
                &kw,
                &c.class_type,
                &c.query_content,
                0,
            ) {
                Ok(page) if !page.rows.is_empty() => {
                    if courses_used.len() > 1 {
                        log::log(&format!("      「{}」目录 {} 个教学班", c.display(), page.rows.len()));
                    }
                    catalog_courses.insert(ci);
                    catalog.extend(page.rows);
                }
                Ok(page) => log::log(&format!(
                    "      「{}」目录 0 条（服务端 code={} msg={} totalCount={}）—— 见 README「目录查不到」",
                    c.display(),
                    if page.code.is_empty() { "-" } else { &page.code },
                    short(&page.msg, 40),
                    page.total
                )),
                Err(e) => log::log(&format!(
                    "⚠ 「{}」目录解析失败（不影响抢课，白名单将只依赖配置里的候选）: {}",
                    c.display(),
                    e.message()
                )),
            }
        }
    }
    if !catalog.is_empty() {
        // 单课程时列全（与老版一致）；多课程时只报条数，免得刷屏
        if courses_used.len() == 1 {
            for c in &catalog {
                log::log(&format!(
                    "      {} 序{:>2} {:<6} {:<26} 满={} 冲突={}",
                    c.tc_id,
                    c.index,
                    c.teacher,
                    short(&c.place, 26),
                    c.is_full,
                    c.is_conflict
                ));
            }
            log::log(&format!(
                "      课程号 {}  学分 {}（MOOC 学分不占学分上限，非 MOOC 学分才计入 limitElective 上限）",
                catalog[0].course_number, catalog[0].credit
            ));
        }
    } else if late {
        log::log("      （已过点：本次跳过目录解析，冲突组与候选以配置为准）");
    } else {
        log::log("      （目录没给出可用的候选，将只用配置里的候选教学班）");
    }

    // 白名单：优先用 --priority，否则用配置里的候选；目录只用于校验与展示
    // group_of: 教学班 -> 冲突组（星期-节次），用来决定哪些候选可以并发
    //
    // 多课程：候选来自 `courses_used`（= 配置里的 courses，`--keyword` 时是筛过的那几门），
    // 每个候选带着 `course_idx`（属于哪门课）；`is_major` 随所属课程走 ——
    // 两门课可能一个方案内一个方案外，而提交报文里的 isMajor 必须跟着对。
    let builtin_labels: Vec<Target> = courses_used
        .iter()
        .enumerate()
        .flat_map(|(ci, k)| {
            k.candidates.iter().map(move |c: &Candidate| Target {
                tc: c.id.clone(),
                label: c.label.clone(),
                tc_type: c.tc_type.clone(),
                course_idx: ci,
                group: c.group.clone(),
                is_major: Some(k.is_major.clone()),
            })
        })
        .collect();
    let mut group_of: Vec<(String, String)> = builtin_labels
        .iter()
        .map(|t| (t.tc.clone(), t.group.clone()))
        .collect();
    for c in &catalog {
        let g = clock::time_group(&c.place);
        if !g.is_empty() {
            upsert(&mut group_of, &c.tc_id, &g);
        }
    }
    let lookup_label = |tc: &str| -> String {
        builtin_labels
            .iter()
            .find(|t| t.tc == tc)
            .map(|t| t.label.clone())
            .unwrap_or_else(|| tc.to_string())
    };

    let mut candidates: Vec<Target> = if let Some(priority) = &args.priority {
        let mut wanted: Vec<String> = Vec::new();
        for item in priority
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if builtin_labels.iter().any(|t| t.tc == item) {
                wanted.push(item.to_string());
                continue;
            }
            let hit: Vec<String> = builtin_labels
                .iter()
                .filter(|t| t.tc.ends_with(item))
                .map(|t| t.tc.clone())
                .collect();
            match hit.len() {
                0 => wanted.push(item.to_string()), // 表外的完整 ID，原样使用
                1 => wanted.push(hit[0].clone()),
                _ => {
                    log::log(&format!(
                        "✗ --priority 里的 '{item}' 匹配到多个候选，请写完整 ID"
                    ));
                    return Ok(2);
                }
            }
        }
        wanted
            .into_iter()
            .map(|tc| match builtin_labels.iter().find(|t| t.tc == tc) {
                // 配置里有这个候选：课程、类别、冲突组、isMajor 整个继承
                Some(t) => t.clone(),
                // 表外的完整 ID：不知道属于哪门课，按第一门课处理，类别交给配置
                None => Target {
                    label: lookup_label(&tc),
                    tc,
                    tc_type: None,
                    course_idx: 0,
                    group: String::new(),
                    is_major: ep.courses.first().map(|k| k.is_major.clone()),
                },
            })
            .collect()
    } else {
        builtin_labels.clone()
    };

    if !catalog.is_empty() {
        let allowed: HashSet<String> = catalog.iter().map(|c| c.tc_id.clone()).collect();
        // 只校验"目录真的返回过结果"的那几门课 —— 某门课目录恰好为空时，
        // 不能把它的候选当成"不在白名单里"删掉（那等于把整门课删了）
        let in_scope = |t: &Target| catalog_courses.contains(&t.course_idx);
        let outside: Vec<String> = candidates
            .iter()
            .filter(|t| in_scope(t) && !allowed.contains(&t.tc))
            .map(|t| t.tc.clone())
            .collect();
        if !outside.is_empty() {
            log::log(&format!(
                "⚠ 以下教学班不在本次目录白名单里，已剔除: {outside:?}"
            ));
            candidates.retain(|t| !in_scope(t) || allowed.contains(&t.tc));
        }
    }
    if candidates.is_empty() {
        log::log("✗ 没有可用候选教学班，退出");
        return Ok(2);
    }

    // 冲突组规范化：组名必须和 `Group.name` 一致 —— 以目录里的上课地点为准，
    // 目录没有就退回配置里写的，两边都没有就**各自成组**（最保守）。
    // 候选自己存一份组名，是因为"每个冲突组最多中一个"这条不变量要在确认选中的
    // 那一刻就能判定，不该反查 groups 表。
    for t in &mut candidates {
        t.group = group_of
            .iter()
            .find(|(id, _)| id == &t.tc)
            .map(|(_, g)| g.clone())
            .filter(|g| !g.is_empty())
            .unwrap_or_else(|| t.tc.clone());
    }

    // 按冲突组切分，保持优先级顺序。组内互相冲突（最多中一个），组间可以轮转 ——
    // 跨课程的时间冲突天然落在同一个组名（`星期三-3-5`）里，所以两门课同一时段的
    // 候选也会进同一组，不会同时中。
    let mut groups: Vec<Group> = Vec::new();
    for t in &candidates {
        match groups.iter_mut().find(|grp| grp.name == t.group) {
            Some(grp) => grp.members.push(t.clone()),
            None => groups.push(Group {
                name: t.group.clone(),
                members: vec![t.clone()],
            }),
        }
    }
    if groups.len() > 1 {
        for grp in &groups {
            info(&format!(
                "  冲突组 {}: {:?}",
                grp.name,
                grp.members
                    .iter()
                    .map(|t| tail(&t.tc, 3))
                    .collect::<Vec<_>>()
            ));
        }
    }

    // 硬保护 1：已经在已选列表里的候选 -> 那一门课不再抢，绝不重复提交。
    //
    // 多课程时**不能整个退出**：手上已经有线性代数、还要抢大学物理是很正常的用法。
    // （单课程的老行为在这里完全一致：那一门齐了就"没有还能提交的候选"，收工。）
    let already: Vec<Target> = candidates
        .iter()
        .filter(|t| enrolled.contains(&t.tc))
        .cloned()
        .collect();
    if !already.is_empty() {
        candidates.retain(|t| !already.iter().any(|a| a.tc == t.tc));
        let ids: Vec<&str> = already.iter().map(|t| t.tc.as_str()).collect();
        log::log(&format!(
            "\n✓ 这些教学班已经在你的已选课程里，不再抢它们：{ids:?}"
        ));
        if candidates.is_empty() {
            log::log("  目标课程都齐了 —— 脚本不做任何提交。");
            return Ok(0);
        }
    }

    info(&format!(
        "预检 4/5：容量快照{}",
        if late { "（已过点，跳过）" } else { "" }
    ));
    if !late {
        for t in &candidates {
            let (tc, lab) = (&t.tc, &t.label);
            match school.capacity(tc, &batch) {
                Ok(cap) => {
                    let main_txt = format!(
                        "{}/{}",
                        cap.text("mainElectiveNumber"),
                        cap.text("mainClassCapacity")
                    );
                    let non_txt = format!(
                        "{}/{}",
                        cap.text("nonMainElectiveNumber"),
                        cap.text("nonMainClassCapacity")
                    );
                    let total = cap
                        .get("nonMainClassCapacity")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let used = cap
                        .get("nonMainElectiveNumber")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let flag = if total <= 0.0 {
                        "⚠ 无数据（系统初始化中，接口不可信）"
                    } else if total - used > 0.0 {
                        "有空位"
                    } else {
                        "已满"
                    };
                    info(&format!(
                        "      {tc} {:<22} 主选 {:>8}  非主选 {:>6}  → {flag}",
                        short(lab, 22),
                        main_txt,
                        non_txt
                    ));
                }
                Err(e) => info(&format!("      {tc} 容量查询失败: {}", e.message())),
            }
        }
    }

    info("预检 5/5：对齐学校服务器时钟（HTTP Date 头区间估计）");
    let (mut offset, mut half_w) = if late {
        // 我们已经不瞄准未来某个时刻了，对时没有意义；直接按本机时间开打。
        info("      已过点：跳过一次 25 采样的对时（省约 2.5 秒）");
        (0.0, 0.0)
    } else {
        server_offset(&school, 25)
    };
    if half_w == -2.0 {
        info("      区间未收敛（有离群样本）—— 不猜偏移，按 0 处理并给足提前量");
        offset = 0.0;
        half_w = 300.0;
    } else if half_w < 0.0 {
        info("      取不到 Date 头 —— 按 0 偏移处理");
        offset = 0.0;
        half_w = 300.0;
    } else {
        info(&format!(
            "      服务器时钟 {:+.0} ms，估计不确定度 ±{half_w:.0} ms",
            offset * 1000.0
        ));
    }
    // 宁可早一点：未开放只会被驳回，晚了就是真的没抢到。提前量至少覆盖时钟不确定度。
    // 放课是**一个瞬间**（退课位子攒到 20:00 统一放），而写接口只有 ~2 req/s，
    // 所以每一发都很贵：早打必吃「超过课容量」，等于白扔一发。
    // 因此不再强制"提前覆盖时钟不确定度"，默认就瞄准 20:00:00.000 本身。
    let early = args.early_ms / 1000.0;
    let (mut fire_at, when_txt) =
        match clock::resolve_fire_time(&args.at, offset, early, args.now, args.tomorrow) {
            Ok(v) => v,
            Err(e) => {
                log::log(&format!("✗ {e}"));
                return Ok(1);
            }
        };
    if early > 0.0 {
        info(&format!("      提前 {:.0} ms 出手", early * 1000.0));
    } else if early < 0.0 {
        info(&format!(
            "      推后 {:.0} ms 出手（宁晚勿早）",
            -early * 1000.0
        ));
    } else {
        info(&format!(
            "      瞄准服务器 {} 正点出手（时钟不确定度 ±{half_w:.0} ms）",
            args.at
        ));
    }
    info(&format!("      出手时机: {when_txt}"));
    if fire_at - timeutil::unix_now() < 0.0 {
        fire_at = timeutil::unix_now();
    }
    log::blank();
    log::log(&format!(
        "  首发倒计时: {:.1} 秒  (本地 {} 配置时区)",
        fire_at - timeutil::unix_now(),
        timeutil::civil(fire_at).hms_ms()
    ));

    if !args.live {
        log::blank();
        log::rule('=');
        log::log("  预检全部通过。当前是【只读模式】，到点不会提交。");
        log::log("  要真正抢课，请加 --live 重新运行：");
        log::log(&format!(
            "    {} {} --live",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "course-grabber".to_string()),
            if school.auth.is_some() {
                ""
            } else {
                "--url '<你的链接>'"
            }
        ));
        log::rule('=');
        if !args.now {
            rehearsal(fire_at);
        }
        return Ok(0);
    }

    // ---------------- LIVE ----------------
    let state = State::for_courses(courses_used.len());
    // 预检时发现"已经在已选列表里"的课程：先记成已抢到，轮转会自动跳过它们
    for t in &already {
        state.acquire(t.course_idx, &t.tc, &t.group);
    }
    // 写请求节拍器：实测滚动 1 秒最多 3 发，第 4 发会被限流并作废整个会话。
    // 首发的裸 socket 与重试循环共用同一个节拍器，否则两边各打各的就会凑出第 4 发
    // —— 2026-09-24 20:00 和 09-25 复现实验都是这么死的。
    install_stop_handler(); // Ctrl-C / kill = 优雅停止（打印结果后再退出）
    let pacer = Arc::new(WritePacer::new(
        ep.pacing.per_window,
        ep.pacing.window,
        ep.pacing.margin,
        args.stagger,
    ));
    school.set_pacer(Arc::clone(&pacer));
    let end_at = fire_at + args.window;
    // 窗口里不收口在"登了几次"上，只收口在墙钟（`--window`）上。
    //
    // 学校的放课瞬间会**连续**拒绝登录（`#E2140600091 认证失败`，约二十秒），
    // 而那正是最需要登回来的时候。默认的 `--relogin-max 4` 会让脚本正当地
    // "放弃并喊人" —— 但窗口只剩几十秒，喊人等于是放弃。
    // 真·凭据错误走的是另一条路（`BadCredentials` 直接熔断），不受这里影响。
    school.relax_relogin_limit(WINDOW_RELOGIN_BUDGET);
    if args.forever {
        info(&format!(
            "常驻模式：{} 开打，之后一直轮转（抢齐 / Ctrl-C 才停）；每秒最多读 {} 个候选，读到空位才发写请求",
            timeutil::civil(fire_at).hms(),
            args.poll
        ));
    } else {
        info(&format!(
            "本次窗口：{} → {}（--window {:.0}s），之后自动收尾退出；中途 Ctrl-C 可随时手动停",
            timeutil::civil(fire_at).hms(),
            timeutil::civil(end_at).hms(),
            args.window
        ));
    }
    info(&format!(
        "写请求节拍：滚动 {:.0}s 内最多 {} 发，相邻两发至少隔开 {:.0}ms（多等 {:.0}ms 安全边界）—— 首发与重试共用同一份额度",
        pacer.window,
        pacer.per_window,
        pacer.min_gap * 1000.0,
        pacer.margin * 1000.0
    ));
    refresh_before_fire(&school, fire_at, &state, args.forever);

    info("预热 TCP 连接（握手提前完成，放课瞬间不再付 RTT）");
    // 首发只打第一冲突组。默认全部连接都打组内第一优先级；
    // 组内每个候选各占一条连接，到点错开发（组内互相冲突，最多中一个）。
    let top_group = groups[0].members.clone();
    // 首发宽度：**默认就是组内前 3 个班、彼此错开 --stagger（默认 0.10s）**。
    // 放开这一步的依据是 2026-09-25 的实测：错开 0.06s 以上的 3 发写请求全部会被
    // 真正评估，而一秒钟的写额度本来就有 3 发（第 4 发才会被限流并作废会话）。
    // 只想打一个班就用 --single（最保守），--conns N 可以单独指定宽度。
    let mut width = 3usize.min(top_group.len());
    if args.single {
        width = 1;
    }
    if conns > 1 {
        width = width.max((conns as usize).min(3));
    }
    width = width.min(3).min(top_group.len()).max(1);
    //
    // **常驻模式不发这一轮**：轮转循环已经在跑，读到空位就立刻出手（那才是"有空位才写"）；
    // 而首发是"放课瞬间盲打"，实测放课那一瞬间学校正好在重排数据、会话当场作废
    // （09-25/09-26/09-27 三晚都是），这 3 发大概率是废的，还要占写额度。
    // 想保留"正点盲打"就别用 --forever。
    let volley: Vec<Target> = if args.forever {
        Vec::new()
    } else {
        top_group.iter().take(width).cloned().collect()
    };

    // **错开**发，而不是同时发：实测（concurrent_probe，3/3 复现）
    //   3 发同一瞬间出去 → 只有 1 发拿到真正的业务回复，另外 2 发是 code=2 且 msg 为空的
    //                        怪回复 —— 学校对同一学生的并发提交做了互斥，等于白扔 2 发额度；
    //   错开 0.06s 以上   → 3 发全部拿到真正的业务回复（0.03s 时仍会偶发空 msg）。
    // 取 0.10s：3 个班在 0.2 秒内全部被真正评估，仍在一秒钟 3 发的额度之内。
    let plans: Vec<(Vec<u8>, String, f64)> = volley
        .iter()
        .enumerate()
        .map(|(i, t)| {
            (
                school.build_wire(&code, &batch, &t.tc, &campus, t.wire_type(), t.wire_is_major()),
                t.tc.clone(),
                fire_at + i as f64 * args.stagger,
            )
        })
        .collect();
    if plans.is_empty() {
        info("  常驻模式：不发盲打的首发，直接进轮转（读到空位才写）");
    } else {
        info(&format!(
        "  首发 {} 发（硬上限 3）→ {:?}{}",
        plans.len(),
        volley.iter().map(|t| tail(&t.tc, 3)).collect::<Vec<_>>(),
        if plans.len() > 1 {
            format!("，彼此错开 {:.0}ms", args.stagger * 1000.0)
        } else {
            String::new()
        }
    ));
    }

    // 多发首发串成"上一发响应回来再发下一发"（最多多等 overlap_wait）：
    // 既不会因为并发互斥白扔后两发，也不会被一个挂住的请求拖死。
    let gates: Vec<Arc<Gate>> = (0..=plans.len()).map(|_| Gate::new()).collect();
    gates[0].open();
    let (tx, rx) = mpsc::channel::<Shot>();
    let write_timeout = args.write_timeout.min(2.0);
    let gate_timeout = ep.pacing.overlap_wait;
    let handles: Vec<_> = plans
        .iter()
        .enumerate()
        .map(|(i, (wire, tc, at))| {
            let job = SniperJob {
                ep: Arc::clone(&ep),
                wire: wire.clone(),
                tc_id: tc.clone(),
                fire_at: *at,
                ref_at: fire_at,
                state: Arc::clone(&state),
                out: tx.clone(),
                idx: i,
                write_timeout,
                pacer: Some(Arc::clone(&pacer)),
                gate: Arc::clone(&gates[i]),
                done: Arc::clone(&gates[i + 1]),
                gate_timeout,
            };
            std::thread::spawn(move || sniper(job))
        })
        .collect();
    drop(tx);

    // 首发之后的持续尝试（内部会等到 fire_at+interval 才出手）
    let retry_done = Gate::new();
    let worker = {
        let school = Arc::clone(&school);
        let code = code.clone();
        let batch = batch.clone();
        let campus = campus.clone();
        let groups = groups.clone();
        let state = Arc::clone(&state);
        let args = Arc::clone(&args);
        let overlap_wait = ep.pacing.overlap_wait;
        let retry_done = Arc::clone(&retry_done);
        std::thread::spawn(move || {
            retry_loop(
                school,
                code,
                batch,
                campus,
                groups,
                state,
                args,
                fire_at,
                overlap_wait,
                retry_done,
            )
        })
    };

    // 连接可能还要等 fire_at 才写出，join 超时必须覆盖这段等待
    // 分片等待：这样 Ctrl-C 之后能马上收尾，而不是干等满超时
    let last_fire = plans.iter().map(|(_, _, at)| *at).fold(f64::MIN, f64::max);
    let join_deadline = timeutil::mono_now() + (last_fire - timeutil::unix_now()).max(10.0) + 25.0;
    let mut shots: Vec<Shot> = Vec::new();
    while shots.len() < plans.len() && timeutil::mono_now() < join_deadline && !state.stopped() {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(shot) => shots.push(shot),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    for h in handles {
        let _ = h.join();
    }

    // 处理首发结果
    shots.sort_by_key(|s| s.idx);
    for shot in &shots {
        if state.is_done() {
            break;
        }
        let text = String::from_utf8_lossy(&shot.body).into_owned();
        let payload = json::parse_or_empty(&text);
        let (verdict, msg) = classify(&payload, shot.status, &text);
        info(&format!(
            "首发 #{} {} HTTP {} {:.0}ms → {} {}",
            shot.idx,
            shot.tc_id,
            shot.status,
            shot.ms,
            verdict.name(),
            short(&msg, 60)
        ));
        match verdict {
            Verdict::Submitted | Verdict::Duplicate => {
                // 找回首发那一发对应的候选：抢到时要落"哪门课 / 哪个时段"两条不变量
                let t = candidates
                    .iter()
                    .find(|c| c.tc == shot.tc_id)
                    .cloned()
                    .unwrap_or_else(|| Target::new(shot.tc_id.clone(), String::new()));
                settle(&school, &code, &t, &state)
            }
            Verdict::Conflict | Verdict::Terminal => state.block(&shot.tc_id),
            _ => {}
        }
    }

    // 多班首发后做一次双选检测（只报警停机，绝不自动退课）
    if plans.len() > 1 {
        info("多班首发结束，检测是否发生双选…");
        audit_parallel(&school, &candidates, &state, &enrolled);
    }

    if !state.is_done() && !state.stopped() {
        if args.forever {
            info("常驻中，交给轮转循环（抢齐 / Ctrl-C 才停）…");
        } else {
            info("首发未定，交给重试循环…");
        }
        // 常驻时**不能**按 --window 收工 —— 那会在十分钟后把常驻进程杀掉。
        // 收口条件只有：轮转循环自己结束（抢齐 / 轮次关闭 / 没有能打的候选）或 Ctrl-C。
        let stop_by = if args.forever {
            f64::INFINITY
        } else {
            timeutil::unix_now() + args.window + 30.0
        };
        while !retry_done.is_open() && timeutil::unix_now() < stop_by && !state.stopped() {
            retry_done.wait(if args.forever { 1.0 } else { 0.25 });
        }
    }
    drop(worker);

    Ok(summarize(
        &state,
        &candidates,
        &courses_used,
        school.relogins(),
        &pacer,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> Json {
        json::parse(s).unwrap()
    }

    #[test]
    fn classify_order_matters() {
        // 限流优先于"会话过期"：这句里带「登录」字样，若先判过期会直接放弃整个任务
        let (v, _) = classify(
            &Json::Null,
            200,
            "{\"code\":\"302\",\"msg\":\"请求过快，请登录后再试\"}",
        );
        assert_eq!(v, Verdict::Busy);
        // 初始化也是退避重试
        let (v, _) = classify(
            &j("{\"code\":\"0\",\"msg\":\"选课系统正在初始化,请稍候...\"}"),
            200,
            "",
        );
        assert_eq!(v, Verdict::Busy);
    }

    #[test]
    fn classify_expired_and_overload() {
        let (v, _) = classify(&j("{\"code\":\"302\",\"msg\":\"未登录用户\"}"), 200, "");
        assert_eq!(v, Verdict::Expired);
        let (v, _) = classify(&Json::Null, 502, "<html>bad gateway</html>");
        assert_eq!(v, Verdict::Overload);
        let (v, _) = classify(&Json::Null, 0, "");
        assert_eq!(v, Verdict::Overload);
        // 网关 HTML 错误页（状态码却是 200）
        let (v, _) = classify(&Json::Null, 200, "<!DOCTYPE html><html>502</html>");
        assert_eq!(v, Verdict::Overload);
    }

    #[test]
    fn classify_business_outcomes() {
        assert_eq!(
            classify(&j("{\"code\":\"1\"}"), 200, "").0,
            Verdict::Submitted
        );
        assert_eq!(
            classify(&j("{\"code\":\"0\",\"msg\":\"该课程超过课容量\"}"), 200, "").0,
            Verdict::Full
        );
        assert_eq!(
            classify(&j("{\"code\":\"0\",\"msg\":\"已经选过该课程\"}"), 200, "").0,
            Verdict::Duplicate
        );
        assert_eq!(
            classify(
                &j("{\"code\":\"0\",\"msg\":\"与已选课程时间冲突\"}"),
                200,
                ""
            )
            .0,
            Verdict::Conflict
        );
        assert_eq!(
            classify(
                &j("{\"code\":\"0\",\"msg\":\"当前时间不在选课开放时间范围内\"}"),
                200,
                ""
            )
            .0,
            Verdict::WindowClosed
        );
        assert_eq!(
            classify(&j("{\"code\":\"0\",\"msg\":\"超过学分上限\"}"), 200, "").0,
            Verdict::Terminal
        );
        // 限流把会话打死之后，每一发都是这一句
        assert_eq!(
            classify(
                &j("{\"code\":\"0\",\"msg\":\"请求数据与登录者身份不一致\"}"),
                200,
                ""
            )
            .0,
            Verdict::Expired
        );
    }

    #[test]
    fn classify_unknown_keeps_text() {
        let (v, msg) = classify(&Json::Null, 200, "something weird");
        assert_eq!(v, Verdict::Unknown);
        assert_eq!(msg, "something weird");
    }

    #[test]
    fn parse_token_from_url() {
        assert_eq!(
            parse_token("http://x/*default/grablessons.do?token=abc-123&y=1"),
            Some("abc-123".to_string())
        );
        assert_eq!(
            parse_token("http://x/page?foo=1&token=ZZZ"),
            Some("ZZZ".to_string())
        );
        assert_eq!(parse_token("http://x/page"), None);
        // 别把 xtoken 当成 token
        assert_eq!(parse_token("http://x/page?xtoken=abc"), None);
    }

    #[test]
    fn query_template_depends_on_category_not_on_the_course() {
        // 慕课那条查询要带开关才查得到；其余类别**绝不能**带上它 ——
        // 带上了服务端就什么都查不出来（"目录里查不到这门课"）。
        assert_eq!(query_template_for("MOOC"), "MOOC:2,{keyword}");
        assert_eq!(query_template_for("mooc"), "MOOC:2,{keyword}");
        for t in ["FANKC", "FAWKC", "XGXK", "TYKC", ""] {
            assert_eq!(query_template_for(t), "{keyword}", "类别 {t:?}");
        }
    }

    #[test]
    fn submit_body_shape() {
        // 报文体必须与前端提交的字段顺序完全一致
        let ep = sample_endpoints();
        let school = School::new(
            Arc::new(ep),
            "tok",
            "JSESSIONID=x",
            "http://example/grablessons.do?token=tok",
            4.0,
            None,
        );
        let body = school.submit_body("TC1", "2026000000", "B1", "01", None, None);
        assert!(body.starts_with("addParam=%7B%22data%22%3A%7B%22operationType%22%3A%221%22"));
        let decoded = body.replace("addParam=", "");
        // 反向解一下，确认关键字段都在
        for needle in [
            "studentCode",
            "electiveBatchCode",
            "teachingClassId",
            "isMajor",
            "campus",
            "teachingClassType",
        ] {
            assert!(decoded.contains(needle), "{needle} 不在报文体里");
        }
    }

    #[test]
    fn wire_has_expected_request_line() {
        let school = School::new(
            Arc::new(sample_endpoints()),
            "tok-abc",
            "JSESSIONID=X",
            "http://example/x/*default/grablessons.do?token=tok-abc",
            4.0,
            None,
        );
        let wire =
            String::from_utf8_lossy(&school.build_wire("2026000000", "B1", "TC1", "01", None, None))
                .into_owned();
        assert!(wire.starts_with("POST /api/elective/volunteer.do HTTP/1.1\r\n"));
        assert!(wire.contains("token: tok-abc\r\n"));
        assert!(wire.contains("Connection: keep-alive\r\n"));
        assert!(wire.contains("Content-Length: "));
        assert!(wire.contains("\r\n\r\naddParam="));
    }

    /// 一套最小的假端点，够构造 School 用。
    pub(crate) fn sample_endpoints() -> Endpoints {
        config::Config::from_raw(
            json::parse(
                r#"{
                  "school": {"host": "example.edu.cn", "port": 80, "base_path": "/api",
                             "page_path": "/api/*default/index.do"},
                  "paths": {
                    "volunteer": "{base}/elective/volunteer.do",
                    "capacity": "{base}/elective/teachingclass/capacity.do",
                    "result": "{base}/elective/courseResult.do",
                    "status": "{base}/elective/studentstatus.do",
                    "sysparam": "{base}/publicinfo/sysparam.do",
                    "program": "{base}/elective/programCourse.do",
                    "student": "{base}/student/{code}.do",
                    "vcode_token": "{base}/student/4/vcode.do",
                    "vcode_image": "{base}/student/vcode/image.do",
                    "login": "{base}/student/check/login.do"
                  },
                  "cookies": {"session": ["JSESSIONID"], "captcha": ["route", "insert_cookie"]},
                  "password": {"des_keys": ["this", "password", "is"]},
                  "course": {"keyword": "测试课程", "class_type": "TEST", "candidates": []}
                }"#,
            )
            .unwrap(),
            "test",
        )
        .endpoints()
        .unwrap()
    }
}
