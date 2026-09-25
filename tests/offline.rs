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
use mock::{LoginReply, Mock, Mode};

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
    let res = school.submit(STUDENT, "B1", "TC1", "01", None, Some(2.0));
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
    let targets: Vec<String> = vec!["TC1".to_string(), "TC2".to_string(), "TC3".to_string()];

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
            ("000000000000000000000001".to_string(), "A班".to_string()),
            ("000000000000000000000002".to_string(), "B班".to_string()),
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
