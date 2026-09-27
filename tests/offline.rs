//! 离线自检 —— 不联网、不需要密码、不需要真实账号。
//!
//! 对应原版 `tests/test_offline.py`，覆盖的是同一批东西：
//!
//! 1. 密码加密与前端 JS 的历史输出（golden vectors）逐字符一致
//! 2. 验证码识别真的接上了（拿姊妹仓库的合成图跑真模型，对答案）
//! 3. 凭据文件解析 / 权限告警 / 命令行覆盖（在 auth.rs 的单元测试里）
//! 4. 会话字段拼装（token + cookie → Referer / token 头）
//! 5. 用本地假学校服务端跑通：登录协议、重登录策略、只读自愈、写请求绝不重放
//! 6. 回归：学校初始化期间（容量 0/0）必须继续发写请求
//!
//! 用的是 `tests/config.test.json` 这份假配置，所以不需要任何真实的学校信息。

mod mock;

use std::sync::Arc;

use course_grabber::auth::{self, Credentials, ReloginManager};
use course_grabber::captcha::{Captcha, Solver};
use course_grabber::cli;
use course_grabber::config::{Config, Endpoints};
use course_grabber::des;
use course_grabber::grab::{self, Group, School, State, Verdict};
use course_grabber::json;
use course_grabber::pacer::WritePacer;
use course_grabber::timeutil;
use mock::{LoginReply, Mock, Mode, AUTH_BACK_AFTER, OUTAGE_ENDS};

const TEST_CONFIG: &str = include_str!("config.test.json");
const KEYS: [&str; 3] = ["this", "password", "is"];
const PASSWORD: &str = "pwd-123";
const STUDENT: &str = "2026000000";

/// 一套指向假学校服务端的端点配置。
fn endpoints(port: u16) -> Arc<Endpoints> {
    let mut ep = Config::from_raw(json::parse(TEST_CONFIG).unwrap(), "config.test.json")
        .endpoints()
        .expect("测试配置应当能解析");
    ep.host = "127.0.0.1".to_string();
    ep.port = port;
    // base_url 是解析配置时按原端口算好的，这里要跟着改（否则 Referer/Origin 会漏掉端口）
    ep.base_url = format!("http://127.0.0.1:{port}");
    Arc::new(ep)
}

fn creds() -> Arc<Credentials> {
    Arc::new(Credentials {
        student_id: STUDENT.to_string(),
        password: PASSWORD.to_string(),
        source: "test".to_string(),
    })
}

/// 没有真模型时的替身：固定返回 4 个合法坐标，足以跑通协议与策略测试。
/// （假服务端只喂假图，真模型当然会拒绝 —— 原版的 StubSolver 也是这个用途。）
struct StubSolver;

impl Solver for StubSolver {
    fn solve(
        &self,
        _jpeg: &[u8],
    ) -> Result<([[i32; 2]; 4], f32), course_grabber::captcha::Rejected> {
        Ok(([[10, 10], [40, 10], [70, 10], [100, 10]], 0.5))
    }
    fn min_margin(&self) -> f32 {
        0.0
    }
    fn describe(&self) -> String {
        "stub".to_string()
    }
}

fn relogin_manager(ep: &Arc<Endpoints>, max_logins: i64, captcha_attempts: i64) -> ReloginManager {
    ReloginManager::new(
        creds(),
        Arc::new(StubSolver),
        Arc::clone(ep),
        max_logins,
        0.0,
        captcha_attempts,
        0.01,
    )
}

// ==========================================================================
// [1] 密码加密
// ==========================================================================
#[test]
fn password_vectors_match_frontend_js() {
    // 向量由前端 JS 生成（与 desencode.py 顶部那份注释同源）
    let vectors = [
        ("abc", "N0QyMEFBM0M2ODQ0MTdGRg=="),
        ("abcd", "MkVCNURGQUY0NUI4MzdFNA=="),
        (
            "0123456789abcdef",
            "MTlBOUE2OTk5NDI4Mjg1RUQwNzUyMjU4RkFFQjZGRkNCRTk4Qjk4M0Y3QTVDQzMxMTVDQ0IzQTNDNEE0MEI3RQ==",
        ),
        ("短密码", "Qzk5RjAwNzk3QzE4RDUzRg=="),
    ];
    for (plain, want) in vectors {
        assert_eq!(
            des::encrypt_password(
                plain,
                &KEYS.iter().map(|s| s.to_string()).collect::<Vec<_>>()
            ),
            want,
            "加密结果与前端向量不一致: {plain:?}"
        );
    }
    // 空/异常输入不能 panic
    assert_eq!(des::str_enc("abc", &[]), "");
    assert_eq!(
        des::encrypt_password("", &KEYS.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        ""
    );
}

// ==========================================================================
// [2] 验证码识别：真的接上了吗
// ==========================================================================
#[test]
fn captcha_model_solves_a_real_synthetic_image() {
    // 姊妹仓库的合成图 + 它自己的答案（不是我们算出来的），所以这是真正的端到端校验：
    // JPEG 解码 → 切字 → 模型 → 360 种分配，一路都要对。
    let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../click-captcha-matcher-rs/tests/fixtures");
    let expected = match std::fs::read_to_string(fixture_dir.join("expected.txt")) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("跳过：读不到姊妹仓库的测试图（{e}）");
            return;
        }
    };
    let line = expected
        .lines()
        .find(|l| l.starts_with("synth0.jpg"))
        .expect("expected.txt 里应当有 synth0.jpg");
    let want = line
        .split_whitespace()
        .nth(3)
        .expect("答案格式：名字 状态 哈希 坐标")
        .to_string();
    let jpeg = std::fs::read(fixture_dir.join("synth0.jpg")).expect("读 synth0.jpg");

    let solver = Captcha::new(None, 0.0, 250, 80).expect("内编模型应当能加载");
    let (points, margin) = solver.solve(&jpeg).expect("真模型应当能解出坐标");
    assert_eq!(
        course_grabber::captcha::verify_code(&points),
        want,
        "识别结果与姊妹仓库记录的答案不一致"
    );
    assert!(margin > 0.05, "margin 太小: {margin}");
}

#[test]
fn captcha_rejects_garbage() {
    let solver = Captcha::new(None, 0.0, 250, 80).unwrap();
    assert!(solver.solve(b"not a jpeg").is_err());
    assert!(
        solver.solve(mock::FAKE_JPEG).is_err(),
        "假图不该被解出合法坐标"
    );
}

