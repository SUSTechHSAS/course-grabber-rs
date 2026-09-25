//! 自动登录 / 被踢重登录（学号 + 密码 → 有效会话）
//!
//! 对接的是常见的"账号密码 + 点选验证码"教务登录页，流程与前端 JS 逐字段一致：
//!
//!     1. POST {vcode_token}?timestamp=<ms>
//!        不带任何 Cookie            → {"code":"1","data":{"token":"<vtoken>"}}
//!
//!     2. GET  {vcode_image}?vtoken=<vtoken>
//!        不带任何 Cookie            → JPEG 点选验证码
//!                                    + Set-Cookie: 本轮验证码 Cookie
//!
//!     3. 本地识别（编译进来的 click-captcha-matcher）→ 4 个点击坐标，按提示顺序
//!        提交格式 "x-y,x-y,x-y,x-y"（与前端点击结果序列化格式一致）
//!
//!     4. POST {login}
//!        Cookie: **只带本轮**的验证码 Cookie（绝不能混入旧会话的登录 Cookie ——
//!                同名 Cookie 重复时服务端取值顺序不可预测，会把登录打死）
//!        Body:   loginPwd   = base64(DES3(密码, 配置里的密钥))
//!                loginName  = 学号
//!                vtoken     = 第 1 步的 token
//!                verifyCode = 第 3 步的坐标串
//!        → code=1 成功，data.token 就是之后 `token:` 头要用的那一串，
//!                  同时下发新的登录 Cookie
//!
//! 返回码语义（前端 JS 原文）:
//!     1 = 成功          2 = 登录名或密码不正确   3 = 验证码不正确
//!     4 = 在线人数超过上限                     其他 = msg 里给原因
//!
//! **防锁号策略**：只有 code=2 会被当成"密码错"，一旦出现就立刻放弃本次运行并且
//! 永不再试 —— 密码错、账号被锁都要人来处理，脚本绝不硬猜。验证码错(code=3)、
//! 在线人数超限(code=4)、网络抖动都只换一张图/退避后重试。

use std::path::Path;
use std::sync::Arc;

use crate::captcha::{Rejected, Solver};
use crate::config::Endpoints;
use crate::des::encrypt_password;
use crate::httpc::{self, Response};
use crate::log::info;
use crate::timeutil;

const WRONG_CAPTCHA: &str = "验证码不正确";
const WRONG_PASSWORD_HINTS: [&str; 3] = ["登录名或密码不正确", "密码不正确", "用户名或密码"];
const ONLINE_LIMIT_HINTS: [&str; 2] = ["在线人数超过上限", "在线人数已达上限"];

#[derive(Debug)]
pub enum AuthError {
    /// 学号或密码不正确 —— 绝不重试，必须人工核对
    BadCredentials(String),
    /// 验证码被拒（识别错误或 token 过期）—— 换一张图重试
    CaptchaRejected(String),
    /// 网络/协议/在线人数上限等临时问题 —— 退避后重试
    LoginUnavailable(String),
    /// 认证服务本身不可用（实测放课瞬间会返回 `#E2140600091 认证失败`）。
    /// 这不是凭据或流程有问题，而是服务端在重排数据；很短暂（实测约 20 秒），
    /// 所以不该消耗自动重登录的次数配额。
    AuthServiceDown(String),
}

impl AuthError {
    pub fn message(&self) -> &str {
        match self {
            AuthError::BadCredentials(m)
            | AuthError::CaptchaRejected(m)
            | AuthError::LoginUnavailable(m)
            | AuthError::AuthServiceDown(m) => m,
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for AuthError {}

// ==========================================================================
// 一、凭据（默认路径见 config.json 的 credentials_path，建议 0600）
// ==========================================================================

#[derive(Clone)]
pub struct Credentials {
    pub student_id: String,
    pub password: String,
    pub source: String,
}

/// 绝不把密码写进日志（Debug 也要挡住 —— `{:?}` 是很容易顺手写出来的）。
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credentials(student_id={:?}, password=***, source={:?})",
            self.student_id, self.source
        )
    }
}

impl Credentials {
    /// 打印给用户看的脱敏学号：`2026****01`
    pub fn masked(&self) -> String {
        mask_id(&self.student_id)
    }
}

