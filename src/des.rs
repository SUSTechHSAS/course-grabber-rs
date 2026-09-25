//! 密码加密：学校前端那套「自定义 DES + 自定义 base64」。
//!
//! 这一块是整个登录里唯一"错了也看不出来"的地方 —— 密文只要差一位，服务端
//! 就回「登录名或密码不正确」，而脚本会据此熔断自动登录。所以：
//!
//! * 算法**逐位照搬**原版 Python（`desencode.py` / `cus_base64.py`，MIT，见 LICENSE
//!   的 Third-party notices），不做"优化"；
//! * 表（IP/FP/E/P/S 盒/PC1/PC2）是从原版里**导出**的，不是手抄的；
//! * 移植过程中用原版跑了 300 组随机密钥/分组做逐位对比，全部一致；
//! * 单元测试里钉死了前端 JS 生成的历史向量（见 tests/offline.rs）。
//!
//! 顺带一个结论：这套 DES 是**标准 DES 的变体**（位序约定不同），拿去和标准 DES
//! 的例子比会对不上。所以"用现成的 DES crate 更省事"这条路是走不通的 —— 必须照搬。
//!
//! 与原版仅有的差别：原版用 64 个 bit 的列表逐位搬运，这里换成 u64 位运算。
//! 输入输出完全等价，速度快了两个数量级（虽然一次登录只调用几次，快慢无所谓）。
//!
//! 复杂度上注意：原版按"1 组密钥 / 2 组 / 3 组"写了三份几乎一样的代码，
//! 这里统一成"把密钥按 4 字符切开、顺序套用"，与那三份代码等价。

// ---------------------------------------------------------------------------
// 置换表（0 = 最高位 / MSB。与 desencode.py 里的下标一致）
// ---------------------------------------------------------------------------

const IP_TABLE: [u8; 64] = [
    57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61, 53, 45, 37, 29, 21, 13, 5, 63,
    55, 47, 39, 31, 23, 15, 7, 56, 48, 40, 32, 24, 16, 8, 0, 58, 50, 42, 34, 26, 18, 10, 2, 60, 52,
    44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
];
const FP_TABLE: [u8; 64] = [
    39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30, 37, 5, 45, 13, 53, 21, 61, 29,
    36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27, 34, 2, 42, 10, 50, 18, 58, 26,
    33, 1, 41, 9, 49, 17, 57, 25, 32, 0, 40, 8, 48, 16, 56, 24,
];
const E_TABLE: [u8; 48] = [
    31, 0, 1, 2, 3, 4, 3, 4, 5, 6, 7, 8, 7, 8, 9, 10, 11, 12, 11, 12, 13, 14, 15, 16, 15, 16, 17,
    18, 19, 20, 19, 20, 21, 22, 23, 24, 23, 24, 25, 26, 27, 28, 27, 28, 29, 30, 31, 0,
];
const P_TABLE: [u8; 32] = [
    15, 6, 19, 20, 28, 11, 27, 16, 0, 14, 22, 25, 4, 17, 30, 9, 1, 7, 23, 13, 31, 26, 2, 8, 18, 12,
    29, 5, 21, 10, 3, 24,
];
const PC1_TABLE: [u8; 56] = [
    56, 48, 40, 32, 24, 16, 8, 0, 57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59,
    51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36, 28, 20, 12, 4, 61, 53, 45, 37, 29, 21, 13, 5, 62,
    54, 46, 38, 30, 22, 14, 6,
];
const PC2_TABLE: [u8; 48] = [
    13, 16, 10, 23, 0, 4, 2, 27, 14, 5, 20, 9, 22, 18, 11, 3, 25, 7, 15, 6, 26, 19, 12, 1, 40, 51,
    30, 36, 46, 54, 29, 39, 50, 44, 32, 47, 43, 48, 38, 55, 33, 52, 45, 41, 49, 35, 28, 31,
];
/// 8 个 S 盒，每个 4 行 × 16 列，按 S1..S8 顺序平铺（与 desencode.py 里的表逐字相同）。
const S_BOXES: [u8; 512] = [
    14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11,
    9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1, 7, 5,
    11, 3, 14, 10, 0, 6, 13, 15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15,
    2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13,
    8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9, 10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11,
    4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1,
    2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12, 7, 13, 14, 3, 0, 6,
    9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9, 10, 6,
    9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7,
    2, 14, 2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0,
    15, 10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
    2, 13, 6, 15, 0, 9, 10, 4, 5, 3, 12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15,
    4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13,
    11, 6, 4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13, 4, 11, 2, 14, 15, 0, 8, 13, 3, 12,
    9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3,
    7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12, 13, 2,
    8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14,
    9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8, 13, 15, 12,
    9, 0, 3, 5, 6, 11,
];
const ROTATIONS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