// ==========================================================================
// [3] 会话字段拼装
// ==========================================================================
#[test]
fn session_shape() {
    let ep = endpoints(1);
    let referer = auth::LoginSession::build_referer(&ep, "abc-123");
    assert!(referer.ends_with("token=abc-123"), "{referer}");
    assert!(referer.starts_with("http://127.0.0.1"), "{referer}");
    assert!(
        referer.contains("/course-system/api/*default/grablessons.do"),
        "Referer 默认取 base_path + 前端那个页面: {referer}"
    );
    assert_eq!(
        course_grabber::captcha::verify_code(&[[1, 2], [3, 4], [5, 6], [7, 8]]),
        "1-2,3-4,5-6,7-8"
    );
    // Set-Cookie 解析不被 Expires 里的逗号带偏
    assert_eq!(
        auth::set_cookie_values(
            &["route=a; Path=/, extra=b; Expires=Wed, 21 Oct 2026 07:28:00 GMT".to_string()],
            &["extra".to_string(), "route".to_string()]
        ),
        "extra=b; route=a"
    );
}

// ==========================================================================
// [3.5] 对时：采样中点必须是墙钟、区间必须能收敛
// ==========================================================================
#[test]
fn clock_alignment_converges() {
    // 这条测试盯的是一个很容易犯、而且**完全看不出来**的错：采样的中点如果用了
    // 单调钟（进程启动起算的小数），拿它去减 Date 头里的 epoch 就量纲对不上，
    // 网格投票一个格都投不到 → 永远返回"区间未收敛" → 对时形同虚设。
    let mock = Mock::start(Mode::Normal, vec![]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "t",
        "JSESSIONID=x",
        "http://x/y.do?token=t",
        4.0,
        None,
    );
    school.set_code(STUDENT);

    let mut samples = Vec::new();
    for _ in 0..25 {
        if let Some(s) = school.date_sample() {
            samples.push(s);
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(0.02));
    }
    assert_eq!(samples.len(), 25, "每次采样都该拿到 Date 头");
    assert!(
        samples[0].mid > 1_600_000_000.0,
        "采样中点必须是墙钟 epoch（拿到 {}，说明用了单调钟）",
        samples[0].mid
    );
    assert!(samples[0].sec > 1_600_000_000, "Date 头要能解析成 epoch");

    let (offset, half) = course_grabber::clock::server_offset(&samples);
    assert!(
        half > 0.0,
        "对时应当收敛（half={half}，-2 表示样本之间对不上）"
    );
    assert!(
        offset.abs() < 1.0,
        "假学校和本机同钟，偏移应当接近 0，实际 {offset}"
    );
    mock.close();
}

// ==========================================================================
// [4] 假学校服务端：登录协议
// ==========================================================================
#[test]
fn login_happy_path() {
    let mock = Mock::start(Mode::Normal, vec![]);
    let ep = endpoints(mock.port);
    let solver = StubSolver;
    let session = auth::Login::new(&creds(), &solver, &ep, 6, 0.01, 15.0)
        .login()
        .expect("登录应当成功");

    assert_eq!(session.token, "tok-new");
    assert!(
        session.cookie.contains("JSESSIONID=S1"),
        "{}",
        session.cookie
    );
    assert!(session.cookie.contains("route=r1"), "{}", session.cookie);
    assert!(
        session.cookie.contains("insert_cookie=ic1"),
        "{}",
        session.cookie
    );
    assert_eq!(session.name, "测试同学");

    let login_calls = mock.calls_to("login.do");
    assert_eq!(login_calls.len(), 1);
    let call = &login_calls[0];
    // loginPwd 必须是学校协议的密文（与前端 JS 的输出逐字符一致）
    assert_eq!(
        call.field("loginPwd"),
        des::encrypt_password(
            PASSWORD,
            &KEYS.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        )
    );
    assert_eq!(call.field("loginName"), STUDENT);
    assert_eq!(call.field("vtoken"), "vt-1");
    let verify = call.field("verifyCode");
    assert_eq!(verify.split(',').count(), 4, "{verify}");
    assert!(verify.split(',').all(|p| p.contains('-')), "{verify}");
    // 登录请求只带本轮验证码 Cookie，绝不混入旧会话的登录 Cookie
    assert!(!call.cookie.contains("JSESSIONID"), "{}", call.cookie);
    assert!(call.cookie.contains("route=r1"), "{}", call.cookie);
    mock.close();
}

/// 分块编码（Transfer-Encoding: chunked）：Java 后端很可能这么回，
/// 而我们这套 HTTP 读取器是自己写的，必须验证分块能正确读完。
#[test]
fn login_works_with_chunked_responses() {
    let mock = Mock::start(Mode::Chunked, vec![]);
    let ep = endpoints(mock.port);
    let session = auth::Login::new(&creds(), &StubSolver, &ep, 6, 0.01, 15.0)
        .login()
        .expect("分块响应也要能正确解析");
    assert_eq!(session.token, "tok-new");
    assert_eq!(session.name, "测试同学");
    mock.close();
}

#[test]
fn login_retries_on_rejected_captcha() {
    let mock = Mock::start(
        Mode::Normal,
        vec![LoginReply::Code("3", "验证码不正确"), LoginReply::Ok],
    );
    let ep = endpoints(mock.port);
    let session = auth::Login::new(&creds(), &StubSolver, &ep, 6, 0.01, 15.0)
        .login()
        .expect("换一张图之后应当成功");
    assert_eq!(session.token, "tok-new");
    assert_eq!(
        mock.calls_to("image.do").len(),
        2,
        "验证码被拒后应当自动换图"
    );
    mock.close();
}

// ==========================================================================
// [5] 假学校服务端：重登录策略
// ==========================================================================
#[test]
fn wrong_password_fuses_autologin() {
    let mock = Mock::start(
        Mode::Normal,
        vec![LoginReply::Code("2", "登录名或密码不正确")],
    );
    let ep = endpoints(mock.port);
    let mut mgr = relogin_manager(&ep, 4, 3);

    assert!(mgr.relogin("t").is_none(), "密码错必须返回 None");
    assert!(mgr.fatal.is_some() && !mgr.available(), "密码错必须熔断");
    assert!(mgr.fatal.as_deref().unwrap_or("").contains("密码"));

    let before = mock.calls.lock().unwrap().len();
    assert!(mgr.relogin("t").is_none());
    assert_eq!(
        mock.calls.lock().unwrap().len(),
        before,
        "熔断后不该再发任何请求"
    );
    mock.close();
}

