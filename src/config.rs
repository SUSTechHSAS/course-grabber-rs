//! 运行配置：一切"学校相关"的东西都从 config.json 读进来，代码里只留占位符。
//!
//! 这一层的作用和原版 `school_config.py` 完全一样：域名、接口路径、Cookie 名、
//! 密码加密密钥、候选教学班都带着某个学校的指纹，写死在代码里，这个仓库就不再是
//! "一套抢课工具"，而是"某个学校的抢课工具"。
//!
//! 读配置的顺序（先找到的先用）：
//!     1. 环境变量 COURSE_GRABBER_CONFIG 指向的文件
//!     2. 可执行文件旁边的 config.json
//!     3. 当前目录下的 config.json
//!     4. ~/.config/course-grabber/config.json
//!
//! 代码里只保留**与学校无关**的默认值：超时、限流节奏、并发形状这些工程参数。

use std::path::{Path, PathBuf};

use crate::json::{self, Json};

pub const ENV_VAR: &str = "COURSE_GRABBER_CONFIG";
pub const APP_DIR: &str = "~/.config/course-grabber";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 第一次运行时写给用户的配置模板（编进二进制，所以"只有那一个文件"真的够用）。
pub const CONFIG_EXAMPLE: &str = include_str!("../config.example.json");

/// 与学校无关的工程默认值。config.json 里没写的键就用这里的。
fn engine_defaults() -> Json {
    json::parse_or_empty(
        r#"{
          "timezone_offset_hours": 8,
          "http": {
            "user_agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
            "read_timeout": 15.0,
            "write_timeout": 4.0,
            "connect_timeout": 1.5
          },
          "pacing": {
            "per_window": 3,
            "window": 1.0,
            "margin": 0.15,
            "min_gap": 0.10,
            "overlap_wait": 0.35
          },
          "credentials_path": "~/.config/course-grabber/credentials.json",
          "captcha": { "width": 250, "height": 80, "min_margin": 0.0 },
          "course": { "keyword": "", "class_type": "", "campus": "", "candidates": [] }
        }"#,
    )
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// 可执行文件所在目录 —— 用户眼里的"程序旁边"。
///
/// 原版这里要区分"打包解包目录（_MEIPASS）"和"可执行文件目录"，因为 PyInstaller
/// 会把数据解到临时目录。Rust 版没有解包目录，一个文件就是全部，所以两者合一。
pub fn here() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 深合并：override 里有的键覆盖 base，两边都是对象就递归。
pub fn deep_merge(base: &Json, over: &Json) -> Json {
    match (base, over) {
        (Json::Obj(b), Json::Obj(o)) => {
            let mut out = b.clone();
            for (k, v) in o {
                match out.iter_mut().find(|(bk, _)| bk == k) {
                    Some((_, existing)) => *existing = deep_merge(existing, v),
                    None => out.push((k.clone(), v.clone())),
                }
            }
            Json::Obj(out)
        }
        _ => over.clone(),
    }
}

pub struct Config {
    pub raw: Json,
    pub source: String,
}

impl Config {
    /// 从一份原始 JSON 构造（补齐工程默认值、设定时区）。
    /// 单测里用它造"假学校"的配置，不必碰文件系统。
    pub fn from_raw(raw: Json, source: &str) -> Config {
        let merged = deep_merge(&engine_defaults(), &raw);
        let tz = merged
            .get("timezone_offset_hours")
            .and_then(|v| v.as_f64())
            .unwrap_or(8.0);
        crate::timeutil::set_offset_secs((tz * 3600.0).round() as i64);
        Config {
            raw: merged,
            source: source.to_string(),
        }
    }
}

