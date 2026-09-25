//! 命令行参数。
//!
//! 手写而不是拉 clap：参数一共二十来个，都是"取值/开关"两种形态，clap 这类库
//! 的好处在这么窄的面上体现不出来，而它的体积要算进"下载即用"的那一个文件里。
//!
//! 行为对齐 argparse 的地方：`--flag value` 与 `--flag=value` 都认；参数不认识或
//! 取值不合法时打印用法并以 2 退出；`--help` / `--version` 在任何时候都能用
//! （包括还没写配置文件的时候）。

use crate::config::VERSION;

#[derive(Debug, Clone)]
pub struct Args {
    pub url: Option<String>,
    pub live: bool,
    pub at: String,
    pub now: bool,
    pub tomorrow: bool,
    pub early_ms: f64,
    pub student: Option<String>,
    pub cookie: Option<String>,
    pub credentials: Option<String>,
    pub password: Option<String>,
    pub no_relogin: bool,
    pub relogin_max: i64,
    pub relogin_gap: f64,
    pub captcha_model: Option<String>,
    pub captcha_min_margin: f64,
    pub captcha_attempts: i64,
    pub priority: Option<String>,
    pub single: bool,
    pub write_timeout: f64,
    pub stagger: f64,
    pub conns: i64,
    pub window: f64,
    pub burst: f64,
    pub interval: f64,
    pub slow: f64,
    pub switch_after: f64,
    pub keyword: Option<String>,
    pub force: bool,
    pub offline: bool,
    /// 解析期间攒下的提示（比如用了已废弃的参数），启动横幅打完之后再打出来
    pub notes: Vec<String>,
}

/// `--stagger` / `--write-timeout` 的"默认值哨兵"。
///
/// 这两个参数的默认值在 config.json 里也有一份（`pacing.min_gap` / `http.write_timeout`），
/// 但原版 Python 里那两个键其实是**死的**（代码只用命令行默认值）。这里让它们活过来：
/// 命令行取值仍然等于本文件的默认值时，就认为"用户没指定"，改用配置里的值。
/// 也就是说显式传一个**恰好等于默认值**的数会被配置覆盖 —— 这是本改写里唯一的行为差异，
/// 而默认值两边一致，所以对现有配置完全无感。
pub const DEFAULT_STAGGER: f64 = 0.10;
pub const DEFAULT_WRITE_TIMEOUT: f64 = 4.0;

pub enum Parsed {
    Run(Box<Args>),
    /// 直接打印然后退出 0（--help / --version）
    Print(String),
}

impl Default for Args {
    fn default() -> Args {
        Args {
            url: None,
            live: false,
            at: "20:00:00".to_string(),
            now: false,
            tomorrow: false,
            early_ms: 0.0,
            student: None,
            cookie: None,
            credentials: None,
            password: None,
            no_relogin: false,
            relogin_max: 4,
            relogin_gap: 2.0,
            captcha_model: None,
            captcha_min_margin: 0.0,
            captcha_attempts: 6,
            priority: None,
            single: false,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            stagger: DEFAULT_STAGGER,
            conns: 1,
            window: 90.0,
            burst: 5.0,
            interval: 1.0,
            slow: 1.5,
            switch_after: 60.0,
            keyword: None,
            force: false,
            offline: false,
            notes: Vec::new(),
        }
    }
}

const USAGE: &str = "\
用法: course-grabber [选项]

教务系统抢课 · 放课窗口精准首发（默认只预检，加 --live 才提交）";