#[test]
fn online_limit_respects_max_relogins() {
    let mock = Mock::start(
        Mode::Normal,
        vec![
            LoginReply::Code("4", "在线人数超过上限"),
            LoginReply::Code("4", "在线人数超过上限"),
        ],
    );
    let ep = endpoints(mock.port);
    let mut mgr = relogin_manager(&ep, 2, 1);

    assert!(mgr.relogin("t").is_none());
    assert!(mgr.available(), "第 1 次超限之后还有配额");
    assert!(mgr.relogin("t").is_none());
    assert!(!mgr.available(), "第 2 次之后到达上限");

    let before = mock.calls.lock().unwrap().len();
    assert!(mgr.relogin("t").is_none());
    assert_eq!(
        mock.calls.lock().unwrap().len(),
        before,
        "到上限后不该再发请求"
    );
    mock.close();
}

// ==========================================================================
// [6] 假学校服务端：只读自愈 / 写请求绝不重放
// ==========================================================================
#[test]
fn read_path_recovers_but_write_never_replays() {
    let mock = Mock::start(Mode::GuardSession, vec![LoginReply::Ok]);
    let ep = endpoints(mock.port);

    // a) 只读接口 → 会话过期 → 自动重登录 → 重放成功
    let school = School::new(
        Arc::clone(&ep),
        "stale-token",
        "JSESSIONID=OLD",
        "http://x/grablessons.do?token=stale-token",
        4.0,
        Some(relogin_manager(&ep, 4, 2)),
    );
    school.set_code(STUDENT);
    let payload = school.student(STUDENT, true).expect("只读请求应当自愈");
    assert_eq!(payload.text("code"), "1", "{payload:?}");
    assert_eq!(school.token(), "tok-new", "token 应当换成新会话的");
    assert_eq!(school.relogins(), 1);
    // 新会话的 Referer 也要跟着换（原版踩过坑：Referer 不带新 token 会被判跨域）
    assert_eq!(
        auth::LoginSession::build_referer(&ep, &school.token()),
        format!(
            "http://127.0.0.1:{}/course-system/api/*default/grablessons.do?token=tok-new",
            mock.port
        )
    );
    mock.close();

    // b) 写接口（recover=false）→ 绝不自动重放，也不偷偷登录
    let mock = Mock::start(Mode::GuardSession, vec![LoginReply::Ok]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "stale-token",
        "JSESSIONID=OLD",
        "http://x/grablessons.do?token=stale",
        4.0,
        Some(relogin_manager(&ep, 4, 2)),
    );
    school.set_code(STUDENT);
    let res = school.submit(STUDENT, "B1", "TC1", "01", None, None, Some(2.0));
    let (verdict, _msg) = grab::classify(&res.payload, res.status, &res.text);
    assert_eq!(verdict, Verdict::Expired, "写请求被判过期");
    assert_eq!(
        mock.calls_to("login.do").len(),
        0,
        "写请求绝不能触发自动登录"
    );
    assert_eq!(
        mock.calls_to("volunteer.do").len(),
        1,
        "写请求只能发一次，不许重放"
    );
    assert_eq!(school.token(), "stale-token", "会话不该被偷偷换掉");
    mock.close();
}

/// 爆发期一轮多发的时序：**等上一发的响应回来**再发下一发，而不是每发固定睡
/// overlap_wait。固定睡会把整轮拉长（三发 1.05 秒），放课瞬间那几百毫秒是白扔的。
#[test]
fn fire_round_waits_for_response_not_a_fixed_sleep() {
    let mock = Mock::start(Mode::Normal, vec![]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "t",
        "JSESSIONID=x",
        "http://x/y.do?token=t",
        4.0,
        None,
    );
    school.set_code(STUDENT);
    school.set_pacer(Arc::new(WritePacer::new(3, 1.0, 0.15, 0.0)));
    let targets: Vec<grab::Target> = vec![
        grab::Target::new("TC1", "A班"),
        grab::Target::new("TC2", "B班"),
        grab::Target::new("TC3", "C班"),
    ];

    let t0 = std::time::Instant::now();
    let res = grab::fire_round(&school, STUDENT, "B1", "01", &targets, Some(2.0), 0.35);
    let dt = t0.elapsed();

    assert_eq!(res.len(), 3, "三发都要有结果");
    assert!(
        dt < std::time::Duration::from_millis(500),
        "三发用了 {dt:?} —— 假学校毫秒级就回了，说明每发都在固定睡 0.35s"
    );
    assert_eq!(mock.write_count(), 3);
    mock.close();
}

// ==========================================================================
// [7] 回归：学校初始化期间必须继续发写请求
// ==========================================================================
#[test]
fn init_outage_keeps_firing() {
    // 2026-09-25 20:00 的真实事故：初始化期间容量接口返回 total=0/used=0，
    // 旧版把它算成 0-0>0=False（"已满"），于是脚本在整个放课窗口里安静地轮询了
    // 56 秒，一发写请求都没发出去。这个测试把那个场景固定下来。
    let mock = Mock::start(Mode::InitOutage, vec![]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "tok",
        "JSESSIONID=x",
        "http://x/y.do?token=tok",
        4.0,
        None,
    );
    school.set_code(STUDENT);
    school.set_pacer(Arc::new(WritePacer::new(3, 1.0, 0.15, 0.10)));

    let args = match cli::parse(
        [
            "--url",
            "http://x/y.do?token=t",
            "--live",
            "--window",
            "2.5",
            "--burst",
            "0.3",
            "--interval",
            "1.0",
            "--slow",
            "1.0",
        ]
        .iter()
        .map(|s| s.to_string()),
    )
    .unwrap()
    {
        cli::Parsed::Run(a) => *a,
        _ => unreachable!(),
    };

    let groups = vec![Group {
        name: "星期一-3-5".to_string(),
        members: vec![
            grab::Target::new("000000000000000000000001", "A班"),
            grab::Target::new("000000000000000000000002", "B班"),
        ],
    }];
    let fire_at = timeutil::unix_now();
    grab::retry_loop(
        Arc::clone(&school),
        STUDENT.to_string(),
        "BATCH1".to_string(),
        "01".to_string(),
        groups,
        State::new(),
        Arc::new(args),
        fire_at,
        0.35,
        grab::Gate::new(),
    );

    let writes = mock.write_count();
    let stamps = mock.writes.lock().unwrap().clone();
    assert!(
        writes >= 2,
        "停机期间仍在写（不是安静地轮询）：只发了 {writes} 发 @ {stamps:?}"
    );
    mock.close();
}