/// 读配置。`path` 给了就只读它。
pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = path {
        candidates.push(p.to_path_buf());
    } else {
        if let Ok(env) = std::env::var(ENV_VAR) {
            if !env.is_empty() {
                candidates.push(PathBuf::from(env));
            }
        }
        candidates.push(here().join("config.json"));
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("config.json"));
        }
        candidates.push(expand_tilde(&format!("{APP_DIR}/config.json")));
    }

    for cand in &candidates {
        let p = expand_tilde(&cand.to_string_lossy());
        if !p.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&p)
            .map_err(|e| ConfigError(format!("配置文件读取失败 {}: {e}", p.display())))?;
        let raw = json::parse(&text).map_err(|_| {
            ConfigError(format!(
                "配置文件不是合法的 JSON: {}\n  提示: 用 `python3 -m json.tool {}` 看一眼哪里写错了",
                p.display(),
                p.display()
            ))
        })?;
        if !matches!(raw, Json::Obj(_)) {
            return Err(ConfigError(format!(
                "配置文件应当是一个 JSON 对象: {}",
                p.display()
            )));
        }
        return Ok(Config::from_raw(raw, &p.display().to_string()));
    }

    if let Some(p) = path {
        return Err(ConfigError(format!("找不到配置文件: {}", p.display())));
    }

    // 一条都没有：如果在程序旁边放一份模板，就替用户生成一份，并明确告诉他下一步。
    let target = here().join("config.json");
    let example = here().join("config.example.json");
    let example_text = if example.is_file() {
        std::fs::read_to_string(&example).unwrap_or_else(|_| CONFIG_EXAMPLE.to_string())
    } else {
        CONFIG_EXAMPLE.to_string()
    };
    if !target.exists() && std::fs::write(&target, example_text).is_ok() {
        return Err(ConfigError(format!(
            "第一次运行：已按模板生成配置文件\n    {}\n请填上你学校的域名、接口路径与候选教学班，然后重新运行。",
            target.display()
        )));
    }
    Err(ConfigError(format!(
        "找不到配置文件。先照着模板填一份：\n    cp {} {}\n    # 然后编辑它，填上你学校的域名与接口路径\n也可以放到 {APP_DIR}/config.json，或用环境变量 {ENV_VAR} 指定路径。",
        example.display(),
        target.display()
    )))
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

// ---------------------------------------------------------------------------
// 端点：把"学校相关"的常量拍平成一个不可变结构
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub label: String,
    pub group: String,
}

#[derive(Clone, Debug)]
pub struct Pacing {
    pub per_window: usize,
    pub window: f64,
    pub margin: f64,
    pub min_gap: f64,
    pub overlap_wait: f64,
}

/// 所有"跟这所学校有关"的运行期常量。
///
/// 做成普通结构体（而不是到处读全局配置）是为了能测：假学校服务端只要换掉
/// host/port 就能把整套真实逻辑跑起来，不需要改环境变量或全局状态。
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub host: String,
    pub port: u16,
    pub base_url: String,
    pub base_path: String,
    pub page_path: String,
    /// 业务接口 Referer 的路径。默认 `{base_path}/*default/grablessons.do`
    /// （与原版逐字一致）；个别学校不同的话用 config.json 的 school.referer_path 覆盖。
    pub referer_path: Option<String>,
    pub time_path: String,
    pub ua: String,
    pub paths: Json,
    pub session_cookies: Vec<String>,
    pub captcha_cookies: Vec<String>,
    pub des_keys: Vec<String>,
    pub class_type: String,
    pub is_major: String,
    pub query_content: String,
    pub keyword: String,
    pub candidates: Vec<Candidate>,
    pub captcha_width: i64,
    pub captcha_height: i64,
    pub captcha_min_margin: f64,
    pub captcha_model_file: Option<String>,
    pub read_timeout: f64,
    pub write_timeout: f64,
    pub connect_timeout: f64,
    pub pacing: Pacing,
    pub credentials_path: String,
}

fn str_list(v: Option<&Json>, default: &[&str]) -> Vec<String> {
    match v {
        Some(Json::Arr(items)) => items
            .iter()
            .filter_map(|x| x.as_str())
            .map(|s| s.to_string())
            .collect(),
        Some(Json::Str(s)) if !s.is_empty() => vec![s.clone()],
        _ => default.iter().map(|s| s.to_string()).collect(),
    }
}

/// 取一个字符串配置项。
///
/// **空串按"没写"处理** —— 对齐 Python 那边到处写的 `CFG.x.get("k") or 默认值`
/// （空串在那里是 falsy）。不这么做的话，`"time_path": ""` 会变成 `GET ?_=123`
/// 这种没意义的请求，而 config.example.json 的注释恰好告诉用户
/// "time_path 留空则用站点根路径 /"。
fn text_of(v: Option<&Json>, default: &str) -> String {
    match v {
        Some(Json::Str(s)) if !s.is_empty() => s.clone(),
        Some(Json::Str(_)) => default.to_string(),
        Some(other) => {
            let t = other.as_text();
            if t.is_empty() {
                default.to_string()
            } else {
                t
            }
        }
        None => default.to_string(),
    }
}

/// 列表配置项，且**空列表也按"没写"处理**（同样对齐 Python 的 `or 默认值`）。
fn str_list_or(v: Option<&Json>, default: &[&str]) -> Vec<String> {
    let got = str_list(v, &[]);
    if got.is_empty() {
        default.iter().map(|s| s.to_string()).collect()
    } else {
        got
    }
}

