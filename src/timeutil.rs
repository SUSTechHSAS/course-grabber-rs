//! 时间与时区：程序全程按配置里的固定偏移（默认 +8）显示和判断"几点了"。
//!
//! 为什么不用本地时区：放课时刻是**学校那边的墙上时间**，脚本可能跑在任何时区的
//! 机器上（原版 Python 也是这么做的：`timezone(timedelta(hours=...))`，不读系统时区）。
//! 所以这里只需要"UTC + 固定偏移"，不需要 tzdata，也就不需要任何依赖。

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 时区偏移（秒）。在配置读进来之后由 `set_offset_secs` 设定，默认 +8。
static OFFSET_SECS: AtomicI64 = AtomicI64::new(8 * 3600);

pub fn set_offset_secs(secs: i64) {
    OFFSET_SECS.store(secs, Ordering::Relaxed);
}

pub fn offset_secs() -> i64 {
    OFFSET_SECS.load(Ordering::Relaxed)
}

/// 当前 Unix 时间（秒，带小数）。负数/异常一律退化成 0，绝不 panic。
pub fn unix_now() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -(e.duration().as_secs_f64()),
    }
}

/// 单调时钟读数（秒）。用它算"过了多久"，不用墙钟。
pub fn mono_now() -> f64 {
    // Instant 没有 f64 接口，借 duration_since 一个固定起点换算。
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}

/// 拆开的日历时间（`civil` = 公历）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
}

/// 把 Unix 秒换成配置时区下的日历时间。
///
/// 日期部分用的是 Howard Hinnant 的 `civil_from_days`（对 1970±30000 年都成立），
/// 不查表、不考虑闰秒 —— 与 Python 的 `datetime.fromtimestamp` 一致。
pub fn civil(unix_secs: f64) -> Civil {
    let shifted = unix_secs + offset_secs() as f64;
    // 负数要向下取整（floor），否则 1969 年的时刻会被算晚一年
    let total = shifted.floor();
    let days = (total / 86400.0).floor() as i64;
    let mut rem = total as i64 - days * 86400;
    if rem < 0 {
        rem += 86400;
    }
    let (y, m, d) = civil_from_days(days);
    Civil {
        year: y,
        month: m,
        day: d,
        hour: (rem / 3600) as u32,
        minute: ((rem % 3600) / 60) as u32,
        second: (rem % 60) as u32,
        millis: ((shifted - total) * 1000.0).round() as u32 % 1000,
    }
}

