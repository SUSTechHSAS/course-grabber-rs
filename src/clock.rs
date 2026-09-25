//! 服务器时钟对齐与出手时机。
//!
//! 只用 HTTP `Date` 头，**不用** JSON 里的 timestamp 字段：实测那个字段在 RTT 仅
//! 28ms 的情况下摆动超过 1100ms，甚至出现时间倒流（后端多台机器时钟不同步 / 缓存），
//! 拿它做对时会把偏移算反、算错一个数量级。

use crate::timeutil::{self, parse_hms};

/// 一次 Date 采样：服务器整秒、本机中点时刻、RTT。
#[derive(Clone, Copy, Debug)]
pub struct DateSample {
    pub sec: i64,
    pub mid: f64,
    pub rtt: f64,
}

/// 用 HTTP Date 头做严格区间估计，返回 `(offset, half_width_ms)`。
///
/// `offset = 服务器时间 - 本机时间`（秒）。`half_width_ms < 0` 表示这个估计不可用：
/// * `-1`：一次都没采到
/// * `-2`：样本之间对不上（离群太多），不猜
///
/// Date 头只有 1 秒分辨率，但「S <= 服务器时间 < S+1」这个约束对多个样本求交集后，
/// 可以把偏移夹到 RTT 量级。Date 是在 [t0,t1] 之间某一刻生成的，所以每条约束要
/// 放宽 ±RTT/2，否则交集会被压成空集。
pub fn server_offset(samples: &[DateSample]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, -1.0);
    }

    // 网格投票：每个样本给出「offset 必须落在 [sec-mid-rtt/2, sec+1-mid+rtt/2)」这条
    // 约束，取被最多样本同时覆盖的偏移。
    // 不用增量求交 —— 那是贪心且依赖顺序的：某一步走偏就会把正确样本当离群点丢掉，
    // 然后锁死在一个错误区间上（实测会给出 ±6ms 这种假精确值）。
    const STEP: f64 = 0.005;
    const GRID_LO: f64 = -1.5;
    const GRID_HI: f64 = 1.5;
    let n_bins = (((GRID_HI - GRID_LO) / STEP).round() as usize) + 1;
    let mut votes = vec![0u32; n_bins];
    for s in samples {
        let a = s.sec as f64 - s.mid - s.rtt / 2.0;
        let b = (s.sec + 1) as f64 - s.mid + s.rtt / 2.0;
        for (i, v) in votes.iter_mut().enumerate() {
            let x = GRID_LO + i as f64 * STEP;
            if a <= x && x < b {
                *v += 1;
            }
        }
    }

    let best = *votes.iter().max().unwrap_or(&0);
    let agree = best as f64 / samples.len() as f64;
    if agree < 0.6 {
        // 样本之间对不上，不猜
        return (0.0, -2.0);
    }
    let idx: Vec<usize> = votes
        .iter()
        .enumerate()
        .filter(|(_, v)| **v == best)
        .map(|(i, _)| i)
        .collect();
    let (first, last) = (idx[0], idx[idx.len() - 1]);
    let est = GRID_LO + (first + last) as f64 / 2.0 * STEP;
    let half = (last - first) as f64 * STEP * 500.0;
    if est.abs() > 0.5 {
        // 超出合理范围，视为不可信
        return (0.0, -2.0);
    }
    (est, half.max(STEP * 500.0))
}

/// 只校验 `--at` 的格式，不决定任何时刻。用于"联网之前先拦下手滑的参数"。
pub fn parse_at(at: &str) -> Result<(u32, u32, u32), String> {
    parse_hms(at).ok_or_else(|| format!("--at 格式应为 HH:MM:SS，收到 {at:?}"))
}