/// 2026-09-26 20:00 的真实事故：放课瞬间学校把选课子系统推倒重排，会话被作废，
/// 认证接口同时返回 `#E2140600091 认证失败`。
///
/// 旧版在 `Verdict::Expired` 里只调用**一次** `recover()`，失败就 `return` ——
/// 那天真实日志是 20:00:00 放课、**20:00:04 进程就退出了**，90 秒的窗口扔掉 86 秒，
/// 而学校 20:00:20 左右就能重新登录（09-25 那晚就是这样）。09-25 / 09-26 连续两晚
/// 都是这个死法，所以这条路径必须钉死。
///
/// 断言的是"恢复之后还在抢"，而不只是"日志好看"：旧代码在 `AUTH_BACK_AFTER`
/// 之前就返回了，一条写请求都不会落在停机之后。
#[test]
fn auth_outage_at_the_window_does_not_abandon_the_run() {
    let mock = Mock::start(Mode::KickThenAuthBack, vec![LoginReply::Ok, LoginReply::Ok]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "tok",
        "JSESSIONID=x",
        "http://x/y.do?token=tok",
        4.0,
        // 故意只给 1 次重登录配额：窗口里必须能突破它（relax_relogin_limit），
        // 但真正收口的是墙钟（--window），不是次数。
        Some(relogin_manager(&ep, 1, 2)),
    );
    school.set_code(STUDENT);
    school.set_pacer(Arc::new(WritePacer::new(3, 1.0, 0.15, 0.10)));

    let args = match cli::parse(
        [
            "--url",
            "http://x/y.do?token=t",
            "--live",
            "--window",
            "11.0",
            "--burst",
            "0.2",
            "--interval",
            "1.0",
            "--slow",
            "1.0",
        ]
        .iter()
        .map(|s| s.to_string()),
    )
    .unwrap()
    {
        cli::Parsed::Run(a) => *a,
        _ => unreachable!(),
    };

    let groups = vec![Group {
        name: "星期一-3-5".to_string(),
        members: vec![
            grab::Target::new("000000000000000000000001", "A班"),
            grab::Target::new("000000000000000000000002", "B班"),
        ],
    }];
    let t_start = timeutil::unix_now();
    grab::retry_loop(
        Arc::clone(&school),
        STUDENT.to_string(),
        "BATCH1".to_string(),
        "01".to_string(),
        groups,
        State::new(),
        Arc::new(args),
        t_start,
        0.35,
        grab::Gate::new(),
    );
    let elapsed = timeutil::unix_now() - t_start;

    let stamps = mock.writes.lock().unwrap().clone();
    let after_back = stamps.iter().filter(|t| **t >= AUTH_BACK_AFTER).count();
    assert!(
        after_back >= 1,
        "学校恢复之后必须还在抢，而不是提前退出（窗口 11.0s，实际跑了 {elapsed:.1}s）：\
         写请求时间轴 {stamps:?}（停机 {AUTH_BACK_AFTER}s）"
    );
    assert!(
        elapsed >= AUTH_BACK_AFTER,
        "整个任务在停机结束前就退出了：只跑了 {elapsed:.1}s"
    );
    assert!(
        !mock.calls_to("login.do").is_empty(),
        "重排数据期间至少要真的试过重登录"
    );
    mock.close();
}

/// 2026-09-27 20:00 的真实事故：学校初始化结束之后所有候选都满员，脚本于是**安静地
/// 只读轮询**（设计如此，不发写请求）。但"停机结束"这个状态以前只认写请求的判决
/// （FULL / 正常回复），而那时候压根没有写请求 —— 于是 `outage_since` 一直挂着，
/// 换组条件里那句 `outage_since <= 0` 就把换组永久挡住了：
/// 那晚 306/307/308/309 那一组轮询了 9 分钟，而 **318 组从头到尾一发没试**
/// （"进入冲突组 星期五-3-5" 打印出来的时刻正好是窗口截止那一刻）。
/// 318 组有 4 个非主选名额，是唯一名额多一倍的一组。
///
/// 断言：第二个冲突组的教学班**被问过容量**。
#[test]
fn full_candidates_after_the_outage_still_roll_to_the_next_group() {
    let mock = Mock::start(Mode::OutageThenFull, vec![]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "tok",
        "JSESSIONID=x",
        "http://x/y.do?token=tok",
        4.0,
        None,
    );
    school.set_code(STUDENT);
    school.set_pacer(Arc::new(WritePacer::new(3, 1.0, 0.15, 0.10)));

    let args = match cli::parse(
        [
            "--url",
            "http://x/y.do?token=t",
            "--live",
            "--window",
            "4.0",
            "--burst",
            "0.2",
            "--interval",
            "1.0",
            "--poll",
            "5",
        ]
        .iter()
        .map(|s| s.to_string()),
    )
    .unwrap()
    {
        cli::Parsed::Run(a) => *a,
        _ => unreachable!(),
    };

    let groups = vec![
        Group {
            name: "星期三-3-5".to_string(),
            members: vec![grab::Target::new("000000000000000000000001", "A班")],
        },
        Group {
            name: "星期五-3-5".to_string(),
            members: vec![grab::Target::new("000000000000000000000002", "B班")],
        },
    ];
    grab::retry_loop(
        Arc::clone(&school),
        STUDENT.to_string(),
        "BATCH1".to_string(),
        "01".to_string(),
        groups,
        State::new(),
        Arc::new(args),
        timeutil::unix_now(),
        0.35,
        grab::Gate::new(),
    );

    let asked: Vec<String> = mock
        .calls_to("capacity.do")
        .iter()
        .map(|c| c.field("teachingClassId"))
        .collect();
    assert!(
        asked.iter().any(|id| id.ends_with("000002")),
        "第二冲突组一次都没被问到 —— 换组被 outage_since 挡住了：{asked:?}"
    );
    // 关键的那半条：**停机结束之后**还在轮转第二组。
    // 2026-09-27 那晚的 bug 不是"从没问过第二组"，而是"停机一结束就再也不换组了"
    // ——脚本在第一组上安静地轮询了 9 分钟。
    let late: Vec<f64> = mock
        .calls_to("capacity.do")
        .iter()
        .filter(|c| c.field("teachingClassId").ends_with("000002") && c.at >= OUTAGE_ENDS)
        .map(|c| c.at)
        .collect();
    assert!(
        !late.is_empty(),
        "停机结束后就没再问过第二冲突组（那晚就是这样漏掉 318 的）：{asked:?}"
    );
    mock.close();
}