impl Config {
    /// 从配置里取出端点常量。缺了关键项就报错（和原版一样在启动阶段就拦下来）。
    pub fn endpoints(&self) -> Result<Endpoints, ConfigError> {
        let school = self.raw.object("school").cloned().unwrap_or(Json::Null);
        let kind = |k: &str| school.get(k);
        let host = kind("host").map(|v| v.as_text()).unwrap_or_default();
        if host.trim().is_empty() {
            return Err(ConfigError(format!(
                "配置不完整（缺 school.host）: {}\n  照 config.example.json 填一份 config.json 再跑。",
                self.source
            )));
        }
        let port_raw = kind("port").and_then(|v| v.as_i64()).unwrap_or(80);
        let port = if port_raw <= 0 { 80 } else { port_raw as u16 };
        let base_url = if port == 80 {
            format!("http://{host}")
        } else {
            format!("http://{host}:{port}")
        };
        let paths = self.raw.object("paths").cloned().unwrap_or(Json::Null);
        if !matches!(paths, Json::Obj(ref p) if !p.is_empty()) {
            return Err(ConfigError(format!(
                "配置不完整（缺 paths）: {}\n  照 config.example.json 填一份 config.json 再跑。",
                self.source
            )));
        }
        // 这些路径全都会用到，缺一个就是配置抄漏了。原版是在 import 期
        // `CFG.path("volunteer")` 直接抛 ConfigError 退出，这里同样在启动阶段拦下来 ——
        // 否则会退化成往一个不存在的 URL 发请求，只看到一个看不懂的 404。
        let missing: Vec<&str> = [
            "volunteer",
            "capacity",
            "result",
            "status",
            "sysparam",
            "program",
            "student",
            "vcode_token",
            "vcode_image",
            "login",
        ]
        .into_iter()
        .filter(|k| paths.text(k).is_empty())
        .collect();
        if !missing.is_empty() {
            return Err(ConfigError(format!(
                "配置里缺少这些接口路径 paths.*: {}\n  照 config.example.json 填一份 config.json 再跑。",
                missing.join(", ")
            )));
        }
        let cookies = self.raw.object("cookies").cloned().unwrap_or(Json::Null);
        let password = self.raw.object("password").cloned().unwrap_or(Json::Null);
        let des_keys: Vec<String> = match password.get("des_keys") {
            Some(Json::Arr(items)) if !items.is_empty() => {
                items.iter().map(|x| x.as_text()).collect()
            }
            _ => {
                return Err(ConfigError(format!(
                    "配置里缺少 password.des_keys（密码加密用的密钥）: {}",
                    self.source
                )))
            }
        };
        // 前端协议就是"最多 3 组密钥"（str_enc(pwd, key1, key2, key3)）。
        // 原版遇到 4 组会 TypeError、遇到空串密钥会 NameError —— 都是无效配置，
        // 这里在启动阶段就说清楚，别等到登录时给一个看不懂的错误。
        if des_keys.len() > 3 {
            return Err(ConfigError(format!(
                "password.des_keys 最多 3 组（前端协议就是 3 组），现在有 {} 组: {}",
                des_keys.len(),
                self.source
            )));
        }
        if let Some(pos) = des_keys.iter().position(|k| k.is_empty()) {
            return Err(ConfigError(format!(
                "password.des_keys 第 {} 组是空串: {}",
                pos + 1,
                self.source
            )));
        }

        let captcha = self.raw.object("captcha").cloned().unwrap_or(Json::Null);
        let http = self.raw.object("http").cloned().unwrap_or(Json::Null);
        let pacing_raw = self.raw.object("pacing").cloned().unwrap_or(Json::Null);
        let course = self.raw.object("course").cloned().unwrap_or(Json::Null);

        let base_path = text_of(kind("base_path"), "");
        let base_path = base_path.trim_end_matches('/').to_string();

        let mut candidates = Vec::new();
        for item in course.array("candidates") {
            let (id, label, group) = match item {
                Json::Obj(_) => (item.text("id"), item.text("label"), item.text("group")),
                Json::Arr(parts) => (
                    parts.first().map(|v| v.as_text()).unwrap_or_default(),
                    parts.get(1).map(|v| v.as_text()).unwrap_or_default(),
                    parts.get(2).map(|v| v.as_text()).unwrap_or_default(),
                ),
                _ => continue,
            };
            if !id.is_empty() {
                candidates.push(Candidate { id, label, group });
            }
        }

        let page_path = text_of(kind("page_path"), "/");
        Ok(Endpoints {
            host: host.clone(),
            port,
            base_url,
            base_path,
            page_path: page_path.clone(),
            referer_path: kind("referer_path")
                .map(|v| v.as_text())
                .filter(|s| !s.is_empty()),
            time_path: text_of(kind("time_path"), "/"),
            ua: text_of(http.get("user_agent"), ""),
            paths,
            // 验证码那两个名字在原版里是**写死**的（配置里那个键其实不生效），
            // 所以缺键/空数组时必须回落到同一套默认值，否则会一条 Cookie 都取不到、
            // 登录直接瘫痪。
            session_cookies: str_list_or(cookies.get("session"), &["JSESSIONID"]),
            captcha_cookies: str_list_or(cookies.get("captcha"), &["route", "insert_cookie"]),
            des_keys,
            class_type: text_of(course.get("class_type"), ""),
            is_major: text_of(course.get("is_major"), "1"),
            query_content: text_of(course.get("query_content"), "{keyword}"),
            keyword: text_of(course.get("keyword"), ""),
            candidates,
            captcha_width: captcha.get("width").and_then(|v| v.as_i64()).unwrap_or(250),
            captcha_height: captcha.get("height").and_then(|v| v.as_i64()).unwrap_or(80),
            captcha_min_margin: captcha
                .get("min_margin")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            captcha_model_file: captcha
                .get("model_file")
                .map(|v| v.as_text())
                .filter(|s| !s.is_empty()),
            read_timeout: http
                .get("read_timeout")
                .and_then(|v| v.as_f64())
                .unwrap_or(15.0),
            write_timeout: http
                .get("write_timeout")
                .and_then(|v| v.as_f64())
                .unwrap_or(4.0),
            connect_timeout: http
                .get("connect_timeout")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.5),
            pacing: Pacing {
                per_window: pacing_raw
                    .get("per_window")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(3)
                    .max(1) as usize,
                window: pacing_raw
                    .get("window")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0),
                margin: pacing_raw
                    .get("margin")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.15),
                min_gap: pacing_raw
                    .get("min_gap")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.10),
                overlap_wait: pacing_raw
                    .get("overlap_wait")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.35),
            },
            credentials_path: text_of(
                self.raw.get("credentials_path"),
                &format!("{APP_DIR}/credentials.json"),
            ),
        })
    }
}