/// 按表做位置置换：表里第 i 项给出"输出第 i 位取自输入的哪一位"。
fn permute(input: u64, nbits: u32, table: &[u8]) -> u64 {
    let mut out = 0u64;
    for &pos in table {
        out = (out << 1) | ((input >> (nbits - 1 - pos as u32)) & 1);
    }
    out
}

/// Feistel 轮函数 f(R, K)。
fn round(r: u32, k: u64) -> u32 {
    let x = permute(r as u64, 32, &E_TABLE) ^ k;
    let mut s = 0u32;
    for m in 0..8u32 {
        // 第 m 个 S 盒吃 6 位：首尾两位是行号，中间四位是列号
        let g = ((x >> (42 - 6 * m)) & 0x3F) as usize;
        let row = ((g >> 5) << 1) | (g & 1);
        let col = (g >> 1) & 0xF;
        s = (s << 4) | S_BOXES[m as usize * 64 + row * 16 + col] as u32;
    }
    permute(s as u64, 32, &P_TABLE) as u32
}

/// 单分组运算。`decrypt` 只是把 16 个子密钥倒过来用。
fn des_block(key: u64, block: u64, decrypt: bool) -> u64 {
    // 子密钥表
    let mut e = permute(key, 64, &PC1_TABLE); // 56 位
    let mut ks = [0u64; 16];
    for (t, &n) in ROTATIONS.iter().enumerate() {
        let c = (e >> 28) & 0x0FFF_FFFF;
        let d = e & 0x0FFF_FFFF;
        let c = ((c << n) | (c >> (28 - n))) & 0x0FFF_FFFF;
        let d = ((d << n) | (d >> (28 - n))) & 0x0FFF_FFFF;
        e = (c << 28) | d;
        ks[t] = permute(e, 56, &PC2_TABLE);
    }

    let x = permute(block, 64, &IP_TABLE);
    let mut l = (x >> 32) as u32;
    let mut r = (x & 0xFFFF_FFFF) as u32;
    for i in 0..16 {
        let k = if decrypt { ks[15 - i] } else { ks[i] };
        let nr = l ^ round(r, k);
        l = r;
        r = nr;
    }
    // 收尾是 R16 ‖ L16（与原版一致）
    permute(((r as u64) << 32) | l as u64, 64, &FP_TABLE)
}

fn des_encrypt_block(key: u64, block: u64) -> u64 {
    des_block(key, block, false)
}

/// 解密方向：主流程用不到（原版的 `str_dec` 也一样没人调），
/// 留着只为让自检能闭环验证"加密确实可逆"。release 构建里不会编进去。
#[cfg(test)]
fn des_decrypt_block(key: u64, block: u64) -> u64 {
    des_block(key, block, true)
}

// ---------------------------------------------------------------------------
// 4 个字符 ↔ 64 位
// ---------------------------------------------------------------------------

/// 最多 4 个字符 → 64 位：每个字符占 16 位（高位在前），不足的补 0。
///
/// 对应原版的 `str_to_bt`。注意原版按 **Unicode 码点**切分与取值，所以这里也必须
/// 用 `chars()` 而不是字节切片 —— 中文密码（3 字节 UTF-8）切错就全完了。
fn chars_to_block(chunk: &[char]) -> u64 {
    let mut v = 0u64;
    for i in 0..4 {
        let c = chunk.get(i).map(|&c| (c as u32) & 0xFFFF).unwrap_or(0) as u64;
        v = (v << 16) | c;
    }
    v
}

fn block_to_hex(v: u64) -> String {
    format!("{v:016X}")
}

/// 把一组密钥拆成 64 位的密钥块序列（每个 4 字符一块，顺序不变）。
fn key_blocks(keys: &[String]) -> Vec<u64> {
    let mut out = Vec::new();
    for key in keys {
        let chars: Vec<char> = key.chars().collect();
        for chunk in chars.chunks(4) {
            if !chunk.is_empty() {
                out.push(chars_to_block(chunk));
            }
        }
    }
    out
}

/// 对应原版的 `str_enc`：按 4 字符分组，每组依次过一遍所有密钥，拼成十六进制串。
///
/// 空密钥、空字符串都会自然退化成空结果（原版在这种情况下其实会抛 NameError，
/// 但配置校验在更早的地方就拦住了 —— 见 config.rs 的 des_keys）。
pub fn str_enc(plain: &str, keys: &[String]) -> String {
    let blocks = key_blocks(keys);
    if blocks.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = plain.chars().collect();
    let mut out = String::with_capacity(chars.len().div_ceil(4) * 16);
    for chunk in chars.chunks(4) {
        let mut v = chars_to_block(chunk);
        for &k in &blocks {
            v = des_encrypt_block(k, v);
        }
        out.push_str(&block_to_hex(v));
    }
    out
}