/// 造一个"带着课程号与冲突组"的候选（`retry_loop` 直接吃 Target）。
fn tgt(tc: &str, label: &str, course_idx: usize, group: &str) -> grab::Target {
    grab::Target {
        tc: tc.to_string(),
        label: label.to_string(),
        tc_type: None,
        course_idx,
        group: group.to_string(),
        is_major: None,
    }
}

/// 抢到一门课之后**继续抢其余的**，而且绝不能双选。
///
/// 顺带把多课程之后"不双选"那两条不变量钉住 —— 它们现在是唯一的依据，
/// 轮转调度本身不再承担这个职责（旧写法靠"换组就不回头"来防，代价是 09-27
/// 那晚第二组整晚没被碰过）：
///   ① 每门课最多中一个；
///   ② 同一个冲突组（星期-节次）最多中一个 —— 跨课程的时间冲突落在同一个组里。
#[test]
fn winning_one_course_keeps_going_and_never_double_selects() {
    let mock = Mock::start(Mode::AlwaysWin, vec![]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "tok",
        "JSESSIONID=x",
        "http://x/y.do?token=tok",
        4.0,
        None,
    );
    school.set_code(STUDENT);
    school.set_pacer(Arc::new(WritePacer::new(3, 1.0, 0.15, 0.10)));

    let args = match cli::parse(
        [
            "--url",
            "http://x/y.do?token=t",
            "--live",
            "--window",
            "8.0",
            "--burst",
            "0.0",
            "--interval",
            "0.5",
            "--poll",
            "6",
        ]
        .iter()
        .map(|s| s.to_string()),
    )
    .unwrap()
    {
        cli::Parsed::Run(a) => *a,
        _ => unreachable!(),
    };

    // 第一门课有两个班在**同一个时段**（同组互斥）；第二门课在另一个时段
    let groups = vec![
        Group {
            name: "星期三-3-5".to_string(),
            members: vec![
                tgt("000000000000000000000001", "线代A班", 0, "星期三-3-5"),
                tgt("000000000000000000000002", "线代B班", 0, "星期三-3-5"),
            ],
        },
        Group {
            name: "星期五-3-5".to_string(),
            members: vec![tgt("000000000000000000000003", "大物A班", 1, "星期五-3-5")],
        },
    ];
    let state = State::for_courses(2);
    grab::retry_loop(
        Arc::clone(&school),
        STUDENT.to_string(),
        "BATCH1".to_string(),
        "01".to_string(),
        groups,
        Arc::clone(&state),
        Arc::new(args),
        timeutil::unix_now(),
        0.35,
        grab::Gate::new(),
    );

    let submitted = mock.submitted.lock().unwrap().clone();
    assert!(
        submitted.iter().any(|id| id.ends_with("000001")),
        "第一门课该被提交：{submitted:?}"
    );
    assert!(
        !submitted.iter().any(|id| id.ends_with("000002")),
        "同一个时段的另一个班不该再提交（那就是双选）：{submitted:?}"
    );
    assert!(
        submitted.iter().any(|id| id.ends_with("000003")),
        "抢到一门之后该继续抢第二门课：{submitted:?}"
    );
    let got = state.acquired();
    assert!(
        got.iter().all(|a| a.is_some()),
        "两门课都该确认到手（抢齐了才收工）：{got:?}"
    );
    assert!(state.is_done());
    mock.close();
}

/// `--forever` + `--poll`：常驻要跑过窗口，而且**读取速率是常数**。
///
/// 常驻的代价全在读取量上（写请求只在真有空位时才发），所以"每秒最多读几个"
/// 必须是硬约束：一个时间片只读一个候选，候选再多也只是轮得慢一些。
#[test]
fn forever_polls_past_the_window_and_honours_poll() {
    let mock = Mock::start(Mode::AlwaysFull, vec![]);
    let ep = endpoints(mock.port);
    let school = School::new(
        Arc::clone(&ep),
        "tok",
        "JSESSIONID=x",
        "http://x/y.do?token=tok",
        4.0,
        None,
    );
    school.set_code(STUDENT);
    school.set_pacer(Arc::new(WritePacer::new(3, 1.0, 0.15, 0.10)));

    let args = match cli::parse(
        [
            "--url",
            "http://x/y.do?token=t",
            "--live",
            "--forever",
            "--window",
            "2.0",
            "--burst",
            "0.0",
            "--interval",
            "1.0",
            "--poll",
            "2",
        ]
        .iter()
        .map(|s| s.to_string()),
    )
    .unwrap()
    {
        cli::Parsed::Run(a) => *a,
        _ => unreachable!(),
    };

    let groups = vec![Group {
        name: "星期三-3-5".to_string(),
        members: vec![tgt("000000000000000000000001", "A班", 0, "星期三-3-5")],
    }];
    let state = State::new();
    let worker = {
        let school = Arc::clone(&school);
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            grab::retry_loop(
                school,
                STUDENT.to_string(),
                "BATCH1".to_string(),
                "01".to_string(),
                groups,
                state,
                Arc::new(args),
                timeutil::unix_now(),
                0.35,
                grab::Gate::new(),
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_secs(3));
    let stopped_at = mock.t0.elapsed().as_secs_f64();
    state.finish_quiet(); // 常驻没有自然的终点，测试里手动收工
    worker.join().unwrap();

    let caps = mock.calls_to("capacity.do");
    assert!(caps.len() >= 3, "该在只读轮询：{} 次容量请求", caps.len());
    let last = caps.last().unwrap().at;
    assert!(
        last >= 2.5,
        "常驻模式该跑过 --window 2.0s（最后一次读容量在 {last:.2}s，收工于 {stopped_at:.2}s）"
    );
    assert_eq!(
        mock.write_count(),
        0,
        "候选全满时一发写请求都不该发（写请求是稀缺资源）"
    );
    let span = last - caps.first().unwrap().at;
    let rate = (caps.len() as f64 - 1.0) / span.max(0.001);
    assert!(
        rate <= 2.0 * 1.3,
        "读取速率 {rate:.2}/s 超过 --poll 2（容差 ×1.3）"
    );
    mock.close();
}

// ==========================================================================
// [N] 开荒：让普通用户"粘一条网址 + 学号密码"就能把配置填出来
// ==========================================================================

/// `log::with_sink` 是 TUI 里"把库的进度日志搬进自己的面板"那根线：
/// 设置之后 `info()` 不再往 stdout 打，而是交给回调。
#[test]
fn log_sink_captures_progress_lines() {
    use std::sync::{Arc, Mutex};
    let got = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&got);
    course_grabber::log::with_sink(
        move |m| sink.lock().unwrap().push(m.to_string()),
        || {
            course_grabber::log::log("第一行");
            course_grabber::log::info("第二行");
        },
    );
    let lines = got.lock().unwrap().clone();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "第一行");
    assert!(lines[1].contains("第二行"), "{:?}", lines[1]);
    assert!(
        lines[1].starts_with('['),
        "info() 要带时间戳：{:?}",
        lines[1]
    );

    // 出了 with_sink 之后回到 stdout —— 这里只确认不 panic、也不写进已经关掉的 sink
    course_grabber::log::with_sink(|_| {}, || course_grabber::log::log("不进 stdout 的"));
    course_grabber::log::log("");
}

