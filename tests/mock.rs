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
    /// 这条请求到达的时刻（相对 `Mock::start` 时的 t0）。
    /// 常驻轮询的节奏（`--poll`）与"什么时候才开始问第二个组"都要靠它断言。
    pub at: f64,
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
    /// 学生状态里带选课批次、课程目录里有教学班 —— 给「开荒」用：
    /// 登录 → 读批次 → 查目录 → 挑候选，这条链路要走通
    Catalog,
    /// 课程目录**只**在 `teachingClassType=FANKC`（方案内课程）下有结果
    OnlyInPlan,
    /// 每一类都回一点东西，而且**分两页** —— 验证"每一类都拉、按 totalCount 翻页"
    ByCategory,
    /// 有批次，但目录是空的 —— 不是选课时间时就是这样
    EmptyCatalog,
    /// 放课瞬间学校把整个选课子系统推倒重排：会话作废（读写一律回"身份不一致"），
    /// 认证接口同时返回 `#E2140600091 认证失败`，`AUTH_BACK_AFTER` 秒后才恢复。
    /// 2026-09-25 和 09-26 两次 20:00 实战都是这个状态。
    KickThenAuthBack,
    /// 初始化**结束之后**容量接口开始回真实数字，但一个空位都没有。
    /// 2026-09-27 的实战：脚本于是安静地只读轮询，"停机结束"这个状态再也没被认出来 ——
    /// 见 `grab.rs` 里那段"第二冲突组整晚一发没试"的注释。
    OutageThenFull,
    /// 一直是满的 —— 常驻轮询的常态：只读轮询、一发写请求都不发。
    AlwaysFull,
    /// 两栏名额故意拧着来：`…001` 主选有名额、非主选那一栏不公布（方案内课的典型样子）；
    /// `…002` 主选也有名额，但**非主选满了**（方案外课：能抢的只有非主选那一栏）。
    /// 用来钉住"看哪一栏取决于我们对这门课是什么身份"。
    PoolSplit,
    /// 有些候选的"非主选"那一栏学校**压根不公布**（一直回 0/0），另一些正常回数字。
    /// 用来测"结构性的 0/0 不该拿写请求去试"。
    MissingNonMain,
    /// 提交什么就"选中"什么：`courseResult.do` 会把提交过的教学班报成已选，
    /// 于是 `settle()` 的复核能真的确认选中。给"抢到一门继续抢下一门""不双选"用。
    AlwaysWin,
}

/// `Mode::OutageThenFull` 的停机时长（秒）。
pub const OUTAGE_ENDS: f64 = 2.0;

/// `Mode::KickThenAuthBack` 的停机时长（秒）：这段时间内认证服务不可用。
///
/// 取 4 秒是为了**避开每一步的固定时刻**，让断言不靠运气：重试循环的第一轮在
/// `--interval`（测试里是 1.0s）之后出手，`session_dead` 再花 0.4+0.8 秒复核，
/// 加上 0.5/1.0/2.0 的重登录退避，恢复点落在 4.0s 前后都有近一秒的余量。
pub const AUTH_BACK_AFTER: f64 = 4.0;

/// 一份课程目录响应：一门「测试课程」，三个教学班（其中一个上课地点没写星期节次）。
const CATALOG_JSON: &str = r#"{"code":"1","data":{"campus":"01"},"dataList":[
     {"courseName":"测试课程","courseNumber":"TS100","credit":"3.0","tcList":[
        {"teachingClassID":"000000000000000000000101","courseIndex":"01",
         "teacherName":"张老师","teachingPlace":"星期一第3-5节 一教101",
         "isFull":"0","isConflict":"0"},
        {"teachingClassID":"000000000000000000000102","courseIndex":"02",
         "teacherName":"李老师","teachingPlace":"星期三第3-5节 二教202",
         "isFull":"1","isConflict":"0"},
        {"teachingClassID":"000000000000000000000103","courseIndex":"03",
         "teacherName":"王老师","teachingPlace":"待定",
         "isFull":"0","isConflict":"1"}]},
     {"courseName":"别的课","courseNumber":"XX200","credit":"2.0","tcList":[
        {"teachingClassID":"000000000000000000000999","courseIndex":"01",
         "teacherName":"不该出现","teachingPlace":"星期一第1-2节"}]}
   ]}"#;