/// 决定什么时候出手，并返回一句人话说明。第二个返回值是错误消息（对应原版直接退出）。
///
/// 这里刻意**不**沿用「已过就取明天」的老逻辑：放课之后才是捡漏的时段
/// （有人退课就漏位子），20:00 后重启脚本必须立刻开打，而不是干等 24 小时。
/// 要等到明天请显式加 `--tomorrow`。
pub fn resolve_fire_time(
    at: &str,
    offset: f64,
    early: f64,
    now: bool,
    tomorrow: bool,
) -> Result<(f64, String), String> {
    if now {
        return Ok((timeutil::unix_now(), "立即（--now）".to_string()));
    }
    let Some((hh, mm, ss)) = parse_hms(at) else {
        return Err(format!("--at 格式应为 HH:MM:SS，收到 {at:?}"));
    };

    let cur = timeutil::civil(timeutil::unix_now());
    // 今天的目标时刻（配置时区）
    let target_today = timeutil::unix_from_civil(cur.year, cur.month, cur.day, hh, mm, ss);
    let now_unix = timeutil::unix_now();
    if target_today > now_unix {
        return Ok((target_today - offset - early, format!("今天 {at}")));
    }
    if tomorrow {
        let t = timeutil::unix_from_civil(cur.year, cur.month, cur.day + 1, hh, mm, ss);
        return Ok((t - offset - early, format!("明天 {at}")));
    }
    Ok((
        now_unix,
        format!("今天 {at} 已过 → 立即开始（想等明天请加 --tomorrow）"),
    ))
}

/// 目标时刻是不是已经过了（或马上就要过）。
///
/// 过了就没必要再做"对齐服务器时钟"这种为精确对点服务的预检 —— 直接开打才是对的。
/// `--tomorrow` 明确表示要等明天，不算过点。
pub fn target_already_passed(at: &str, now: bool, tomorrow: bool) -> bool {
    if now {
        return true;
    }
    if tomorrow {
        return false;
    }
    let Some((hh, mm, ss)) = parse_hms(at) else {
        return false;
    };
    let cur = timeutil::civil(timeutil::unix_now());
    let target = timeutil::unix_from_civil(cur.year, cur.month, cur.day, hh, mm, ss);
    target - timeutil::unix_now() <= 10.0 // 10 秒内也算"到了"
}

/// 从课表文本（如 `5-18周 星期三 3-5节 某楼101`）里提取冲突组 `"星期三-3-5"`。
///
/// 同一冲突组的教学班互相撞时间，学校最多只让中一个，所以并发发是安全的；
/// 不同组（周三 vs 周五）并发则可能真的同时选上两门。
pub fn time_group(place: &str) -> String {
    let chars: Vec<char> = place.trim().chars().collect();
    // 逐个"星期X"/"周X"去试 —— Python 那边是 re.search，第一个日期词后面找不到节次
    // 时会换下一个起点继续找（不是只看第一个）。
    for i in 0..chars.len() {
        let Some((day_char, after)) = day_name_at(&chars, i) else {
            continue;
        };
        // `.{0,12}?` 不跨换行：窗口里一旦出现换行，比它更长的窗口也跨换行，
        // 就没有再往后试的必要了。
        for start in after..=(after + 12) {
            if start >= chars.len() || chars[after..start].contains(&'\n') {
                break;
            }
            if let Some((text, _end)) = match_period(&chars, start) {
                let periods: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                let periods = periods.replace('至', "-");
                return format!("星期{day_char}-{periods}");
            }
        }
    }
    String::new()
}

/// `chars[i..]` 处是不是一个"星期X"或"周X"，是的话返回 (那个字, 日期词之后的下标)。
fn day_name_at(chars: &[char], i: usize) -> Option<(char, usize)> {
    const NAMES: [char; 8] = ['一', '二', '三', '四', '五', '六', '日', '天'];
    if chars.get(i) == Some(&'星') && chars.get(i + 1) == Some(&'期') {
        let d = *chars.get(i + 2)?;
        if NAMES.contains(&d) {
            return Some((d, i + 3));
        }
    }
    if chars.get(i) == Some(&'周') {
        let d = *chars.get(i + 1)?;
        if NAMES.contains(&d) {
            return Some((d, i + 2));
        }
    }
    None
}