/// 从网址推出"学校"那一节，再接上"按参考实现补齐"——
/// 合起来就是用户按一次 Enter 之后发生的事。
#[test]
fn onboarding_fills_a_runnable_school_section() {
    use course_grabber::json::Json;
    let mut raw = json::parse(r#"{"school":{"host":"course.example.edu.cn"}}"#).unwrap();
    // 用户最可能粘的那条：完整选课页网址（带 token）
    let input = "http://jw.example.edu.cn/course-system/*default/grablessons.do?token=abcd1234";
    let parts = course_grabber::onboard::parse_input(input).unwrap();
    let d = course_grabber::onboard::derive_from_path(&parts, &parts.path);
    let notes = course_grabber::onboard::apply(&mut raw, &d, Some("/course-system/"));

    // 推导出来的四项
    assert_eq!(
        raw.object("school").unwrap().text("host"),
        "jw.example.edu.cn"
    );
    assert_eq!(
        raw.object("school").unwrap().text("base_path"),
        "/course-system"
    );
    assert_eq!(
        raw.object("school").unwrap().text("page_path"),
        "/course-system/*default/index.do"
    );
    // 补齐的键让这份配置真的能跑起来
    assert_eq!(raw.object("password").unwrap().array("des_keys").len(), 3);
    assert_eq!(raw.object("cookies").unwrap().array("captcha").len(), 2);
    assert!(raw
        .object("paths")
        .unwrap()
        .text("volunteer")
        .contains("{base}"));

    // 补完之后程序自己就认了（除了 host 指向一个不存在的地方，配置层面是完整的）
    let ep = Config::from_raw(raw.clone(), "tui")
        .endpoints()
        .expect("补齐之后配置应当能解析");
    assert_eq!(ep.base_path, "/course-system");
    assert_eq!(ep.host, "jw.example.edu.cn");
    assert_eq!(ep.path("volunteer"), "/course-system/elective/volunteer.do");

    // 提示里必须说清楚"哪一条是猜的"——不许让用户以为全是确定的
    assert!(
        notes
            .iter()
            .any(|(l, t)| *l == course_grabber::onboard::Level::Warn && t.contains("参考实现")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|(_, t)| t.contains("Referer")),
        "得说明 Referer 没动：{notes:?}"
    );
    let _ = Json::Null;
}

/// 登录 → 读批次 → 查课程目录 → 挑候选，整条链路对着假学校跑通。
#[test]
fn onboarding_fetches_candidates_from_mock_school() {
    let mock = Mock::start(Mode::Catalog, vec![]);
    let ep = endpoints(mock.port);
    // 每一类都拉了一遍（假学校对每一类都回同样那门课，所以五类五份）
    let got = course_grabber::onboard::fetch_catalog(&ep, &creds(), &StubSolver, 6, 0.01)
        .expect("登录 + 拉目录应当成功");
    assert_eq!(got.types.len(), 5, "{:?}", got.types);
    assert!(got.types.iter().all(|(_, _, n)| *n == 4), "{:?}", got.types);
    // 每一行都带着自己属于哪一类 —— 提交选课时要带对
    for r in &got.rows {
        assert!(!r.tc_type.is_empty());
    }
    assert!(got.rows.iter().any(|r| r.tc_type == "FANKC"));
    assert!(got.rows.iter().any(|r| r.tc_type == "MOOC"));
    // 这一页没满（4 < 50），所以每一类只问了一页
    assert_eq!(mock.calls_to("programCourse.do").len(), 5);

    assert_eq!(got.student_name, "测试同学");
    assert_eq!(got.batch_code, "B1");
    assert_eq!(got.batch_name, "正选");
    assert_eq!(got.campus, "01");
    // 不筛关键词：整类课都在里面（"别的课"那门也在 —— 这才叫"拉全量让用户自己挑"）
    assert_eq!(got.rows.len(), 20, "五类 × 4 个教学班");
    assert!(got
        .rows
        .iter()
        .any(|r| r.tc_id == "000000000000000000000999"));

    // 冲突组从上课地点里推出来；推不出来（"待定"）就照实留空
    assert_eq!(got.rows[0].group, "星期一-3-5");
    assert_eq!(got.rows[1].group, "星期三-3-5");
    assert_eq!(got.rows[2].group, "");
    assert_eq!(got.rows[0].label(), "01班 张老师 星期一-3-5");
    assert_eq!(got.rows[1].is_full, "1");
    assert_eq!(got.rows[2].is_conflict, "1");
    // 全程只读：一条写请求都不许发
    assert_eq!(mock.write_count(), 0, "开荒阶段绝不能有写请求");

    // 勾两个写回配置：整段替换，并记下关键词
    let raw = json::parse(TEST_CONFIG).unwrap();
    // 多课程：写进第 0 门课（老配置进来时界面会先把 course 搬进 courses[0]）
    let mut raw = {
        let mut r = raw;
        let old = r.get("course").cloned().unwrap_or(json::Json::Null);
        r.set_key("courses", json::Json::Arr(vec![old]));
        r.remove_key("course");
        r
    };
    let n = course_grabber::onboard::write_candidates(
        &mut raw,
        0,
        &[&got.rows[0], &got.rows[2]],
        "测试课程",
    );
    assert_eq!(n, 2);
    let cands = raw.array("courses")[0].array("candidates").to_vec();
    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0].text("id"), "000000000000000000000101");
    assert_eq!(cands[0].text("label"), "01班 张老师 星期一-3-5");
    assert_eq!(cands[0].text("group"), "星期一-3-5");
    assert_eq!(cands[1].text("group"), "", "推不出冲突组就留空，别编一个");
    // 类型写进每个候选：一次可能同时挑方案内 + 方案外，提交时每一发都要带对
    assert_eq!(cands[0].text("type"), "FANKC");
    mock.close();
}

