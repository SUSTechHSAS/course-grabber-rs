//! 终端里的"一个字占几列"。
//!
//! TUI 里到处要按**显示宽度**而不是字节数或字符数排版：配置里的键和值大多是中文
//! （`目标课程`、`星期一-3-5`），一个汉字占两列。用 `s.len()` 排版会让右边的边框
//! 参差不齐，用 `chars().count()` 也一样。
//!
//! 这里只实现 wcwidth 的一个够用子集（East Asian Wide/Fullwidth + 组合符零宽）。
//! 拉 unicode-width 这个 crate 能得到更完整的表，但为了排版对齐多带一张几十 KB 的
//! 表，和本项目"零第三方运行时"的取舍不符 —— 我们真正要排的就是中英日韩加数字符号。

/// 一个字符占几列：0（组合符）、1 或 2。
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    // 控制字符当作 0：它们不该出现在要排版的值里（真有的话别把行撑歪）
    if u < 0x20 || u == 0x7f {
        return 0;
    }
    if is_zero_width(u) {
        return 0;
    }
    if is_wide(u) {
        return 2;
    }
    1
}

/// 组合符、变体选择符、零宽空格那一类：不占列。
fn is_zero_width(u: u32) -> bool {
    matches!(u,
        0x0300..=0x036F   // 组合用附加符号
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E   // 泰文
        | 0x200B..=0x200F   // 零宽空格 / 双向标记
        | 0x2028..=0x202E
        | 0x2060..=0x2064
        | 0x20D0..=0x20F0   // 组合用符号
        | 0xFE00..=0xFE0F   // 变体选择符
        | 0xFE20..=0xFE2F
        | 0xFEFF            // BOM
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0xE0100..=0xE01EF // 变体选择符补充
    )
}

/// 东亚宽字符（占两列）。
fn is_wide(u: u32) -> bool {
    matches!(u,
        0x1100..=0x115F     // 谚文字母
        | 0x2E80..=0x303E   // 康熙部首、CJK 部首、CJK 符号
        | 0x3041..=0x33FF   // 平假名/片假名/注音/CJK 兼容
        | 0x3400..=0x4DBF   // CJK 扩展 A
        | 0x4E00..=0x9FFF   // CJK 基本区
        | 0xA000..=0xA4CF   // 彝文
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3   // 谚文音节
        | 0xF900..=0xFAFF   // CJK 兼容表意文字
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60   // 全角形式
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F // 表情
        | 0x1F680..=0x1F6FF
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD // CJK 扩展 B 及以后
    )
}

/// 字符串的显示宽度（列数）。
pub fn width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 按显示宽度截断。
///
/// 返回值是 `(可见文本, 实际占用的列数)`；被截断时末尾补一个 `…`（它占 1 列，
/// 算在 `max` 之内）。绝不会把一个宽字符劈成半个 —— 那会让后续排版整体错位。
pub fn clip(s: &str, max: usize) -> (String, usize) {
    if max == 0 {
        return (String::new(), 0);
    }
    if width(s) <= max {
        return (s.to_string(), width(s));
    }
    // 留一列给省略号
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    (out, used + 1)
}

/// 右侧补空格到 `cols` 列（已经够宽就原样返回）。
pub fn pad_right(s: &str, cols: usize) -> String {
    let w = width(s);
    if w >= cols {
        return s.to_string();
    }
    let mut out = s.to_string();
    out.push_str(&" ".repeat(cols - w));
    out
}

/// 按显示宽度折行。中文没有空格可断，所以就是硬折；
/// 从第二行起加 `indent`，一眼能看出是上一行的继续。
///
/// 报告页（向导、自检结果）用这个：那些话必须整句看得见 —— 把它们裁成
/// 「按 v 看校验，或者用「联网自检」…」就等于没说。
pub fn wrap(s: &str, cols: usize, indent: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let budget = cols.saturating_sub(width(indent)).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw > budget && !line.is_empty() {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push(c);
        used += cw;
    }
    if !line.is_empty() {
        out.push(line);
    }
    for l in out.iter_mut().skip(1) {
        l.insert_str(0, indent);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_counts_two_columns() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("目标课程"), 8);
        assert_eq!(width("01班 周一3-5"), 12); // 1+1+2+1+2+2+1+1+1
        assert_eq!(char_width('课'), 2);
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\u{0301}'), 0); // 组合用锐音符
    }

    #[test]
    fn clip_never_splits_a_wide_char() {
        // 预算 3 列 + 省略号：只能放 1 个汉字
        let (s, used) = clip("目标课程", 4);
        assert_eq!(s, "目…");
        assert_eq!(used, 3); // 2 + 1，没超过 max
        assert!(width(&s) <= 4);

        // 刚好装得下就原样返回
        let (s, used) = clip("目标课程", 8);
        assert_eq!(s, "目标课程");
        assert_eq!(used, 8);

        // ASCII 截断
        let (s, used) = clip("abcdef", 4);
        assert_eq!(s, "abc…");
        assert_eq!(used, 4);

        // max=0 不能 panic，也不能返回省略号
        assert_eq!(clip("abc", 0), (String::new(), 0));
    }

    #[test]
    fn pad_right_uses_display_width() {
        assert_eq!(pad_right("中文", 6), "中文  ");
        assert_eq!(pad_right("中文", 2), "中文");
    }

    #[test]
    fn wrap_breaks_by_display_width_and_indents() {
        // 缩进也算在宽度里：宽 4 + 缩进 2 → 每行只放得下 2 个字符
        let got = wrap("abcdefghij", 4, "  ");
        assert_eq!(got, vec!["ab", "  cd", "  ef", "  gh", "  ij"]);

        // 一行放得下就原样返回
        assert_eq!(wrap("短", 10, "  "), vec!["短"]);
        // 空串给一行空的（调用方按行渲染，不能凭空多一行也不能少）
        assert_eq!(wrap("", 10, "  "), vec![String::new()]);

        // 中文按两列算，且不劈开宽字符
        let got = wrap("目标课程名", 6, "");
        assert_eq!(got, vec!["目标课", "程名"]);
        for l in &got {
            assert!(width(l) <= 6);
        }
        // 缩进占用宽度，所以内容更早换行
        let got = wrap("目标课程名", 6, "  ");
        assert_eq!(got, vec!["目标", "  课程", "  名"]);
    }
}
