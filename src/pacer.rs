//! 写请求节拍器：**所有**写请求（首发那几发、重试循环的每一发）都要先过它。
//!
//! 实测模型（2026-09-25 用真实满员教学班复测，2/2 复现）：
//!     滚动 1 秒内最多 3 发写请求，第 4 发必定返回
//!     「请求过快，请登录后再试」，**并且当场作废整个会话**。
//!
//! 真实死因是这样的：首发 1 发（T+0.29s）+ 重试循环第一轮 3 发（T+1.0s）
//! = 1.05 秒内 4 发 → 第 4 发被限流 → 会话死 → 任务在放课瞬间自杀。
//!
//! 所以这里把"一秒钟最多 N 发"变成代码里的硬约束，任何调用路径都绕不过去。
//! 两个约束同时成立才放行：
//!   ① 滚动窗口内不超过 `per_window` 发；
//!   ② 与上一发至少隔开 `min_gap` —— 实测 0.03s 内连发两发，学校会回一个
//!      msg 为空的并发拒绝（等于白打一发），错开 0.06s 以上才稳。
//!
//! "遗忘"一条记录也要等到 `window + margin`：踩过一次边界 —— 第 4 发正好在第 1 发
//! 之后 1.000s 发出，我们这边已经"过期"了，学校那边还算在窗口内 → 4 发 → 当场被踢。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct WritePacer {
    pub per_window: usize,
    pub window: f64,
    pub margin: f64,
    pub min_gap: f64,
    /// 保守窗口 = 服务端窗口 + 安全边界
    pub span: f64,
    inner: Mutex<Inner>,
    pub waits: AtomicUsize,
    pub total: AtomicUsize,
}

struct Inner {
    times: VecDeque<Instant>,
    last: Option<Instant>,
}

impl WritePacer {
    pub fn new(per_window: usize, window: f64, margin: f64, min_gap: f64) -> WritePacer {
        WritePacer {
            per_window: per_window.max(1),
            window,
            margin,
            min_gap: min_gap.max(0.0),
            span: window + margin,
            inner: Mutex::new(Inner {
                times: VecDeque::new(),
                last: None,
            }),
            waits: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 节拍器被 poison 也必须继续工作：放课瞬间不能因为一次 panic 就停摆
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前滚动窗口内已经用掉几个名额（调试用）。
    pub fn snapshot(&self) -> usize {
        let now = Instant::now();
        let inner = self.lock();
        inner
            .times
            .iter()
            .filter(|t| now.duration_since(**t).as_secs_f64() < self.span)
            .count()
    }

    /// 拿到一个写请求名额；拿不到就在这里等到能拿。返回等待秒数。
    pub fn acquire(&self) -> f64 {
        let mut waited = 0.0f64;
        loop {
            let sleep_for = {
                let now = Instant::now();
                let mut inner = self.lock();
                while let Some(front) = inner.times.front().copied() {
                    if now.duration_since(front).as_secs_f64() >= self.span {
                        inner.times.pop_front();
                    } else {
                        break;
                    }
                }
                let gap_left = match inner.last {
                    Some(last) => self.min_gap - now.duration_since(last).as_secs_f64(),
                    None => 0.0,
                };
                if inner.times.len() < self.per_window && gap_left <= 0.0 {
                    inner.times.push_back(now);
                    inner.last = Some(now);
                    self.total.fetch_add(1, Ordering::Relaxed);
                    return waited;
                }
                let mut sleep_for = gap_left.max(0.0);
                if inner.times.len() >= self.per_window {
                    let oldest = inner.times.front().copied().unwrap_or(now);
                    let left = self.span - now.duration_since(oldest).as_secs_f64();
                    sleep_for = sleep_for.max(left);
                }
                sleep_for
            };
            let chunk = sleep_for.clamp(0.005, 0.25);
            self.waits.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs_f64(chunk));
            waited += chunk;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_of_per_window() {
        let p = WritePacer::new(3, 1.0, 0.15, 0.0);
        let t0 = Instant::now();
        for _ in 0..3 {
            p.acquire();
        }
        // 前三发是第一个窗口的额度，应当几乎零等待
        assert!(t0.elapsed().as_secs_f64() < 0.05, "{:?}", t0.elapsed());
        assert_eq!(p.total.load(Ordering::Relaxed), 3);
        assert_eq!(p.snapshot(), 3);
    }

    #[test]
    fn blocks_the_fourth_until_window_plus_margin() {
        let p = WritePacer::new(3, 0.3, 0.1, 0.0);
        for _ in 0..3 {
            p.acquire();
        }
        let t0 = Instant::now();
        p.acquire(); // 第 4 发必须等满 0.3+0.1
        let waited = t0.elapsed().as_secs_f64();
        assert!(
            waited >= 0.35,
            "第 4 发只等了 {waited:.3}s，说明窗口边界没守住"
        );
        assert_eq!(p.total.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn respects_min_gap() {
        // per_window 给足，只让 min_gap 生效
        let p = WritePacer::new(100, 1.0, 0.0, 0.12);
        p.acquire();
        let t0 = Instant::now();
        p.acquire();
        assert!(t0.elapsed().as_secs_f64() >= 0.10, "{:?}", t0.elapsed());
    }

    #[test]
    fn is_thread_safe() {
        use std::sync::Arc;
        let p = Arc::new(WritePacer::new(3, 0.2, 0.05, 0.0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let p = Arc::clone(&p);
            handles.push(std::thread::spawn(move || {
                for _ in 0..3 {
                    p.acquire();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(p.total.load(Ordering::Relaxed), 12);
    }
}