/// 目录整个是空的（不是选课时间 / 学校没开）时，要说人话。
#[test]
fn onboarding_says_something_useful_when_the_catalog_is_empty() {
    // 有批次、但目录是空的（不是选课时间时就是这样）
    let mock = Mock::start(Mode::EmptyCatalog, vec![]);
    let ep = endpoints(mock.port);
    let err = course_grabber::onboard::fetch_catalog(&ep, &creds(), &StubSolver, 6, 0.01)
        .expect_err("目录为空就该报错");
    assert!(err.contains("一门都没有"), "{err}");
    assert!(err.contains("不是选课时间"), "{err}");
    mock.close();
}

/// 只读自检：假学校对不存在的路径也回 200，所以它必须**照实说"判断不了"**，
/// 不能因为看到 200 就宣布"路径存在"。
#[test]
fn probe_reports_honestly_against_the_mock() {
    use course_grabber::onboard::Level;
    let mock = Mock::start(Mode::Normal, vec![]);
    let ep = endpoints(mock.port);
    let lines = course_grabber::onboard::probe(&ep);

    assert!(lines.len() > 3, "{lines:?}");
    // 先报了"对照路径"
    assert!(lines.iter().any(|(_, t)| t.contains("对照")), "{lines:?}");
    // 顺便读到了服务器时钟（Date 头），这是对时那套的输入
    assert!(
        lines.iter().any(|(_, t)| t.contains("服务器时钟")),
        "{lines:?}"
    );
    // 关键：假服务端什么都回 200，所以判断只能是"判断不了"，绝不是"存在"
    assert!(
        lines.iter().any(|(_, t)| t.contains("判断不了")),
        "对不存在的路径也回 200 的服务器，必须照实说判断不了：{lines:?}"
    );
    // 没有一条被判成 ✗：它明明有响应
    assert!(lines.iter().all(|(l, _)| *l != Level::Err), "{lines:?}");
    assert_eq!(mock.write_count(), 0, "自检只发 GET");
    mock.close();
}

/// "只粘一个域名"也要能配好 —— 服务器把页面路径告诉我们（根路径 302 到应用页）。
#[test]
fn onboarding_discovers_everything_from_a_bare_host() {
    let mock = Mock::start(Mode::Catalog, vec![]);
    let out = course_grabber::onboard::discover(&format!("127.0.0.1:{}", mock.port), &|_| {})
        .expect("只给域名也应当能问到路径");

    assert_eq!(out.derived.host, "127.0.0.1");
    assert_eq!(out.derived.port, mock.port as i64);
    assert_eq!(out.derived.base_path.as_deref(), Some("/api"));
    assert_eq!(
        out.derived.page_path.as_deref(),
        Some("/api/*default/index.do")
    );
    // 每条结论都要能看见"凭什么"：重定向、接口前缀验证、Cookie
    let said = |needle: &str| out.notes.iter().any(|(_, t)| t.contains(needle));
    assert!(said("指到了"), "{:?}", out.notes);
    assert!(said("接口前缀验证过"), "{:?}", out.notes);
    // 假服务端每个响应都带 Date，所以第一候选 "/" 就够用 —— 不必写 time_path
    assert_eq!(out.time_path, None);
    assert!(said("带 Date 头"), "{:?}", out.notes);
    // 顺路记下服务器下发的 Cookie 名
    assert!(
        out.cookie_names.iter().any(|n| n == "route"),
        "{:?}",
        out.cookie_names
    );

    // 全程只读：一条写请求都没有
    assert_eq!(mock.write_count(), 0);
    mock.close();
}

/// 认不出的系统（既没有 `*default` 也没有 `/api`）：只填域名端口，并且**明说**。
#[test]
fn onboarding_says_so_when_it_cannot_figure_out_the_paths() {
    let mock = Mock::start(Mode::Normal, vec![]);
    // 这条路径既没有 `*default`，也不是根路径 —— 服务器回什么都不会被认成应用页
    let out = course_grabber::onboard::discover(
        &format!(
            "http://127.0.0.1:{}/jwglxt/xtgl/login_slogin.html",
            mock.port
        ),
        &|_| {},
    )
    .expect("连通了就该有结果（只是推不出路径）");
    assert_eq!(out.derived.host, "127.0.0.1");
    assert!(out.derived.base_path.is_none());
    assert!(
        !out.derived.warns.is_empty()
            || out
                .notes
                .iter()
                .any(|(l, _)| *l == course_grabber::onboard::Level::Warn),
        "推不出路径必须说清楚，不能让用户以为配好了：{:?} {:?}",
        out.derived.warns,
        out.notes
    );
    mock.close();
}

/// 服务器下发的会话 Cookie 名比配置里列的全时，要**自己补上并重登一次**。
///
/// 真实例子：脱敏后的示例配置 `cookies.session` 只有 `JSESSIONID`，而学校登录时
/// 还会下发 `_WEU` —— 少带一个 Cookie，后面每个接口都可能被判未登录。
/// 假学校在 login.do 上下发 `JSESSIONID` + `EXTRA`，正好当这个场景的替身。
#[test]
fn onboarding_widens_session_cookies_observed_at_login() {
    let mock = Mock::start(Mode::Catalog, vec![]);
    let ep = endpoints(mock.port);
    assert_eq!(
        ep.session_cookies,
        vec!["JSESSIONID"],
        "测试配置里只列了一个"
    );

    let got = course_grabber::onboard::fetch_catalog(&ep, &creds(), &StubSolver, 6, 0.01)
        .expect("补上 Cookie 之后应当照样跑通");

    assert!(
        got.session_cookies.iter().any(|n| n == "EXTRA"),
        "登录时下发的 EXTRA 应当被补进名单：{:?}",
        got.session_cookies
    );
    assert_eq!(got.session_cookies[0], "JSESSIONID", "配置里写的排前面");
    // 补上之后**用新名单重新登录过**：所以 login.do 被打了两次
    assert_eq!(
        mock.calls_to("login.do").len(),
        2,
        "名单变了要重登一次，否则这次会话仍然缺 Cookie"
    );
    // 依然全程只读
    assert_eq!(mock.write_count(), 0);
    mock.close();
}