impl Endpoints {
    /// `Host` 请求头：非 80 端口时要带上端口（照 `http.client` 的行为）。
    ///
    /// 例外是首发那包手写报文（`build_wire`）：原版那里就是裸 host，保持一致。
    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// 取接口路径模板（`{base}` 已替换）。
    pub fn path(&self, name: &str) -> String {
        let raw = self.paths.text(name);
        if raw.is_empty() {
            return format!("/{name}");
        }
        raw.replace("{base}", &self.base_path)
    }

    /// 取接口路径并把 `{code}` 填成学号。
    pub fn path_code(&self, name: &str, code: &str) -> String {
        self.path(name).replace("{code}", code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_merge_recurses() {
        let base = json::parse(r#"{"a":{"b":1,"c":2},"d":3}"#).unwrap();
        let over = json::parse(r#"{"a":{"c":9},"e":5}"#).unwrap();
        let got = deep_merge(&base, &over);
        assert_eq!(got.get("a").unwrap().get("b").unwrap().as_i64(), Some(1));
        assert_eq!(got.get("a").unwrap().get("c").unwrap().as_i64(), Some(9));
        assert_eq!(got.get("d").unwrap().as_i64(), Some(3));
        assert_eq!(got.get("e").unwrap().as_i64(), Some(5));
    }

    #[test]
    fn example_config_parses_and_has_expected_keys() {
        let raw = json::parse(CONFIG_EXAMPLE).unwrap();
        assert!(raw.object("school").is_some());
        assert!(raw.object("paths").is_some());
        assert_eq!(
            raw.object("password").unwrap().text("des_keys"),
            "" // des_keys 是数组，text() 给空串（这里只是确认取得到对象）
        );
        assert_eq!(raw.object("password").unwrap().array("des_keys").len(), 3);
    }

    /// 缺键/空数组时的默认值必须与 Python 一致 —— 这里每一条都对应一个"会真的
    /// 把登录打死"的坑（原版那几个键是写死的，配置里写不写都不影响）。
    #[test]
    fn cookie_and_path_defaults_match_python() {
        let ep = |json_text: &str| -> Result<Endpoints, ConfigError> {
            Config::from_raw(json::parse(json_text).unwrap(), "test").endpoints()
        };
        let base = |extra: &str| {
            format!(
                r#"{{"school":{{"host":"h"}},"paths":{{"volunteer":"/v","capacity":"/c","result":"/r",
                   "status":"/s","sysparam":"/y","program":"/p","student":"/st",
                   "vcode_token":"/vt","vcode_image":"/vi","login":"/l"}},
                   "password":{{"des_keys":["a","b","c"]}}{extra}}}"#
            )
        };

        // ① 完全没有 cookies 段 → 验证码那两个名字要回落到原版写死的那套
        let e = ep(&base("")).unwrap();
        assert_eq!(e.captcha_cookies, vec!["route", "insert_cookie"]);
        assert_eq!(e.session_cookies, vec!["JSESSIONID"]);
        assert_eq!(e.time_path, "/");
        assert_eq!(e.page_path, "/");

        // ② 显式空数组也算"没写"（Python 那边 `[]` 是 falsy）
        let e = ep(&base(r#","cookies":{"session":[],"captcha":[]}"#)).unwrap();
        assert_eq!(e.captcha_cookies, vec!["route", "insert_cookie"]);
        assert_eq!(e.session_cookies, vec!["JSESSIONID"]);

        // ③ 显式空串也算"没写"（config.example.json 就写着"time_path 留空则用 /"）
        let e = ep(r#"{"school":{"host":"h","time_path":"","page_path":""},
                     "paths":{"volunteer":"/v","capacity":"/c","result":"/r","status":"/s",
                              "sysparam":"/y","program":"/p","student":"/st",
                              "vcode_token":"/vt","vcode_image":"/vi","login":"/l"},
                     "password":{"des_keys":["a","b","c"]},
                     "course":{"is_major":"","query_content":""}}"#)
        .unwrap();
        assert_eq!(e.time_path, "/");
        assert_eq!(e.page_path, "/");
        assert_eq!(e.is_major, "1");
        assert_eq!(e.query_content, "{keyword}");
    }

    #[test]
    fn rejects_incomplete_or_invalid_config() {
        const FULL_PATHS: &str = r#""volunteer":"/v","capacity":"/c","result":"/r",
             "status":"/s","sysparam":"/y","program":"/p","student":"/st",
             "vcode_token":"/vt","vcode_image":"/vi","login":"/l""#;
        let with =
            |json_text: &str| Config::from_raw(json::parse(json_text).unwrap(), "test").endpoints();

        // 缺一个接口路径（原版在 import 期就会因为缺键报错退出 2）
        let missing_sysparam = r#"{"school":{"host":"h"},
             "paths":{"volunteer":"/v","capacity":"/c","result":"/r","status":"/s",
                      "program":"/p","student":"/st","vcode_token":"/vt",
                      "vcode_image":"/vi","login":"/l"},
             "password":{"des_keys":["a"]}}"#;
        match with(missing_sysparam) {
            Err(e) => assert!(e.0.contains("sysparam"), "{}", e.0),
            Ok(_) => panic!("缺 paths.sysparam 应当报错"),
        }

        // 缺 des_keys
        let no_keys = format!(r#"{{"school":{{"host":"h"}},"paths":{{{FULL_PATHS}}}}}"#);
        assert!(with(&no_keys).is_err());

        // 4 组密钥（前端协议只有 3 组；原版会 TypeError）
        let four = format!(
            r#"{{"school":{{"host":"h"}},"paths":{{{FULL_PATHS}}},
                 "password":{{"des_keys":["a","b","c","d"]}}}}"#
        );
        match with(&four) {
            Err(e) => assert!(e.0.contains("最多 3 组"), "{}", e.0),
            Ok(_) => panic!("4 组密钥应当报错"),
        }

        // 空串密钥（原版会 NameError）
        let empty_key = format!(
            r#"{{"school":{{"host":"h"}},"paths":{{{FULL_PATHS}}},
                 "password":{{"des_keys":["","b"]}}}}"#
        );
        match with(&empty_key) {
            Err(e) => assert!(e.0.contains("空串"), "{}", e.0),
            Ok(_) => panic!("空串密钥应当报错"),
        }
    }

    #[test]
    fn tilde_expands() {
        std::env::set_var("HOME", "/tmp/fakehome");
        assert_eq!(expand_tilde("~/x/y"), PathBuf::from("/tmp/fakehome/x/y"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
