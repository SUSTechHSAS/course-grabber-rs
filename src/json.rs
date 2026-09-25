//! 极简 JSON：只做这套工具真正用得到的事。
//!
//! 为什么不用 serde_json：抢课报文里要序列化的东西一共就那几个字段
//! （`addParam` 里的一层对象 + 一个课程目录查询设置），要解析的则是学校返回的
//! 松散对象。为了这点需求把 serde + serde_json 收进二进制，值不了那几百 KB ——
//! 这个项目的卖点就是"零第三方运行时"。
//!
//! 与 Python 的 `json` 模块对齐的地方（都是实际报文里会碰到的）：
//!
//! * **`\uXXXX` 转义**：学校的 Java 后端 `json.dumps` 默认把中文写成转义序列，
//!   所以解析器必须解开它（含代理对），否则所有中文提示都读不出来。
//! * **序列化时不做 ASCII 转义**（对应 `ensure_ascii=False`）：`queryContent` 里
//!   可能是中文课程名，要和原版发出完全一样的字节。
//! * **数字区分整数与浮点**：`str(code)` 在 Python 里对 1 是 "1"、对 1.0 是 "1.0"，
//!   而服务端的 code 判定是字符串比较，所以不能一律用 f64。

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    /// 取成员；不是对象或没有这个键都返回 None。
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `payload.get(k) or []` —— 缺键/类型不对都当成空数组
    pub fn array(&self, key: &str) -> &[Json] {
        match self.get(key) {
            Some(Json::Arr(a)) => a,
            _ => &[],
        }
    }

    /// `payload.get(k) or {}` —— 缺键/类型不对都当成空对象
    pub fn object(&self, key: &str) -> Option<&Json> {
        match self.get(key) {
            Some(v @ Json::Obj(_)) => Some(v),
            _ => None,
        }
    }

    /// `str(payload.get(k) or "")` —— 字符串原样，数字按 Python 的 str() 排布，
    /// 其余（null/数组/对象）给空串。这一条被 classify() 和登录返回码判定依赖。
    pub fn text(&self, key: &str) -> String {
        match self.get(key) {
            Some(v) => v.as_text(),
            None => String::new(),
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            Json::Str(s) => s.clone(),
            // Python: str(1) == "1"，str(1.0) == "1.0"
            Json::Int(i) => i.to_string(),
            Json::Float(f) => {
                if f.is_finite() && f.fract() == 0.0 {
                    format!("{f:.1}")
                } else {
                    format!("{f}")
                }
            }
            Json::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Json::Null => String::new(),
            _ => String::new(),
        }
    }

    /// 字符串化（和 Python 的 `str(x)` 对字符串/数字一致），用于把整个响应体
    /// 拼进 classify() 的 `hay`（原版是 `f"{msg}\n{text}"`）。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            Json::Float(f) if f.is_finite() => Some(*f as i64),
            Json::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// 松散取值：能当数字用就给数字（对应原版 `int(cap.get(...) or 0)`）。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            Json::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn is_empty_object(&self) -> bool {
        matches!(self, Json::Obj(p) if p.is_empty())
    }

    // ---------------------------------------------------------------
    // 序列化
    // ---------------------------------------------------------------

    /// 紧凑形式（对应 `separators=(",", ":")` + `ensure_ascii=False`）。
    pub fn to_compact(&self) -> String {
        let mut out = String::with_capacity(64);
        self.write_compact(&mut out);
        out
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Json::Float(f) => {
                if f.is_finite() {
                    // Python 的 repr(1.0) 是 "1.0"；这里也只有自查用得到
                    if f.fract() == 0.0 && f.abs() < 1e16 {
                        let _ = write!(out, "{f:.1}");
                    } else {
                        let _ = write!(out, "{f}");
                    }
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_json_string(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_compact(out);
                }
                out.push(']');
            }
            Json::Obj(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_compact(out);
                }
                out.push('}');
            }
        }
    }
}

/// 字符串转义：与 Python `json.dumps(..., ensure_ascii=False)` 的默认转义一致
/// （`"`、`\`、控制字符；其余原样输出 UTF-8）。
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

const MAX_DEPTH: usize = 64;

/// 解析失败一律返回 Err，调用方按原版的做法退化成空对象 —— 学校偶尔会返回
/// HTML 错误页，那不是程序该崩的地方。
///
/// 错误就是 `()`（对应原版那句 `except ValueError`）：调用方从来不需要区分
/// "为什么解析不了"，只需要知道"这不是 JSON"。给个错误类型只是多一层包装。
#[allow(clippy::result_unit_err)]
pub fn parse(text: &str) -> Result<Json, ()> {
    let bytes = text.as_bytes();
    let mut p = Parser { b: bytes, i: 0 };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.i != bytes.len() {
        return Err(()); // 和 Python 一样：尾随内容算失败
    }
    Ok(v)
}