pub fn mask_id(code: &str) -> String {
    if code.chars().count() >= 6 {
        let head: String = code.chars().take(4).collect();
        let tail: String = code
            .chars()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}****{tail}")
    } else {
        code.to_string()
    }
}

fn pick(obj: &crate::json::Json, names: &[&str]) -> String {
    for n in names {
        if let Some(v) = obj.get(n) {
            let s = v.as_text().trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

/// 按「命令行参数 → 凭据文件」的顺序取学号密码；都没有则返回 Ok(None)。
///
/// 命令行只作为临时覆盖：正常用法是把密码放进 0600 的凭据文件，
/// 这样密码不进 shell 历史、也不出现在 `ps` 里。
pub fn load_credentials(
    path: Option<&str>,
    student_id: Option<&str>,
    password: Option<&str>,
    default_path: &str,
) -> Result<Option<Credentials>, AuthError> {
    let cli_student = student_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let cli_password = password.map(|s| s.to_string()).filter(|s| !s.is_empty());
    match (cli_student, cli_password) {
        (Some(sid), Some(pwd)) => {
            return Ok(Some(Credentials {
                student_id: sid,
                password: pwd,
                source: "命令行".to_string(),
            }))
        }
        (None, Some(_)) => {
            return Err(AuthError::LoginUnavailable(
                "给了 --password 但没给 --student（学号无法可靠地自动推断）".to_string(),
            ))
        }
        (Some(_), None) => {
            return Err(AuthError::LoginUnavailable(
                "给了 --student 但没给 --password".to_string(),
            ))
        }
        (None, None) => {}
    }

    let p = crate::config::expand_tilde(path.unwrap_or(default_path));
    if !p.is_file() {
        return Ok(None);
    }

    warn_if_world_readable(&p);

    let text = std::fs::read_to_string(&p).map_err(|e| {
        AuthError::LoginUnavailable(format!("凭据文件读取失败 {}: {e}", p.display()))
    })?;
    let mut raw = crate::json::parse_or_empty(&text);
    // 容忍 {"credentials": {...}} 包一层
    if let Some(inner) = raw
        .get("credentials")
        .filter(|v| matches!(v, crate::json::Json::Obj(_)))
    {
        raw = inner.clone();
    }
    if !matches!(raw, crate::json::Json::Obj(_)) {
        return Err(AuthError::LoginUnavailable(format!(
            "凭据文件格式不对（应为 JSON 对象）: {}",
            p.display()
        )));
    }

    let sid = pick(
        &raw,
        &[
            "student_id",
            "studentId",
            "student",
            "code",
            "username",
            "loginName",
            "学号",
        ],
    );
    let pwd = pick(&raw, &["password", "passwd", "pwd", "密码"]);
    if sid.is_empty() || pwd.is_empty() {
        return Err(AuthError::LoginUnavailable(format!(
            "凭据文件里缺少 student_id / password: {}",
            p.display()
        )));
    }
    if !(6..=12).contains(&sid.len()) || !sid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AuthError::LoginUnavailable(format!(
            "凭据文件里的学号不像学号: {sid:?}"
        )));
    }
    Ok(Some(Credentials {
        student_id: sid,
        password: pwd,
        source: p.display().to_string(),
    }))
}

/// 权限比 0600 松就提醒一句（不阻断 —— 用户可能是有意为之）。
fn warn_if_world_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                crate::log::log(&format!(
                    "⚠ {} 权限是 {mode:o}，同机其他用户可能读到你的密码；建议 chmod 600 {}",
                    path.display(),
                    path.display()
                ));
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

// ==========================================================================
// 二、验证码：抓图（学校接口）+ 识别（内编模型）
// ==========================================================================

/// 从 Set-Cookie 里取出指定名字，保持 names 的顺序，不破坏 Expires 里的逗号。
///
/// 与原版的 `re.finditer(r"(?:^|[,;]\s*)([A-Za-z_][A-Za-z0-9_]*)=([^;,]+)")` 等价：
/// 名字必须出现在串首或紧跟 `,`/`;`（允许中间有空白），值不能含 `,`/`;`。
/// 同名取最后一次出现（原版是往 dict 里赋值）。
pub fn set_cookie_values(set_cookie: &[String], names: &[String]) -> String {
    let text = set_cookie.join(", ");
    let bytes = text.as_bytes();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let starts_here = i == 0 || bytes[i - 1] == b',' || bytes[i - 1] == b';';
        if starts_here {
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            let name_start = j;
            if bytes
                .get(j)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            {
                j += 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'=') {
                    let value_start = j + 1;
                    let mut k = value_start;
                    while k < bytes.len() && bytes[k] != b';' && bytes[k] != b',' {
                        k += 1;
                    }
                    if k > value_start {
                        let name = text[name_start..j].to_string();
                        let value = text[value_start..k].trim().to_string();
                        pairs.retain(|(n, _)| n != &name); // 同名后者覆盖前者
                        pairs.push((name, value));
                        i = k;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    names
        .iter()
        .filter_map(|n| {
            pairs
                .iter()
                .rev()
                .find(|(k, _)| k == n)
                .map(|(_, v)| format!("{n}={v}"))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone)]
pub struct CaptchaChallenge {
    pub vtoken: String,
    /// route + insert_cookie，只属于这一张图
    pub cookie: String,
    pub image: Vec<u8>,
}

/// 登录流程用的请求头（与前端 XHR 一致；没有 Referer 时用业务页）。
fn auth_headers(
    ep: &Endpoints,
    referer: &str,
    has_body: bool,
    cookie: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Host".to_string(), ep.host_header()),
        ("User-Agent".to_string(), ep.ua.clone()),
        (
            "Accept".to_string(),
            "application/json, text/javascript, */*; q=0.01".to_string(),
        ),
        (
            "Accept-Language".to_string(),
            "zh-CN,zh;q=0.9,en;q=0.8".to_string(),
        ),
        (
            "Referer".to_string(),
            format!(
                "http://{}{}",
                ep.host,
                if referer.is_empty() {
                    &ep.page_path
                } else {
                    referer
                }
            ),
        ),
        ("X-Requested-With".to_string(), "XMLHttpRequest".to_string()),
        // http.client 默认就会发这一条；不发的话服务器可能回 gzip，而我们不解压
        ("Accept-Encoding".to_string(), "identity".to_string()),
    ];
    if has_body {
        headers.push((
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded; charset=UTF-8".to_string(),
        ));
    }
    if let Some(c) = cookie {
        headers.push(("Cookie".to_string(), c.to_string()));
    }
    headers
}

fn auth_request(
    ep: &Endpoints,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    cookie: Option<&str>,
    timeout: f64,
) -> std::io::Result<Response> {
    let headers = auth_headers(ep, "", body.is_some(), cookie);
    let mut stream = httpc::connect(&ep.host, ep.port, ep.connect_timeout)?;
    httpc::exchange(&mut stream, method, path, &headers, body, timeout)
}

/// 抓一张新验证码。不带任何 Cookie —— 这是登录页的原始契约。
pub fn fetch_captcha(ep: &Endpoints, timeout: f64) -> Result<CaptchaChallenge, AuthError> {
    let path = format!(
        "{}?timestamp={}",
        ep.path("vcode_token"),
        (timeutil::unix_now() * 1000.0) as i64
    );
    let resp = auth_request(ep, "POST", &path, Some(b""), None, timeout)
        .map_err(|e| AuthError::LoginUnavailable(format!("vcode.do 请求失败: {e}")))?;
    let payload = resp.json();
    let vtoken = payload
        .object("data")
        .map(|d| d.text("token"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if payload.text("code") != "1" || vtoken.is_empty() {
        return Err(AuthError::LoginUnavailable(format!(
            "vcode.do 未返回 token: code={} msg={}",
            payload.text("code"),
            payload.text("msg")
        )));
    }

    let image_path = format!(
        "{}?vtoken={}",
        ep.path("vcode_image"),
        httpc::quote(&vtoken, "/")
    );
    let resp = auth_request(ep, "GET", &image_path, None, None, timeout)
        .map_err(|e| AuthError::LoginUnavailable(format!("验证码图片请求失败: {e}")))?;
    let image = resp.body.clone();
    if resp.status != 200 || !image.starts_with(b"\xff\xd8\xff") {
        return Err(AuthError::LoginUnavailable(format!(
            "验证码图片异常: HTTP {} {}B",
            resp.status,
            image.len()
        )));
    }
    let cookie = set_cookie_values(&resp.set_cookies(), &ep.captcha_cookies);
    if cookie.is_empty() {
        return Err(AuthError::LoginUnavailable(
            "验证码响应里没有 验证码 Cookie".to_string(),
        ));
    }
    Ok(CaptchaChallenge {
        vtoken,
        cookie,
        image,
    })
}

// ==========================================================================
// 三、登录
// ==========================================================================

#[derive(Clone, Debug)]
pub struct LoginSession {
    pub token: String,
    /// 登录 Cookie + 本轮验证码 Cookie 拼起来的整串
    pub cookie: String,
    pub name: String,
    pub referer: String,
}

impl LoginSession {
    /// 之后所有业务接口要带的 Referer。
    ///
    /// 必须是**完整 URL**：实测只给相对路径会被服务端判
    /// 「Illegal refferer. 此页面不能跨域访问」，登录成功但每个接口都调不通。
    ///
    /// 路径默认取 `{base_path}/*default/grablessons.do`（与原版逐字一致，也就是
    /// 前端选课页那个路径）。个别学校如果不同，可以在 config.json 里用
    /// `school.referer_path` 覆盖。
    ///
    /// 前缀用 `ep.base_url` 而不是裸 `http://{host}`：非 80 端口时 Referer 必须带上
    /// 端口（原版这里写死了不带端口 —— 对 80 端口的学校没影响，但换个端口就不对了）。
    pub fn build_referer(ep: &Endpoints, token: &str) -> String {
        let path = ep
            .referer_path
            .clone()
            .unwrap_or_else(|| format!("{}/*default/grablessons.do", ep.base_path));
        format!("{}{}?token={}", ep.base_url, path, token)
    }
}

/// 一次登录会话的完整流程，带「验证码换图重试 / 密码错立刻收手」的策略。
pub struct Login<'a> {
    creds: &'a Credentials,
    captcha: &'a dyn Solver,
    ep: &'a Endpoints,
    captcha_attempts: i64,
    gap: f64,
    timeout: f64,
    login_pwd: String,
}

impl<'a> Login<'a> {
    pub fn new(
        creds: &'a Credentials,
        captcha: &'a dyn Solver,
        ep: &'a Endpoints,
        captcha_attempts: i64,
        gap: f64,
        timeout: f64,
    ) -> Login<'a> {
        Login {
            creds,
            captcha,
            ep,
            captcha_attempts: captcha_attempts.max(1),
            gap,
            timeout,
            login_pwd: encrypt_password(&creds.password, &ep.des_keys),
        }
    }

    /// 抓到能过的验证码并登录成功为止。密码错 → 返回 BadCredentials（不会重试）。
    pub fn login(&self) -> Result<LoginSession, AuthError> {
        let mut last: Option<AuthError> = None;
        for attempt in 1..=self.captcha_attempts {
            if attempt > 1 {
                std::thread::sleep(std::time::Duration::from_secs_f64(self.gap.max(0.0)));
            }
            let challenge = match fetch_captcha(self.ep, self.timeout) {
                Ok(c) => c,
                Err(e) => {
                    info(&format!(
                        "[auth]   取验证码失败（{attempt}/{}）: {}",
                        self.captcha_attempts,
                        e.message()
                    ));
                    std::thread::sleep(std::time::Duration::from_secs_f64(
                        (0.5 * attempt as f64).min(2.0),
                    ));
                    last = Some(e);
                    continue;
                }
            };
            let (points, margin) = match self.captcha.solve(&challenge.image) {
                Ok(v) => v,
                Err(Rejected(msg)) => {
                    info(&format!(
                        "[auth]   识别失败（{attempt}/{}）: {msg}",
                        self.captcha_attempts
                    ));
                    last = Some(AuthError::CaptchaRejected(msg));
                    continue;
                }
            };
            if margin < self.captcha.min_margin() {
                info(&format!(
                    "[auth]   识别把握不大 margin={margin:.3}，换一张图"
                ));
                last = Some(AuthError::CaptchaRejected(format!(
                    "置信度不足 margin={margin:.3}"
                )));
                continue;
            }
            let verify = crate::captcha::verify_code(&points);
            info(&format!(
                "[auth]   验证码 {verify}  margin={margin:.3}  （第 {attempt}/{} 张）",
                self.captcha_attempts
            ));
            match self.submit(&challenge, &verify) {
                Ok(session) => return Ok(session),
                Err(e @ AuthError::CaptchaRejected(_)) => {
                    info(&format!(
                        "[auth]   学校说验证码不对（{attempt}/{}），换一张",
                        self.captcha_attempts
                    ));
                    last = Some(e);
                }
                // 密码错 / 服务不可用 / 其它临时问题：立刻交给上层决定，不再换图
                Err(e) => return Err(e),
            }
        }
        match last {
            Some(e @ AuthError::LoginUnavailable(_)) => Err(AuthError::LoginUnavailable(format!(
                "连续 {} 次都没能取到可用验证码: {}",
                self.captcha_attempts,
                e.message()
            ))),
            Some(e) => Err(AuthError::CaptchaRejected(format!(
                "连续 {} 张验证码都没过: {}",
                self.captcha_attempts,
                e.message()
            ))),
            None => Err(AuthError::CaptchaRejected("验证码一张都没取到".to_string())),
        }
    }

    fn submit(
        &self,
        challenge: &CaptchaChallenge,
        verify: &str,
    ) -> Result<LoginSession, AuthError> {
        let form = httpc::urlencode(&[
            ("loginPwd", self.login_pwd.clone()),
            ("loginName", self.creds.student_id.clone()),
            ("vtoken", challenge.vtoken.clone()),
            ("verifyCode", verify.to_string()),
        ]);
        let resp = auth_request(
            self.ep,
            "POST",
            &self.ep.path("login"),
            Some(form.as_bytes()),
            Some(&challenge.cookie),
            self.timeout,
        )
        .map_err(|e| AuthError::LoginUnavailable(format!("login.do 请求失败: {e}")))?;

        let payload = resp.json();
        let code = payload.text("code");
        let msg = payload.text("msg").trim().to_string();
        let body_head: String = resp.text().chars().take(200).collect();
        let hay = format!("{msg} {body_head}");

        if code == "1" {
            let token = payload
                .object("data")
                .map(|d| d.text("token"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let login_cookie = set_cookie_values(&resp.set_cookies(), &self.ep.session_cookies);
            if token.is_empty() || login_cookie.is_empty() {
                return Err(AuthError::LoginUnavailable(format!(
                    "登录成功但响应缺少会话信息: token={} cookie={}",
                    !token.is_empty(),
                    !login_cookie.is_empty()
                )));
            }
            let cookie = [login_cookie, challenge.cookie.clone()]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("; ");
            let referer = LoginSession::build_referer(self.ep, &token);
            return Ok(LoginSession {
                token,
                cookie,
                name: payload
                    .object("data")
                    .map(|d| d.text("name"))
                    .unwrap_or_default(),
                referer,
            });
        }

        if code == "2" || WRONG_PASSWORD_HINTS.iter().any(|w| hay.contains(w)) {
            return Err(AuthError::BadCredentials(format!(
                "学号或密码不正确（HTTP {}）: {}",
                resp.status,
                if msg.is_empty() { short(&hay) } else { msg }
            )));
        }
        if code == "3" || hay.contains(WRONG_CAPTCHA) {
            return Err(AuthError::CaptchaRejected(if msg.is_empty() {
                WRONG_CAPTCHA.to_string()
            } else {
                msg
            }));
        }
        if hay.contains("认证失败") || code.starts_with('#') {
            return Err(AuthError::AuthServiceDown(format!(
                "认证服务暂时不可用（code={code}）: {}",
                if msg.is_empty() { short(&hay) } else { msg }
            )));
        }
        if code == "4" || ONLINE_LIMIT_HINTS.iter().any(|w| hay.contains(w)) {
            return Err(AuthError::LoginUnavailable(format!(
                "在线人数超过上限，稍后再试: {msg}"
            )));
        }
        Err(AuthError::LoginUnavailable(format!(
            "登录被拒（HTTP {} code={code}）: {}",
            resp.status,
            if msg.is_empty() { short(&hay) } else { msg }
        )))
    }
}

fn short(s: &str) -> String {
    s.chars().take(80).collect()
}

// ==========================================================================
// 四、带次数上限 / 冷却的自动重登录（给抢课主流程用）
// ==========================================================================

/// 管住"被踢了就自动登回来"这件事的次数与节奏。
///
/// 刻意保守：一次运行最多 `max_logins` 次自动登录，两次之间至少 `cooldown` 秒，
/// 一旦发现密码错（BadCredentials）就永久熔断 —— 后端连续失败可能触发风控/锁号。
pub struct ReloginManager {
    creds: Arc<Credentials>,
    captcha: Arc<dyn Solver>,
    ep: Arc<Endpoints>,
    max_logins: i64,
    cooldown: f64,
    captcha_attempts: i64,
    min_gap: f64,
    pub attempts: i64,
    pub last_at: f64,
    /// 一旦设置，之后所有 relogin 直接失败
    pub fatal: Option<String>,
}

impl ReloginManager {
    pub fn new(
        creds: Arc<Credentials>,
        captcha: Arc<dyn Solver>,
        ep: Arc<Endpoints>,
        max_logins: i64,
        cooldown: f64,
        captcha_attempts: i64,
        min_gap: f64,
    ) -> ReloginManager {
        ReloginManager {
            creds,
            captcha,
            ep,
            max_logins: max_logins.max(1),
            cooldown: cooldown.max(0.0),
            captcha_attempts,
            min_gap: min_gap.max(0.0),
            attempts: 0,
            // 用单调钟，所以"从没登录过"不能写 0.0（那等于"进程刚启动"，
            // 会让第一次登录白白等一个 min_gap）；负无穷就是"永远不用等"。
            last_at: f64::NEG_INFINITY,
            fatal: None,
        }
    }

    pub fn available(&self) -> bool {
        self.fatal.is_none() && self.attempts < self.max_logins
    }

    pub fn why_not(&self) -> String {
        if let Some(f) = &self.fatal {
            return f.clone();
        }
        if self.attempts >= self.max_logins {
            return format!(
                "本次运行已自动登录 {} 次，达到上限（--relogin-max）",
                self.attempts
            );
        }
        String::new()
    }

    /// 尽力登回来。成功返回新会话，失败返回 None（原因写在日志里）。
    pub fn relogin(&mut self, reason: &str) -> Option<LoginSession> {
        if !self.available() {
            info(&format!("[auth] 放弃自动重登录: {}", self.why_not()));
            return None;
        }
        let wait = self.min_gap - (timeutil::mono_now() - self.last_at);
        if wait > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(wait));
        }
        self.attempts += 1;
        self.last_at = timeutil::mono_now();
        let what = if reason == "启动" {
            "启动登录".to_string()
        } else {
            format!("第 {}/{} 次自动重登录", self.attempts, self.max_logins)
        };
        let tag = if reason.is_empty() || reason == "启动" {
            String::new()
        } else {
            format!("（{reason}）")
        };
        info(&format!("[auth] {what}{tag} …"));
        let t0 = timeutil::mono_now();

        let login = Login::new(
            &self.creds,
            self.captcha.as_ref(),
            &self.ep,
            self.captcha_attempts,
            0.4,
            15.0,
        );
        match login.login() {
            Ok(session) => {
                info(&format!(
                    "[auth] ✓ 登录成功（{:.1}s）{}",
                    timeutil::mono_now() - t0,
                    if session.name.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", session.name)
                    }
                ));
                if self.cooldown > 0.0 {
                    // 让新会话先落稳
                    std::thread::sleep(std::time::Duration::from_secs_f64(self.cooldown.min(1.0)));
                }
                Some(session)
            }
            Err(AuthError::BadCredentials(msg)) => {
                self.fatal = Some(format!("学号或密码不正确，已熔断自动重登录: {msg}"));
                info(&format!("[auth] ✗ {}", self.fatal.as_deref().unwrap_or("")));
                info("[auth]   请核对凭据文件；脚本不会再用错误密码重试（避免账号被锁）。");
                None
            }
            Err(AuthError::AuthServiceDown(msg)) => {
                // 服务端认证自己在重排数据 —— 不消耗次数配额，下一轮再试
                self.attempts -= 1;
                self.last_at = f64::NEG_INFINITY;
                info(&format!("[auth] ⚠ {msg}"));
                info("[auth]   这是服务端的问题（放课瞬间常见），不消耗重登录次数，稍后自动重试");
                None
            }
            Err(e) => {
                info(&format!(
                    "[auth] ✗ 自动重登录失败（{:.1}s）: {}",
                    timeutil::mono_now() - t0,
                    e.message()
                ));
                None
            }
        }
    }
}

/// 没有凭据时的提示文案。
pub fn no_credentials_note(default_path: &str) -> String {
    format!(
        "没有可用凭据。\n  写一个 {default_path}（chmod 600）：\n    {{\"student_id\": \"2026xxxxxx\", \"password\": \"你的密码\"}}\n  或临时用 --student XXX --password XXX 传一次。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_cookie_not_confused_by_expires_comma() {
        let got = set_cookie_values(
            &["route=a; Path=/, extra=b; Expires=Wed, 21 Oct 2026 07:28:00 GMT".to_string()],
            &["extra".to_string(), "route".to_string()],
        );
        assert_eq!(got, "extra=b; route=a");
    }

    #[test]
    fn set_cookie_multiple_headers_and_missing_names() {
        let got = set_cookie_values(
            &[
                "JSESSIONID=S1; Path=/".to_string(),
                "_WEU=W1; Path=/; HttpOnly".to_string(),
            ],
            &[
                "JSESSIONID".to_string(),
                "_WEU".to_string(),
                "nope".to_string(),
            ],
        );
        assert_eq!(got, "JSESSIONID=S1; _WEU=W1");
    }

    #[test]
    fn set_cookie_last_wins() {
        let got = set_cookie_values(
            &[
                "route=first; Path=/".to_string(),
                "route=second".to_string(),
            ],
            &["route".to_string()],
        );
        assert_eq!(got, "route=second");
    }

    #[test]
    fn set_cookie_ignores_path_and_expires() {
        // Path/Expires 会被扫到但不是我们要的名字，不该出现在结果里
        let got = set_cookie_values(
            &["insert_cookie=ic1; Path=/; Expires=Wed, 21 Oct 2026 07:28:00 GMT".to_string()],
            &["insert_cookie".to_string()],
        );
        assert_eq!(got, "insert_cookie=ic1");
    }

    #[test]
    fn credentials_aliases_and_wrapping() {
        let dir = std::env::temp_dir().join(format!("cg-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");

        std::fs::write(&path, r#"{"student_id":"2026000000","password":"s3cret"}"#).unwrap();
        let c = load_credentials(Some(path.to_str().unwrap()), None, None, "")
            .unwrap()
            .unwrap();
        assert_eq!(c.student_id, "2026000000");
        assert_eq!(c.password, "s3cret");
        assert_eq!(c.source, path.display().to_string());

        // 别名 + 少一层包装
        std::fs::write(
            &path,
            r#"{"credentials":{"studentId":2026000000,"pwd":"p"}}"#,
        )
        .unwrap();
        let c = load_credentials(Some(path.to_str().unwrap()), None, None, "")
            .unwrap()
            .unwrap();
        assert_eq!(c.student_id, "2026000000");

        // 缺密码要报错
        std::fs::write(&path, r#"{"student_id":"2026000000"}"#).unwrap();
        assert!(load_credentials(Some(path.to_str().unwrap()), None, None, "").is_err());

        // 学号不像学号也要报错
        std::fs::write(&path, r#"{"student_id":"abc","password":"p"}"#).unwrap();
        assert!(load_credentials(Some(path.to_str().unwrap()), None, None, "").is_err());

        // 没有文件 → None
        let missing = dir.join("nope.json");
        assert!(
            load_credentials(Some(missing.to_str().unwrap()), None, None, "")
                .unwrap()
                .is_none()
        );

        // 命令行覆盖文件
        let c = load_credentials(
            Some(path.to_str().unwrap()),
            Some("2026111111"),
            Some("cli"),
            "",
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.password, "cli");
        assert_eq!(c.source, "命令行");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credentials_debug_hides_password() {
        let c = Credentials {
            student_id: "2026000000".into(),
            password: "s3cret".into(),
            source: "test".into(),
        };
        let text = format!("{c:?}");
        assert!(!text.contains("s3cret"), "{text}");
        assert!(text.contains("***"));
    }

    #[test]
    fn partial_cli_credentials_are_errors() {
        assert!(load_credentials(None, None, Some("pwd"), "").is_err());
        assert!(load_credentials(None, Some("2026000000"), None, "").is_err());
    }

    #[test]
    fn masked_id() {
        assert_eq!(mask_id("2026000000"), "2026****00");
        assert_eq!(mask_id("12345"), "12345");
    }
}