fn help() -> String {
    format!(
        "{USAGE}

必读
  --live                真正提交选课请求。不加此参数只做只读预检与演练。
  --at HH:MM:SS         首发时刻（配置时区，默认北京时间），默认 20:00:00
  --now                 跳过等待，立刻开始打
  --tomorrow            --at 已过时等到明天；默认是「已过就立刻开打」（放课后才有漏可捡）
  --early-ms MS         正数=提前、负数=推后多少毫秒出手。默认 0：瞄准正点，
                        因为放课是一个瞬间，早打等于白扔一发

会话
  --url URL             浏览器地址栏里带 token 的完整业务页 URL。
                        有凭据文件（自动登录）时可以不给 —— 登录后会自己拿到 token
  --cookie COOKIE       直接给 Cookie 字符串（没有凭据时的兜底）

自动登录（学号+密码 → 会话，验证码用内编的识别模型）
  --credentials 文件    凭据文件路径，默认 ~/.config/course-grabber/credentials.json
  --password 密码       临时给一次密码（会出现在 ps / shell 历史里，日常请用凭据文件）
  --student 学号        学号（默认取凭据文件里的）
  --no-relogin          关掉整条自动登录链路，只用 --cookie / --url 给的会话
  --relogin-max N       一次运行最多自动重登录几次，默认 4（达到后停下来喊人）
  --relogin-gap S       两次自动登录之间的最小间隔秒数，默认 2
  --captcha-attempts N  一次登录最多换几张验证码，默认 6
  --captcha-min-margin F 识别置信度低于此值就换一张图而不是提交，默认 0（模型 99%+ 够稳）
  --captcha-model 文件  指定识别模型（.ccm）；默认用编在二进制里的 w16

候选与节奏
  --priority ID,ID      候选教学班 ID，逗号分隔，按志愿优先级；默认取 config.json
                        允许只写 ID 后缀（如 --priority 308）
  --single              首发只打第一优先级那一个班（最保守，命中率也最低）
  --stagger S           首发多班之间错开多少秒，默认 0.10。实测 0.03s 会出现空回复、
                        0.06s 以上才稳定拿到真实业务回复
  --conns N             首发宽度（= 预热连接数），默认 1；传 2~3 等同加宽首发，硬上限 3
  --write-timeout S     单发写请求最多占用几秒，默认 4。放课瞬间服务器过载时，
                        挂住的请求只会拖死它自己，不会吃掉整个窗口
  --window S            放课后持续尝试的秒数，默认 90
  --burst S             放课后高强度的秒数（组内按优先级整轮轮询），默认 5
  --interval S          爆发期每轮周期（秒），默认 1.0。每轮最多连发 3 发
  --slow S              爆发期之后每轮间隔秒数，默认 1.5
  --switch-after S      第一组一直没名额时，多少秒后换到下一冲突组（0=不换），默认 60
  --keyword 课程名      目标课程名（用于建白名单）；默认取 config.json 里的 course.keyword

其它
  --offline             完全不联网，只打印将要发送的内容
  --force               忽略单实例锁（同一账号同时只能有一个会话，跑两个会互相踢）
  -h, --help            显示这段帮助
  --version             显示版本号
"
    )
}

fn missing(name: &str) -> String {
    format!("{USAGE}\n\ncourse-grabber: error: argument {name}: 缺少取值\n")
}

/// 取一个参数的值：`--flag value` 或 `--flag=value`。
fn value_of(
    items: &[String],
    i: &mut usize,
    inline: &Option<String>,
    name: &str,
) -> Result<String, String> {
    match inline {
        Some(v) => Ok(v.clone()),
        None => {
            if *i + 1 >= items.len() {
                return Err(missing(name));
            }
            *i += 1;
            Ok(items[*i].clone())
        }
    }
}

fn parse_float(name: &str, v: &str) -> Result<f64, String> {
    v.trim().parse::<f64>().map_err(|_| {
        format!("{USAGE}\n\ncourse-grabber: error: argument {name}: 不是合法的数字: '{v}'\n")
    })
}

fn parse_int(name: &str, v: &str) -> Result<i64, String> {
    v.trim().parse::<i64>().map_err(|_| {
        format!("{USAGE}\n\ncourse-grabber: error: argument {name}: 不是合法的整数: '{v}'\n")
    })
}

const NO_CHROME_NOTE: &str =
    "已经没有读 Chrome Cookie 这条路径了（会话来自凭据文件自动登录），这个参数现在是无操作";