/// 宽松解析：失败给 `Json::Obj(vec![])`（原版到处是 `except ValueError: payload = {}`）。
pub fn parse_or_empty(text: &str) -> Json {
    parse(text).unwrap_or(Json::Obj(Vec::new()))
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn value(&mut self, depth: usize) -> Result<Json, ()> {
        if depth > MAX_DEPTH {
            return Err(());
        }
        match self.peek().ok_or(())? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => {
                self.literal(b"true")?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.literal(b"false")?;
                Ok(Json::Bool(false))
            }
            b'n' => {
                self.literal(b"null")?;
                Ok(Json::Null)
            }
            _ => self.number(),
        }
    }

    fn literal(&mut self, want: &[u8]) -> Result<(), ()> {
        if self.b.len() >= self.i + want.len() && &self.b[self.i..self.i + want.len()] == want {
            self.i += want.len();
            Ok(())
        } else {
            Err(())
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ()> {
        self.i += 1; // '{'
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(());
            }
            self.i += 1;
            self.skip_ws();
            let val = self.value(depth + 1)?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(pairs));
                }
                _ => return Err(()),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ()> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(()),
            }
        }
    }

    fn string(&mut self) -> Result<String, ()> {
        if self.peek() != Some(b'"') {
            return Err(());
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or(())?;
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let e = self.peek().ok_or(())?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&hi) {
                                // 代理对：后面必须跟一个低代理，否则按替换字符处理
                                // （Python 的 json 会报错，但那不是我们要区分的行为）
                                if self.peek() == Some(b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                        char::from_u32(cp).unwrap_or('\u{fffd}')
                                    } else {
                                        '\u{fffd}'
                                    }
                                } else {
                                    '\u{fffd}'
                                }
                            } else {
                                char::from_u32(hi).unwrap_or('\u{fffd}')
                            };
                            out.push(ch);
                        }
                        _ => return Err(()),
                    }
                }
                _ => {
                    // 原样拷贝一个 UTF-8 字符（不能按字节切，中文是多字节）
                    let start = self.i;
                    let len = utf8_len(c);
                    if len == 0 || self.i + len > self.b.len() {
                        return Err(());
                    }
                    let s = std::str::from_utf8(&self.b[start..start + len]).map_err(|_| ())?;
                    out.push_str(s);
                    self.i += len;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ()> {
        if self.i + 4 > self.b.len() {
            return Err(());
        }
        let mut v = 0u32;
        for k in 0..4 {
            let d = match self.b[self.i + k] {
                c @ b'0'..=b'9' => (c - b'0') as u32,
                c @ b'a'..=b'f' => (c - b'a' + 10) as u32,
                c @ b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return Err(()),
            };
            v = v * 16 + d;
        }
        self.i += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, ()> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits_start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == digits_start {
            return Err(());
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.i += 1;
            let fs = self.i;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == fs {
                return Err(());
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            let es = self.i;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == es {
                return Err(());
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| ())?;
        if !is_float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Json::Int(i));
            }
        }
        text.parse::<f64>().map(Json::Float).map_err(|_| ())
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_school_style_response() {
        // 学校的 Java 后端就是这么发中文的：\uXXXX 转义
        let raw = r#"{"code":"1","msg":"验证码不正确","data":{"token":"vt-1"}}"#;
        let v = parse(raw).unwrap();
        assert_eq!(v.text("code"), "1");
        assert_eq!(v.text("msg"), "验证码不正确");
        assert_eq!(v.object("data").unwrap().text("token"), "vt-1");
    }

    #[test]
    fn parses_loose_shapes() {
        let v = parse(r#"{"dataList":[{"teachingClassID": 123, "credit": "3.0"}, null]}"#).unwrap();
        let list = v.array("dataList");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].text("credit"), "3.0");
        // 原版 str(r.get("teachingClassID") or "").strip()
        assert_eq!(list[0].text("teachingClassID"), "123");
        assert!(list[1].get("x").is_none());
        assert_eq!(v.array("nope").len(), 0);
        assert!(v.object("nope").is_none());
    }

    #[test]
    fn surrogate_pairs() {
        let v = parse(r#""😀""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "😀");
    }

    #[test]
    fn rejects_junk_like_python() {
        assert!(parse("<html>502</html>").is_err());
        assert!(parse("{\"a\":1} trailing").is_err());
        assert!(parse("").is_err());
        assert!(parse_or_empty("<html>").is_empty_object());
    }

    #[test]
    fn serializes_like_python_ensure_ascii_false() {
        let v = Json::obj(vec![
            ("queryContent", Json::str("高等数学")),
            ("pageSize", Json::str("50")),
            ("n", Json::Int(-3)),
        ]);
        assert_eq!(
            v.to_compact(),
            r#"{"queryContent":"高等数学","pageSize":"50","n":-3}"#
        );
    }

    #[test]
    fn round_trip() {
        let raw = r#"{"a":[1,2,{"b":null}],"c":true,"d":-2.5}"#;
        let v = parse(raw).unwrap();
        assert_eq!(v.to_compact(), raw);
    }
}