/// days 是"1970-01-01 起的天数"，返回 (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 从"年月日时分秒"（配置时区）反算 Unix 秒。用于算放课时刻。
pub fn unix_from_civil(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> f64 {
    let days = days_from_civil(year, month, day);
    let local = days as f64 * 86400.0 + (hour * 3600 + minute * 60 + second) as f64;
    local - offset_secs() as f64
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

// ---------------------------------------------------------------------------
// 格式化（都在配置时区下）
// ---------------------------------------------------------------------------

impl Civil {
    /// "HH:MM:SS"
    pub fn hms(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// "HH:MM:SS.mmm" —— 日志前缀与写请求调试输出用
    pub fn hms_ms(&self) -> String {
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            self.hour, self.minute, self.second, self.millis
        )
    }

    /// "YYYY-MM-DD HH:MM:SS"（只有自检用得到：日志里从不打完整日期）
    #[cfg(test)]
    pub fn ymd_hms(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {}",
            self.year,
            self.month,
            self.day,
            self.hms()
        )
    }
}

/// `civil(now()).hms_ms()` 的简写。
pub fn ts() -> String {
    civil(unix_now()).hms_ms()
}

/// 解析 "HH:MM:SS"，全范围校验（和原版一样拒绝 25:00:00 这种）。
pub fn parse_hms(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut out = [0u32; 3];
    for (i, p) in parts.iter().enumerate() {
        let v: i64 = p.trim().parse().ok()?;
        if !(0..=59).contains(&v) {
            return None;
        }
        out[i] = v as u32;
    }
    if out[0] > 23 {
        return None;
    }
    Some((out[0], out[1], out[2]))
}

// ---------------------------------------------------------------------------
// HTTP Date 头解析
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// 把 HTTP `Date` 头解析成整秒 epoch；解析不了返回 None。
///
/// 兼容两种常见写法（服务器实际只会发第一种）：
///   IMF-fixdate  Fri, 25 Sep 2026 12:00:00 GMT
///   RFC 850      Friday, 25-Sep-26 12:00:00 GMT
///
/// 与 Python 的 `email.utils.parsedate_to_datetime` 对齐的两点：认不出时区就返回
/// None（原版显式 `if dt.tzinfo is None: return None`，asctime 那种 `Fri Sep 25
/// 12:00:00 2026` 因此会被拒），两位年份按 1969/2068 分界。
pub fn parse_http_date(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // token 化：按逗号/空格/连字符切开，丢掉星期几
    let cleaned = s.replace(',', " ");
    let words: Vec<&str> = cleaned
        .split([' ', '-'])
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    let mut idx = 0;
    if let Some(first) = words.first() {
        let low = first.to_ascii_lowercase();
        if WEEKDAYS.iter().any(|w| low.starts_with(w)) {
            idx = 1;
        }
    }
    let rest = &words[idx.min(words.len())..];
    if rest.len() < 4 {
        return None;
    }

    // 时区：必须认出来，否则不猜（原版的 tzinfo is None 分支）
    let tz_off = parse_tz(rest)?;

    // 两种排布：日 月 年 时:分:秒   |   月 日 时:分:秒 年
    let (day, month, year, clock) = if rest[0].bytes().all(|b| b.is_ascii_digit()) {
        (rest[0], rest[1], rest[2], rest[3])
    } else if rest[1].bytes().all(|b| b.is_ascii_digit()) {
        (rest[1], rest[0], rest[3], rest[2])
    } else {
        return None;
    };

    let day: u32 = day.parse().ok()?;
    let month_name = month.to_ascii_lowercase();
    let month = MONTHS
        .iter()
        .position(|m| month_name.starts_with(m))
        .map(|i| i as u32 + 1)?;
    let mut year: i64 = year.parse().ok()?;
    if year < 100 {
        // 两位年份：69..=99 → 1900 段，00..=68 → 2000 段
        year += if year < 69 { 2000 } else { 1900 };
    }

    let clock_parts: Vec<&str> = clock.split(':').collect();
    if clock_parts.len() < 3 {
        return None;
    }
    let hour: u32 = clock_parts[0].parse().ok()?;
    let minute: u32 = clock_parts[1].parse().ok()?;
    let second: u32 = clock_parts[2].parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 || day == 0 || day > 31 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(days * 86400 + (hour * 3600 + minute * 60 + second) as i64 - tz_off)
}

/// 认时区。"GMT"/"UTC"/"Z" 和 "+0800" 都接受；认不出返回 None。
fn parse_tz(words: &[&str]) -> Option<i64> {
    for w in words {
        let low = w.to_ascii_lowercase();
        if low == "gmt" || low == "utc" || low == "z" {
            return Some(0);
        }
        if low == "est" {
            return Some(-5 * 3600);
        }
        if low == "edt" {
            return Some(-4 * 3600);
        }
        if low == "cst" {
            return Some(-6 * 3600);
        }
        if low == "pst" {
            return Some(-8 * 3600);
        }
        if low == "pdt" {
            return Some(-7 * 3600);
        }
        let b = low.as_bytes();
        if (b[0] == b'+' || b[0] == b'-')
            && b.len() >= 3
            && b[1..].iter().all(|c| c.is_ascii_digit())
        {
            let sign = if b[0] == b'-' { -1 } else { 1 };
            let digits: String = low[1..].chars().filter(|c| c.is_ascii_digit()).collect();
            let (h, m) = match digits.len() {
                1 | 2 => (digits.parse::<i64>().ok()?, 0),
                3 => (
                    digits[0..1].parse::<i64>().ok()?,
                    digits[1..3].parse::<i64>().ok()?,
                ),
                _ => (
                    digits[0..2].parse::<i64>().ok()?,
                    digits[2..4].parse::<i64>().ok()?,
                ),
            };
            return Some(sign * (h * 3600 + m * 60));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // 注意：这些测试**不去改**全局时区 —— 测试是并行跑的，改全局会把别的测试
    // 一起改坏（踩过一次：clock 的测试因此偶发失败）。默认就是 +8。

    #[test]
    fn civil_round_trip() {
        // 2026-09-25 20:00:00 +08
        let t = unix_from_civil(2026, 9, 25, 20, 0, 0);
        let c = civil(t);
        assert_eq!((c.year, c.month, c.day), (2026, 9, 25));
        assert_eq!(c.hms(), "20:00:00");
    }

    #[test]
    fn civil_epoch_and_leap() {
        // 时区 +8：epoch 0 是当地 1970-01-01 08:00:00
        assert_eq!(civil(0.0).ymd_hms(), "1970-01-01 08:00:00");
        let feb29 = unix_from_civil(2024, 2, 29, 12, 0, 0);
        assert_eq!(civil(feb29).ymd_hms(), "2024-02-29 12:00:00");
        // 1970 年之前（负时间）不能算错
        assert_eq!(civil(-1.0).ymd_hms(), "1970-01-01 07:59:59");
    }

    #[test]
    fn http_date_imf() {
        // 2026-09-25 12:00:00 GMT（epoch 1790337600 是用 Python 的 parsedate_to_datetime 对出来的）
        const WANT: i64 = 1790337600;
        assert_eq!(parse_http_date("Fri, 25 Sep 2026 12:00:00 GMT"), Some(WANT));
        assert_eq!(
            parse_http_date("Friday, 25-Sep-26 12:00:00 GMT"),
            Some(WANT)
        );
        assert_eq!(
            parse_http_date("Fri, 25 Sep 2026 12:00:00 +0000"),
            Some(WANT)
        );
        assert_eq!(
            parse_http_date("Fri, 25 Sep 2026 20:00:00 +0800"),
            Some(WANT)
        );
        assert_eq!(parse_http_date("garbage"), None);
        assert_eq!(parse_http_date("Fri, 25 Sep 2026 12:00:00"), None); // 没时区不猜
                                                                        // asctime 不带时区 —— 原版也会把它判成"不可信"，我们也一样
        assert_eq!(parse_http_date("Fri Sep 25 12:00:00 2026"), None);
    }

    #[test]
    fn hms_parse() {
        assert_eq!(parse_hms("20:00:00"), Some((20, 0, 0)));
        assert_eq!(parse_hms("00:00:59"), Some((0, 0, 59)));
        assert_eq!(parse_hms("25:00:00"), None);
        assert_eq!(parse_hms("20:60:00"), None);
        assert_eq!(parse_hms("200000"), None);
    }
}