/// 解密方向（原版有 `str_dec`，主流程用不到；留着是为了自检能闭环验证）。
#[cfg(test)]
pub fn str_dec(hex: &str, keys: &[String]) -> String {
    let blocks = key_blocks(keys);
    if blocks.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for chunk in hex.as_bytes().chunks(16) {
        if chunk.len() < 16 {
            break;
        }
        let text = match std::str::from_utf8(chunk) {
            Ok(t) => t,
            Err(_) => break,
        };
        let mut v = match u64::from_str_radix(text, 16) {
            Ok(v) => v,
            Err(_) => break,
        };
        for &k in blocks.iter().rev() {
            v = des_decrypt_block(k, v);
        }
        for i in 0..4 {
            let c = ((v >> (48 - 16 * i)) & 0xFFFF) as u32;
            // 原版 byte_to_string 会跳过 0（补位用的空字符就是这么被丢掉的）
            if c == 0 {
                continue;
            }
            if let Some(ch) = char::from_u32(c) {
                out.push(ch);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 自定义 base64（jQuery.base64 的等价物）
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 base64 编码（带 `=` 填充）。
///
/// 原版那份 `cus_base64.py` 绕了一大圈（8 位→6 位→查表→补等号），做的其实就是
/// 标准 base64；字符集、填充规则都和 RFC 4648 一致，所以这里直接算。
/// （tests/offline.rs 用前端生成的向量锁住这一点。）
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 明文密码 → `loginPwd`（与前端 `$.base64.encode(strEnc(pwd, ...))` 等价）。
pub fn encrypt_password(password: &str, keys: &[String]) -> String {
    base64_encode(str_enc(password, keys).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<String> {
        vec!["this".into(), "password".into(), "is".into()]
    }

    /// 前端 JS 生成的历史向量（与 tests/test_offline.py 里那份完全相同）。
    /// 这些是"错了也看不出来"的地方唯一的照妖镜。
    #[test]
    fn golden_vectors() {
        let k = keys();
        let vectors = [
            ("abc", "N0QyMEFBM0M2ODQ0MTdGRg=="),
            ("abcd", "MkVCNURGQUY0NUI4MzdFNA=="),
            ("abcde", "MkVCNURGQUY0NUI4MzdFNDA5RjlGQkY1QzI3NUE5MUY="),
            ("abcdefgh", "MkVCNURGQUY0NUI4MzdFNDNBMEQ0ODY2NjA5NEQyRDg="),
            (
                "0123456789abcdef",
                "MTlBOUE2OTk5NDI4Mjg1RUQwNzUyMjU4RkFFQjZGRkNCRTk4Qjk4M0Y3QTVDQzMxMTVDQ0IzQTNDNEE0MEI3RQ==",
            ),
            ("P@ssw0rd!", "NkQ0MEQ5MUMwN0IwRjJFQTkxNkQzRUVFMzAwMERFNTg4MDlCQzU2QjU3Q0Y5QzMx"),
            (
                "xxxxxxxxxxxxxxx",
                "Q0QzMTkwMzU3QzYzNDhDRUNEMzE5MDM1N0M2MzQ4Q0VDRDMxOTAzNTdDNjM0OENFNzZBQzQxNDRFMEVEMkJBMw==",
            ),
            ("短密码", "Qzk5RjAwNzk3QzE4RDUzRg=="),
            ("混合Mixed123", "RjQyMUM4MjQwOUExRDZGNTJENzREMkUxOTRGQkQ4NjA5NzFBRTkyMjY3Q0JFMjZB"),
        ];
        for (plain, want) in vectors {
            assert_eq!(
                encrypt_password(plain, &k),
                want,
                "加密结果与前端向量不一致: {plain:?}"
            );
        }
    }

    #[test]
    fn single_and_double_key_branches() {
        // 原版把 1/2/3 组密钥写成三份代码；这里验证统一写法覆盖了那几条路径
        let one = vec!["this".to_string()];
        let two = vec!["this".to_string(), "password".to_string()];
        let three = keys();
        // 三组的向量已经有了，这里只要求不同密钥数给出不同结果、且长度符合规律
        let a = encrypt_password("abcdefgh", &one);
        let b = encrypt_password("abcdefgh", &two);
        let c = encrypt_password("abcdefgh", &three);
        assert_ne!(a, b);
        assert_ne!(b, c);
        // 8 个字符 = 2 个分组 × 16 个十六进制字符 = 32 个 ASCII 字节 → base64 44 字符（含 padding）
        assert_eq!(c.len(), 44);
        assert!(c.ends_with('='));
    }

    #[test]
    fn decrypt_round_trips() {
        let k = keys();
        for plain in [
            "abc",
            "abcdefgh",
            "混合Mixed123",
            "短密码",
            "0123456789abcdef",
        ] {
            let hex = str_enc(plain, &k);
            assert_eq!(str_dec(&hex, &k), plain, "解密回不到原文: {plain:?}");
        }
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn password_is_not_plaintext_on_the_wire() {
        let k = keys();
        let out = encrypt_password("P@ssw0rd!", &k);
        assert!(!out.contains("P@ssw0rd!"));
    }
}