/// 在 `chars[pos..]` 处尝试匹配 `数字[-或至]数字`，返回匹配到的原文与结束下标。
fn match_period(chars: &[char], pos: usize) -> Option<(String, usize)> {
    let mut i = pos;
    let start = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let after_digits = i;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() || (chars[i] != '-' && chars[i] != '至') {
        return None;
    }
    i += 1;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    let d2 = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == d2 {
        return None;
    }
    let _ = after_digits;
    Some((chars[start..i].iter().collect(), i))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一批自洽的样本：服务器比本机快 `offset` 秒，Date 头只有整秒，
    /// 每次采样的本机中点按 `step` 前进（真实代码是 25 次 × 0.02s 的节奏）。
    fn make_samples(offset: f64, n: usize, step: f64) -> Vec<DateSample> {
        let mut t = 1_790_000_000.0;
        (0..n)
            .map(|_| {
                let s = DateSample {
                    sec: (t + offset).floor() as i64,
                    mid: t,
                    rtt: 0.02,
                };
                t += step;
                s
            })
            .collect()
    }

    #[test]
    fn offset_from_consistent_samples() {
        // 采样跨过一个整秒，区间才会收窄到几十毫秒（这正是原版注释里的结论）
        let samples = make_samples(0.4, 25, 0.05);
        let (offset, half) = server_offset(&samples);
        assert!((offset - 0.4).abs() < 0.06, "offset={offset}");
        assert!(half > 0.0 && half < 60.0, "half={half}");
    }

    #[test]
    fn rejects_wildly_disagreeing_samples() {
        // 一半样本说快 0.4s，一半说慢 1.4s —— 两组票数都不到 60%，不该硬猜
        let mut samples = make_samples(0.4, 25, 0.05);
        for (i, s) in samples.iter_mut().enumerate() {
            if i % 2 == 1 {
                s.sec = (s.mid - 1.4).floor() as i64;
            }
        }
        assert_eq!(server_offset(&samples), (0.0, -2.0));
    }

    #[test]
    fn empty_samples_are_reported() {
        assert_eq!(server_offset(&[]), (0.0, -1.0));
    }

    #[test]
    fn time_group_extracts_periods() {
        assert_eq!(time_group("5-18周 星期三 3-5节 某楼101"), "星期三-3-5");
        assert_eq!(time_group("1-16周 周五 6至8节 教学楼"), "星期五-6-8");
        assert_eq!(time_group("星期二 1-2节"), "星期二-1-2");
        assert_eq!(time_group("周天 3 - 5 节"), "星期天-3-5");
        // 第一个日期词后面 12 个字符里没有节次时，要换下一个日期词继续找
        //（Python 是 re.search，不是只认第一个）
        assert_eq!(
            time_group("星期三 一二三四五六七八九十 星期五 6-8节"),
            "星期五-6-8"
        );
        // `.{0,12}?` 不跨换行（Python 正则的 `.` 不匹配 \n）
        assert_eq!(time_group("星期三\n3-5节 教学楼"), "");
        assert_eq!(time_group("没有时间信息"), "");
        assert_eq!(time_group(""), "");
    }

    #[test]
    fn fire_time_past_goes_now() {
        // 今天的 00:00:00 一定已经过了
        let (fire, text) = resolve_fire_time("00:00:00", 0.0, 0.0, false, false).unwrap();
        assert!((fire - timeutil::unix_now()).abs() < 1.0);
        assert!(text.contains("已过"));
        // --tomorrow 则瞄准"明天的 00:00:00"（一定在未来、且是整点）
        let (fire2, text2) = resolve_fire_time("00:00:00", 0.0, 0.0, false, true).unwrap();
        assert!(fire2 > timeutil::unix_now(), "明天的整点必须还在未来");
        assert_eq!(timeutil::civil(fire2).hms(), "00:00:00");
        assert!(text2.contains("明天"));
    }

    #[test]
    fn fire_time_bad_format() {
        assert!(resolve_fire_time("25:00:00", 0.0, 0.0, false, false).is_err());
        assert!(resolve_fire_time("8pm", 0.0, 0.0, false, false).is_err());
    }
}