/// 仓库里不许出现真实学校的信息 —— 这是这个项目的立足点
/// （"一套配置驱动的工具"，不是"某校的工具"）。
///
/// 只查最容易漏、也最伤的一类：源码 / 配置 / 文档里出现的 `*.edu.cn` 主机名
/// 必须落在 `example` 域里（占位符长这样：`jw.example.edu.cn`）。
///
/// 写这条测试是因为真的漏过 —— 有个功能是照着真实系统做的，写测试时顺手就把
/// 真实域名和部署路径抄了进去，`grep` 才发现。它拦不住"把真实路径抄进去"
/// （路径没有通用形状可判），那还得靠人看着。
#[test]
fn source_has_no_real_school_names() {
    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if p.is_dir() {
                // 构建产物和工作区不查
                if matches!(name, "target" | "dist" | ".git" | "__pycache__") {
                    continue;
                }
                collect(&p, out);
            } else if matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("rs" | "json" | "md" | "sh" | "yml" | "yaml" | "toml")
            ) {
                out.push(p);
            }
        }
    }

    /// 一段文本里所有"教育网主机名"，以及其中不像占位符的那些。
    fn suspicious(text: &str) -> Vec<String> {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(at) = text[i..].find(".edu.cn") {
            let at = i + at + ".edu.cn".len();
            // 往左走：取完整的主机名
            let mut start = at - ".edu.cn".len();
            while start > 0
                && (bytes[start - 1].is_ascii_alphanumeric()
                    || matches!(bytes[start - 1], b'.' | b'-'))
            {
                start -= 1;
            }
            i = at;
            let host = &text[start..at];
            // 得有"主机名"的形状：至少一个字母/数字打头的标签，否则就是裸写的 ".edu.cn"
            let has_label = host.len() > ".edu.cn".len()
                && host[..host.len() - ".edu.cn".len()]
                    .rsplit('.')
                    .next()
                    .map(|l| !l.is_empty() && l.chars().any(|c| c.is_ascii_alphabetic()))
                    .unwrap_or(false);
            if has_label && !host.ends_with("example.edu.cn") {
                out.push(host.to_string());
            }
        }
        out
    }

    // 先证明这个检查真的会报警（用一个不存在学校的域名当样本）
    assert_eq!(
        suspicious("见 http://jw.some-university.edu.cn/ 的说明"),
        vec!["jw.some-university.edu.cn".to_string()]
    );
    assert!(suspicious("占位符 jw.example.edu.cn 不算").is_empty());

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(root, &mut files);
    assert!(files.len() > 5, "没扫到文件，测试本身有问题");

    let mut bad: Vec<String> = Vec::new();
    for f in &files {
        // 跳过守卫自己：它的样本字符串、以及下面那句格式串，本来就会命中这个模式
        if f.ends_with("tests/offline.rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for host in suspicious(&text) {
            bad.push(format!(
                "{}: {host}",
                f.strip_prefix(root).unwrap_or(f).display()
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "这些文件里出现了像真实学校的主机名（要脱敏成 *.example.edu.cn）：\n{}",
        bad.join("\n")
    );
}

/// 目录查询会把**每一类**都拉一遍，而且每一行都带着自己属于哪一类。
///
/// 为什么要这样：同一门课在方案内 / 方案外 / 校公选下都可能开，名字还一模一样 ——
/// 只让用户打关键词搜，他分不清是哪一类；只查一类，又会漏掉其他的。
#[test]
fn onboarding_pulls_every_category_and_pages_through() {
    let mock = Mock::start(Mode::ByCategory, vec![]);
    let ep = endpoints(mock.port);
    let got = course_grabber::onboard::fetch_catalog(&ep, &creds(), &StubSolver, 6, 0.01)
        .expect("每一类都该拉到东西");

    // 五类都有结果，每类 55 个教学班（假服务端发了 50 + 5 两页）
    assert_eq!(got.types.len(), 5, "{:?}", got.types);
    assert!(
        got.types.iter().all(|(_, _, n)| *n == 55),
        "{:?}",
        got.types
    );
    assert_eq!(got.rows.len(), 275, "五类 × 每类 55");

    // 每一行都标着自己的类别，ID 也能对上（提交选课时带的就是这个类别）
    for r in &got.rows {
        assert!(!r.tc_type.is_empty());
        assert!(
            r.tc_id.contains(&r.tc_type),
            "{} 的类别标签应当是 {}",
            r.tc_id,
            r.tc_type
        );
    }
    // 分页确实翻了：五类 × 两页 = 10 次目录请求
    assert_eq!(
        mock.calls_to("programCourse.do").len(),
        10,
        "每一类都该按 totalCount 翻到第二页"
    );
    // 第二页也真的被算进去了（不是只看第一页）
    assert!(got.rows.iter().any(|r| r.tc_id.ends_with("-54")));
    // 全程只读
    assert_eq!(mock.write_count(), 0);
    mock.close();
}

/// 类型代码翻成人话（界面上给用户看的）。
#[test]
fn course_type_labels() {
    use course_grabber::onboard::{type_label, COURSE_TYPES};
    assert_eq!(type_label("FANKC"), "方案内课程（FANKC）");
    assert_eq!(type_label("fawkc"), "方案外课程（FAWKC）");
    assert_eq!(type_label(""), "（没填课程类型）");
    assert_eq!(type_label("XXXX"), "XXXX");
    // 类型表里必须有这两类 —— 方案内/方案外是"能不能抢必修课"的分水岭
    let codes: Vec<&str> = COURSE_TYPES.iter().map(|(c, _)| *c).collect();
    assert!(
        codes.contains(&"FANKC") && codes.contains(&"FAWKC"),
        "{codes:?}"
    );
}