/// 解析命令行。返回值里 `Err` 是要打到 stderr 的完整消息（含用法）。
pub fn parse<I: IntoIterator<Item = String>>(argv: I) -> Result<Parsed, String> {
    let mut args = Args::default();
    let items: Vec<String> = argv.into_iter().collect();
    let mut i = 0usize;

    while i < items.len() {
        let raw = items[i].clone();
        // 支持 --flag=value
        let (flag, inline) = match raw.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (raw.clone(), None),
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Print(help())),
            "--version" => return Ok(Parsed::Print(format!("course-grabber {VERSION}\n"))),
            "--live" => args.live = true,
            "--now" => args.now = true,
            "--tomorrow" => args.tomorrow = true,
            "--single" => args.single = true,
            "--force" => args.force = true,
            "--offline" => args.offline = true,
            "--no-relogin" => args.no_relogin = true,
            "--hedge" => args.notes.push(
                "--hedge：首发覆盖组内前 3 个候选**已经是默认行为**，这个参数只为兼容保留"
                    .to_string(),
            ),
            "--no-chrome" => args.notes.push(format!("--no-chrome：{NO_CHROME_NOTE}")),
            "--profile" => {
                let _ = value_of(&items, &mut i, &inline, "--profile")?;
                args.notes.push(format!("--profile：{NO_CHROME_NOTE}"));
            }
            "--captcha-model-dir" => {
                let _ = value_of(&items, &mut i, &inline, "--captcha-model-dir")?;
                args.notes.push(
                    "--captcha-model-dir：识别库现在是编译进来的依赖（模型也编在里面），不再需要指向目录；要用别的模型请用 --captcha-model 指 .ccm 文件"
                        .to_string(),
                );
            }
            "--url" => args.url = Some(value_of(&items, &mut i, &inline, "--url")?),
            "--at" => args.at = value_of(&items, &mut i, &inline, "--at")?,
            "--student" => args.student = Some(value_of(&items, &mut i, &inline, "--student")?),
            "--cookie" => args.cookie = Some(value_of(&items, &mut i, &inline, "--cookie")?),
            "--credentials" => {
                args.credentials = Some(value_of(&items, &mut i, &inline, "--credentials")?)
            }
            "--password" => args.password = Some(value_of(&items, &mut i, &inline, "--password")?),
            "--priority" => args.priority = Some(value_of(&items, &mut i, &inline, "--priority")?),
            "--keyword" => args.keyword = Some(value_of(&items, &mut i, &inline, "--keyword")?),
            "--captcha-model" => {
                args.captcha_model = Some(value_of(&items, &mut i, &inline, "--captcha-model")?)
            }
            "--early-ms" => {
                let v = value_of(&items, &mut i, &inline, "--early-ms")?;
                args.early_ms = parse_float("--early-ms", &v)?;
            }
            "--write-timeout" => {
                let v = value_of(&items, &mut i, &inline, "--write-timeout")?;
                args.write_timeout = parse_float("--write-timeout", &v)?;
            }
            "--stagger" => {
                let v = value_of(&items, &mut i, &inline, "--stagger")?;
                args.stagger = parse_float("--stagger", &v)?;
            }
            "--window" => {
                let v = value_of(&items, &mut i, &inline, "--window")?;
                args.window = parse_float("--window", &v)?;
            }
            "--burst" => {
                let v = value_of(&items, &mut i, &inline, "--burst")?;
                args.burst = parse_float("--burst", &v)?;
            }
            "--interval" => {
                let v = value_of(&items, &mut i, &inline, "--interval")?;
                args.interval = parse_float("--interval", &v)?;
            }
            "--slow" => {
                let v = value_of(&items, &mut i, &inline, "--slow")?;
                args.slow = parse_float("--slow", &v)?;
            }
            "--switch-after" => {
                let v = value_of(&items, &mut i, &inline, "--switch-after")?;
                args.switch_after = parse_float("--switch-after", &v)?;
            }
            "--relogin-gap" => {
                let v = value_of(&items, &mut i, &inline, "--relogin-gap")?;
                args.relogin_gap = parse_float("--relogin-gap", &v)?;
            }
            "--captcha-min-margin" => {
                let v = value_of(&items, &mut i, &inline, "--captcha-min-margin")?;
                args.captcha_min_margin = parse_float("--captcha-min-margin", &v)?;
            }
            "--conns" => {
                let v = value_of(&items, &mut i, &inline, "--conns")?;
                args.conns = parse_int("--conns", &v)?;
            }
            "--relogin-max" => {
                let v = value_of(&items, &mut i, &inline, "--relogin-max")?;
                args.relogin_max = parse_int("--relogin-max", &v)?;
            }
            "--captcha-attempts" => {
                let v = value_of(&items, &mut i, &inline, "--captcha-attempts")?;
                args.captcha_attempts = parse_int("--captcha-attempts", &v)?;
            }
            other => {
                let hint = if other.starts_with('-') {
                    "未知参数"
                } else {
                    "不认识这个位置参数"
                };
                return Err(format!(
                    "{USAGE}\n\ncourse-grabber: error: {hint}: {other}\n用 --help 看全部参数。\n"
                ));
            }
        }
        i += 1;
    }
    Ok(Parsed::Run(Box::new(args)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(argv: &[&str]) -> Args {
        match parse(argv.iter().map(|s| s.to_string())).unwrap() {
            Parsed::Run(a) => *a,
            Parsed::Print(_) => panic!("期望是运行参数"),
        }
    }

    #[test]
    fn defaults_match_python() {
        let a = parse_ok(&[]);
        assert_eq!(a.at, "20:00:00");
        assert_eq!(a.window, 90.0);
        assert_eq!(a.burst, 5.0);
        assert_eq!(a.interval, 1.0);
        assert_eq!(a.slow, 1.5);
        assert_eq!(a.switch_after, 60.0);
        assert_eq!(a.stagger, 0.10);
        assert_eq!(a.write_timeout, 4.0);
        assert_eq!(a.relogin_max, 4);
        assert_eq!(a.relogin_gap, 2.0);
        assert_eq!(a.captcha_attempts, 6);
        assert_eq!(a.conns, 1);
        assert!(!a.live && !a.now && !a.tomorrow && !a.offline);
    }

    #[test]
    fn both_value_syntaxes() {
        assert_eq!(parse_ok(&["--at", "19:59:59"]).at, "19:59:59");
        assert_eq!(parse_ok(&["--at=19:59:59"]).at, "19:59:59");
        assert_eq!(parse_ok(&["--window", "600"]).window, 600.0);
        assert_eq!(parse_ok(&["--window=600"]).window, 600.0);
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(
            parse(["--help".to_string()]).unwrap(),
            Parsed::Print(_)
        ));
        assert!(matches!(
            parse(["-h".to_string()]).unwrap(),
            Parsed::Print(_)
        ));
        match parse(["--version".to_string()]).unwrap() {
            Parsed::Print(t) => assert_eq!(t.trim(), format!("course-grabber {VERSION}")),
            Parsed::Run(_) => panic!(),
        }
    }

    #[test]
    fn rejects_unknown_and_bad_values() {
        assert!(parse(["--nope".to_string()]).is_err());
        assert!(parse(["--window".to_string(), "abc".to_string()]).is_err());
        assert!(parse(["--window".to_string()]).is_err()); // 缺取值
        assert!(parse(["--conns".to_string(), "x".to_string()]).is_err());
    }

    #[test]
    fn dropped_flags_become_notes() {
        let a = parse_ok(&[
            "--no-chrome",
            "--captcha-model-dir",
            "/tmp/x",
            "--profile",
            "/tmp/p",
        ]);
        assert_eq!(a.notes.len(), 3);
        assert!(a.notes[0].contains("--no-chrome"));
        assert!(a.notes[1].contains("--captcha-model-dir"));
        assert!(a.notes[2].contains("--profile"));
    }

    #[test]
    fn boolean_flags_do_not_eat_the_next_arg() {
        let a = parse_ok(&["--live", "--now", "--single", "--force", "--at", "20:00:01"]);
        assert!(a.live && a.now && a.single && a.force);
        assert_eq!(a.at, "20:00:01");
    }
}