pub struct Mock {
    pub port: u16,
    pub calls: Arc<Mutex<Vec<Call>>>,
    /// 写请求（volunteer.do）到达的时刻，相对 `t0`
    pub writes: Arc<Mutex<Vec<f64>>>,
    /// 写请求里提交过的教学班 ID（按提交顺序）
    pub submitted: Arc<Mutex<Vec<String>>>,
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
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));
        let stop = Arc::new(AtomicBool::new(false));
        let t0 = Instant::now();

        let accept = {
            let calls = Arc::clone(&calls);
            let writes = Arc::clone(&writes);
            let submitted = Arc::clone(&submitted);
            let script = Arc::clone(&script);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let calls = Arc::clone(&calls);
                            let writes = Arc::clone(&writes);
                            let submitted = Arc::clone(&submitted);
                            let script = Arc::clone(&script);
                            std::thread::spawn(move || {
                                serve(stream, mode, calls, writes, submitted, script, t0)
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
            submitted,
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
    submitted: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<VecDeque<LoginReply>>>,
    t0: Instant,
) {
    // keep-alive：一个连接上可能连着来好几条请求（客户端就是这么复用的）
    //
    // 必须显式设回阻塞：监听套接字是非阻塞的（accept 循环要轮询 stop 标志），
    // 而 **Windows 上 accept() 出来的套接字会继承监听套接字的非阻塞状态**
    //（POSIX 不会）。继承了的话，serve 线程读完第一条请求就会立刻拿到 WouldBlock,
    // 于是每条连接只服务一条请求 —— 客户端第二次复用连接时就会撞上死连接。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut served = 0usize;
    while let Some(req) = read_request(&mut stream) {
        served += 1;
        let (method, path, form, cookie) = req;
        let path_only = path.split('?').next().unwrap_or(&path).to_string();
        calls.lock().unwrap().push(Call {
            method: method.clone(),
            path: path_only.clone(),
            form: form.clone(),
            cookie: cookie.clone(),
            at: t0.elapsed().as_secs_f64(),
        });

        let mut cookies: Vec<String> = Vec::new();
        let body: String = if method == "POST" && path_only.ends_with("vcode.do") {
            json(r#"{"code":"1","data":{"token":"vt-1"}}"#)
        } else if method == "POST" && path_only.ends_with("login.do") {
            // 重排数据期间认证接口自己也是坏的 —— 这条**不消耗**重登录次数配额
            let step = if mode == Mode::KickThenAuthBack
                && t0.elapsed().as_secs_f64() < AUTH_BACK_AFTER
            {
                LoginReply::Code("#E2140600091", "认证失败")
            } else {
                script.lock().unwrap().pop_front().unwrap_or(LoginReply::Ok)
            };
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
        } else if path_only == "/" {
            // 真实系统就是这么干的：根路径 302 到应用页。配置界面靠这一跳做到"只粘域名"。
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: /api/*default/index.do\r\nContent-Length: 0\r\nDate: {}\r\nConnection: keep-alive\r\n\r\n",
                http_date()
            );
            let _ = stream.write_all(head.as_bytes());
            continue;
        } else if method == "GET" && path_only.ends_with("image.do") {
            cookies.push("route=r1; Path=/".to_string());
            cookies.push("insert_cookie=ic1; Path=/".to_string());
            respond(&mut stream, 200, "image/jpeg", FAKE_JPEG, &cookies);
            continue;
        } else if matches!(
            mode,
            Mode::Catalog | Mode::OnlyInPlan | Mode::ByCategory | Mode::EmptyCatalog
        ) && path_only.contains("/student/")
        {
            // 学生状态里带着"当前选课批次"和"校区" —— 查课程目录要用这两个
            json(
                r#"{"code":"1","data":{"campus":"01",
                     "electiveBatch":{"code":"B1","name":"正选","typeName":"正选","tacticName":"先到先得"}},
                     "dataList":[]}"#,
            )
        } else if mode == Mode::Catalog && path_only.ends_with("programCourse.do") {
            json(CATALOG_JSON)
        } else if mode == Mode::EmptyCatalog && path_only.ends_with("programCourse.do") {
            json(r#"{"code":"1","data":{"campus":"01"},"totalCount":"0","dataList":[]}"#)
        } else if mode == Mode::ByCategory && path_only.ends_with("programCourse.do") {
            // 从 querySetting 里抠出类别与页码：每一类回 2 个教学班、分两页
            let q = form
                .iter()
                .find(|(k, _)| k == "querySetting")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let field = |name: &str| -> String {
                q.split(&format!("\"{name}\":\""))
                    .nth(1)
                    .and_then(|r| r.split('"').next())
                    .unwrap_or("")
                    .to_string()
            };
            let tc_type = field("teachingClassType");
            let page: i64 = field("pageNumber").parse().unwrap_or(0);
            // 55 门课分两页：第一页满 50、第二页 5 —— 真实服务端就是这么发的
            const TOTAL: i64 = 55;
            let start = page * 50;
            if start >= TOTAL {
                json(&format!(
                    r#"{{"code":"1","data":{{"campus":"01"}},"totalCount":"{TOTAL}","dataList":[]}}"#
                ))
            } else {
                let n = (TOTAL - start).min(50);
                let mut items = String::new();
                for i in 0..n {
                    let k = start + i;
                    if i > 0 {
                        items.push(',');
                    }
                    items.push_str(&format!(
                        r#"{{"courseName":"课程{k}","courseNumber":"C{k}","credit":"3.0","tcList":[
                            {{"teachingClassID":"ID-{tc_type}-{k}","courseIndex":"01",
                              "teacherName":"老师{tc_type}","teachingPlace":"星期三第3-5节 一教101",
                              "isFull":"0","isConflict":"0"}}]}}"#
                    ));
                }
                json(&format!(
                    r#"{{"code":"1","data":{{"campus":"01"}},"totalCount":"{TOTAL}","dataList":[{items}]}}"#
                ))
            }
        } else if mode == Mode::GuardSession && !cookie.contains("JSESSIONID=S1") {
            json(r#"{"code":"302","msg":"未登录用户"}"#)
        } else if mode == Mode::KickThenAuthBack && t0.elapsed().as_secs_f64() < AUTH_BACK_AFTER {
            // 会话被作废期间：读写一律这个回复（写请求照旧记账）
            if method == "POST" && path_only.ends_with("volunteer.do") {
                writes.lock().unwrap().push(t0.elapsed().as_secs_f64());
            }
            json(r#"{"code":"0","msg":"请求数据与登录者身份不一致，非法请求。"}"#)
        } else if method == "POST" && path_only.ends_with("volunteer.do") {
            // 写请求在任何模式下都要记账（自检要能断言"发了几发"、发的是哪个班）
            writes.lock().unwrap().push(t0.elapsed().as_secs_f64());
            let add = form
                .iter()
                .find(|(k, _)| k == "addParam")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            if let Some(i) = add.find("teachingClassId") {
                let id: String = add[i + "teachingClassId".len()..]
                    .chars()
                    .skip_while(|c| !c.is_ascii_alphanumeric())
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if !id.is_empty() {
                    submitted.lock().unwrap().push(id);
                }
            }
            let in_outage = mode == Mode::InitOutage
                || (mode == Mode::OutageThenFull && t0.elapsed().as_secs_f64() < OUTAGE_ENDS);
            if mode == Mode::AlwaysFull || (mode == Mode::OutageThenFull && !in_outage) {
                // 满员：写请求会被业务性地拒掉（可重试）。
                // OutageThenFull 停机结束后也是"满员" —— 这个模式模拟的正是
                // "系统回来了，但所有候选都满着"（那才是常态），回成功会让 settle()
                // 复核 2.5 秒，把爆发期的节奏整个拖没。
                json(r#"{"code":"0","msg":"该课程超过课容量"}"#)
            } else if in_outage {
                // 真实措辞：放课瞬间学校就是这个状态，而且**它被算作可重试**
                json(r#"{"code":"0","msg":"选课系统正在初始化,请稍候..."}"#)
            } else {
                json(r#"{"code":"1","msg":"添加选课志愿成功"}"#)
            }
        } else if mode == Mode::InitOutage && path_only.ends_with("capacity.do") {
            // 初始化中的真实行为：没有数据，返回 0/0
            json(r#"{"code":"1","data":{"nonMainClassCapacity":"0","nonMainElectiveNumber":"0"}}"#)
        } else if mode == Mode::PoolSplit && path_only.ends_with("capacity.do") {
            // **两栏都公布、而且故意拧着来**：主选有 10 个空位、非主选满。
            // 这样"看哪一栏"就完全由课程身份决定，两个方向都能钉住：
            //   方案内（FANKC）→ 看主选 → 有空位 → 该打；
            //   方案外（FAWKC）→ 看非主选 → 满 → 不该打（主选那 10 个空位不是我们的）。
            json(
                r#"{"code":"1","data":{"mainClassCapacity":"20","mainElectiveNumber":"10",
                     "nonMainClassCapacity":"2","nonMainElectiveNumber":"2"}}"#,
            )
        } else if mode == Mode::MissingNonMain && path_only.ends_with("capacity.do") {
            let tc = form
                .iter()
                .find(|(k, _)| k == "teachingClassId")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            if tc.ends_with("003") {
                // 这一栏不公布：一直 0/0
                json(r#"{"code":"1","data":{"nonMainClassCapacity":"0","nonMainElectiveNumber":"0"}}"#)
            } else {
                json(r#"{"code":"1","data":{"nonMainClassCapacity":"2","nonMainElectiveNumber":"2"}}"#)
            }
        } else if mode == Mode::AlwaysFull && path_only.ends_with("capacity.do") {
            // 一直是满的（非主选 2/2）—— 常驻轮询的常态
            json(r#"{"code":"1","data":{"nonMainClassCapacity":"2","nonMainElectiveNumber":"2"}}"#)
        } else if mode == Mode::AlwaysWin && path_only.ends_with("capacity.do") {
            // 一直有空位：脚本读到就会出手，于是能验证"抢到之后"的行为
            json(r#"{"code":"1","data":{"nonMainClassCapacity":"2","nonMainElectiveNumber":"0"}}"#)
        } else if mode == Mode::AlwaysWin && path_only.ends_with("courseResult.do") {
            // 提交过的都报成"已选" —— 让 settle() 的复核真的确认选中
            let ids = submitted.lock().unwrap().clone();
            let items: Vec<String> = ids
                .iter()
                .map(|id| format!(r#"{{"teachingClassID":"{id}"}}"#))
                .collect();
            json(&format!(
                r#"{{"code":"1","data":{{"campus":"01"}},"dataList":[{}]}}"#,
                items.join(",")
            ))
        } else if mode == Mode::OutageThenFull && path_only.ends_with("capacity.do") {
            if t0.elapsed().as_secs_f64() < OUTAGE_ENDS {
                // 还在初始化：没有数据
                json(r#"{"code":"1","data":{"nonMainClassCapacity":"0","nonMainElectiveNumber":"0"}}"#)
            } else {
                // 系统回来了，容量接口开始回真实数字 —— 但一个空位也没有。
                // 脚本据此只会安静地只读轮询，写请求一发都不会发。
                json(r#"{"code":"1","data":{"nonMainClassCapacity":"2","nonMainElectiveNumber":"2"}}"#)
            }
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
    // 这条连接结束了：留一句，自检失败时能看出"是服务端先关的"还是客户端断的
    eprintln!("[mock] 连接关闭（这条连接服务了 {served} 条请求）");
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
            Ok(0) => {
                eprintln!("[mock] 对端关闭了连接（EOF）");
                return None;
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if is_would_block(&e) => {
                eprintln!("[mock] 读返回 WouldBlock（套接字是非阻塞的？）");
                return None;
            }
            Err(e) => {
                eprintln!("[mock] 读错误: {e}");
                return None;
            }
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

fn is_would_block(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
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
