//! 端到端冒烟：**直接跑编译出来的那个可执行文件**，让它去打假学校服务端。
//!
//! 上面那些测试（tests/offline.rs）覆盖的是模块级逻辑；这里覆盖的是 `main()` 里
//! 那段"接线"：读配置 → 解析参数 → 建会话 → 五项预检 → 首发 → 复核 → 收尾报告。
//! 原版 CI 里也有一个同类任务（"打包冒烟"），只是它只能跑 `--offline`。

mod mock;

use std::io::Write;
use std::process::Command;

use course_grabber::json;
use mock::{Mock, Mode};

const TEST_CONFIG: &str = include_str!("config.test.json");

/// 在临时目录里写一份指向假学校服务端的 config.json，返回目录。
fn stage_config(port: u16, tag: &str) -> std::path::PathBuf {
    let mut raw = json::parse(TEST_CONFIG).unwrap();
    // 改 host/port：把 school 那段里的两个键换掉
    if let Some(school) = raw.get("school") {
        let mut pairs: Vec<(String, json::Json)> = match school {
            json::Json::Obj(p) => p.clone(),
            _ => Vec::new(),
        };
        for (k, v) in pairs.iter_mut() {
            match k.as_str() {
                "host" => *v = json::Json::str("127.0.0.1"),
                "port" => *v = json::Json::Int(port as i64),
                "base_path" => *v = json::Json::str("/api"),
                "page_path" => *v = json::Json::str("/api/*default/index.do"),
                _ => {}
            }
        }
        raw = replace_key(&raw, "school", json::Json::Obj(pairs));
    }
    // 候选只留一个，日志短一点
    let dir = std::env::temp_dir().join(format!("cg-cli-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("config.json")).unwrap();
    f.write_all(raw.to_compact().as_bytes()).unwrap();
    dir
}

fn replace_key(obj: &json::Json, key: &str, value: json::Json) -> json::Json {
    match obj {
        json::Json::Obj(pairs) => json::Json::Obj(
            pairs
                .iter()
                .map(|(k, v)| {
                    if k == key {
                        (k.clone(), value.clone())
                    } else {
                        (k.clone(), v.clone())
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn run_binary(dir: &std::path::Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_course-grabber"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("跑得起来");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// 指向假学校那个选课页的 --url（token 从这里解析出来）。
fn url_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}/api/*default/grablessons.do?token=testtoken")
}

#[test]
fn cli_preflight_is_read_only() {
    let mock = Mock::start(Mode::Normal, vec![]);
    let dir = stage_config(mock.port, "preflight");
    let url = url_for(mock.port);

    // --at 00:00:00 一定是"已过点"，所以预检会跳过对时/目录/容量，直接进入倒计时；
    // 不加 --live，它到点也不会提交（只做一次 3 秒内的计时演练）。
    let (code, out) = run_binary(
        &dir,
        &[
            "--url",
            &url,
            "--cookie",
            "JSESSIONID=S1",
            "--no-relogin",
            "--student",
            "2026000000",
            // 锁文件放在可执行文件旁边（target/debug/），两个测试并行跑会互相挡住；
            // 这里用 --force 跳过单实例锁 —— 正好也把那个开关覆盖上。
            "--force",
            "--at",
            "00:00:00",
        ],
    );

    assert_eq!(code, 0, "只读预检应当正常退出\n{out}");
    assert!(out.contains("预检全部通过"), "{out}");
    assert!(out.contains("只读模式"), "{out}");
    // 绝对没有提交任何东西 —— 这是整个工具最重要的安全约束
    assert_eq!(mock.write_count(), 0, "不加 --live 绝不能有写请求\n{out}");
    // 会话校验与已选课程读取都真的发生了
    assert_eq!(mock.calls_to("student/2026000000.do").len(), 1, "{out}");
    assert!(!mock.calls_to("courseResult.do").is_empty(), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
    mock.close();
}

#[test]
fn cli_live_fires_and_reports() {
    let mock = Mock::start(Mode::Normal, vec![]);
    let dir = stage_config(mock.port, "live");
    let url = url_for(mock.port);

    // 窗口压到 1 秒：足以走完"首发 → 复核 → 重试循环 → 收尾报告"整条路
    let (code, out) = run_binary(
        &dir,
        &[
            "--url",
            &url,
            "--cookie",
            "JSESSIONID=S1",
            "--no-relogin",
            "--student",
            "2026000000",
            "--force",
            "--live",
            "--now",
            "--window",
            "1",
            "--burst",
            "0.5",
            "--interval",
            "0.8",
        ],
    );

    let writes = mock.write_count();
    assert!(
        writes >= 1,
        "LIVE 至少要发出首发那一发（实际 {writes}）\n{out}"
    );
    // 假服务端从不让"已选课程列表"出现目标班，所以最终是"未确认抢到"（退出码 1）
    assert_eq!(code, 1, "没抢到时的退出码应当是 1\n{out}");
    assert!(out.contains("写请求节拍器"), "{out}");
    assert!(out.contains("未确认抢到"), "{out}");
    // **每一发写请求都必须过写请求节拍器**（首发那几发也是）。这条不变量是硬的：
    // 首发与重试循环共用同一份额度，首发不记账的话，重试循环会以为窗口是空的，
    // 于是凑出第 N+1 发被限流 —— 实测那会当场作废整个会话。
    let opened: usize = out
        .split("共放行 ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(
        opened >= writes,
        "有写请求没被节拍器记账：实际发出 {writes} 发，节拍器只放行 {opened} 发\n{out}"
    );
    // 提交的报文体里 operationType 必须是 1（选课），绝不可能是 2（退选）
    let writes_detail = mock.calls_to("volunteer.do");
    for c in &writes_detail {
        assert!(
            c.field("addParam").contains(r#""operationType":"1""#),
            "提交的 operationType 必须是 1: {}",
            c.field("addParam")
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    mock.close();
}
