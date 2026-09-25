//! 验证码识别：JPEG 字节 → 4 个点击坐标 + 置信度。
//!
//! 识别本身在姊妹仓库 [click-captcha-matcher-rs]（`no_std`、只依赖 libc、模型 124 KB
//! 编在库里）里实现，这里是**编译期**的绑定 —— 不再是原版的
//! `python/solver.py` + `ctypes` 加载 `libccm.so`，也就不存在"找不到动态库"
//! "模型目录指错"这类运行时故障了。
//!
//! 原版那个 `--captcha-model-dir` / `--captcha-model` 的意义也跟着变了：
//! 默认用编在二进制里的模型，`--captcha-model` 仍可指一个外部的 `.ccm` 文件。
//!
//! [click-captcha-matcher-rs]: https://github.com/SUSTechHSAS/click-captcha-matcher-rs

use ccm::{Model, Solver as CcmSolver, EMBEDDED_MODEL};

/// 识别失败（换一张图重试即可）。
#[derive(Debug)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 识别器接口。
///
/// 抽这一层是为了**自检能注入桩**：假学校服务端只能喂假图（`\xff\xd8\xff` 开头的
/// 几个字节），真模型当然解不出来；原版 `tests/test_offline.py` 也是这么做的
/// （StubSolver）。另外它让 `Login`/`ReloginManager` 不必依赖 ccm 的具体类型。
pub trait Solver: Send + Sync {
    /// JPEG → (4 个点击坐标, 置信度)。解不出合法结果时返回 [`Rejected`]（换图重试）。
    fn solve(&self, jpeg: &[u8]) -> Result<([[i32; 2]; 4], f32), Rejected>;
    /// 低于这个置信度就换一张图，而不是提交
    fn min_margin(&self) -> f32;
    /// 打印给用户看的一句话（模型来自哪里）
    fn describe(&self) -> String;
}

pub struct Captcha {
    solver: CcmSolver,
    /// 模型来自哪里（打印给用户看）
    pub model_name: String,
    pub min_margin: f32,
    width: i64,
    height: i64,
}

impl Captcha {
    /// `model_file` 给 None 就用编在二进制里的 w16。
    pub fn new(
        model_file: Option<&str>,
        min_margin: f64,
        width: i64,
        height: i64,
    ) -> Result<Captcha, String> {
        let (model, name) = match model_file {
            Some(path) if !path.is_empty() => {
                let bytes =
                    std::fs::read(path).map_err(|e| format!("读不到识别模型 {path}: {e}"))?;
                let model =
                    Model::load(&bytes).map_err(|_| format!("识别模型文件格式不对: {path}"))?;
                (model, path.to_string())
            }
            _ => {
                let model = Model::load(EMBEDDED_MODEL)
                    .map_err(|_| "内编的识别模型加载失败（不该发生，请报 bug）".to_string())?;
                (model, "内编 w16".to_string())
            }
        };
        Ok(Captcha {
            solver: CcmSolver::new(model),
            model_name: name,
            min_margin: min_margin as f32,
            width,
            height,
        })
    }

    /// 前端 `verifyResult` 的序列化格式：`left-top,left-top,...`
    pub fn to_verify_code(points: &[[i32; 2]; 4]) -> String {
        verify_code(points)
    }
}

impl Solver for Captcha {
    fn min_margin(&self) -> f32 {
        self.min_margin
    }

    fn describe(&self) -> String {
        self.model_name.clone()
    }

    fn solve(&self, jpeg: &[u8]) -> Result<([[i32; 2]; 4], f32), Rejected> {
        let solution = self
            .solver
            .solve(jpeg)
            .map_err(|e| Rejected(format!("验证码图片无法解析: {e:?}")))?;
        let points = solution.points;
        for p in &points {
            let (x, y) = (p[0] as i64, p[1] as i64);
            if !(0..=self.width).contains(&x) || !(0..=self.height).contains(&y) {
                return Err(Rejected(format!("点击坐标越界: {x},{y}")));
            }
        }
        // 4 个点必须互不重合（原版也是这么查的）
        for i in 0..4 {
            for j in (i + 1)..4 {
                if points[i] == points[j] {
                    return Err(Rejected("识别出的 4 个点有重合".to_string()));
                }
            }
        }
        Ok((points, solution.margin))
    }
}

/// 前端 `verifyResult` 的序列化格式：`left-top,left-top,...`
pub fn verify_code(points: &[[i32; 2]; 4]) -> String {
    points
        .iter()
        .map(|p| format!("{}-{}", p[0], p[1]))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_model_loads() {
        let c = Captcha::new(None, 0.0, 250, 80).unwrap();
        assert_eq!(c.model_name, "内编 w16");
    }

    #[test]
    fn missing_model_file_is_a_clear_error() {
        let err = Captcha::new(Some("/nonexistent/x.ccm"), 0.0, 250, 80)
            .err()
            .unwrap();
        assert!(err.contains("读不到识别模型"), "{err}");
    }

    #[test]
    fn garbage_model_file_is_rejected() {
        let dir = std::env::temp_dir().join(format!("cg-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.ccm");
        std::fs::write(&path, b"not a model").unwrap();
        let err = Captcha::new(Some(path.to_str().unwrap()), 0.0, 250, 80)
            .err()
            .unwrap();
        assert!(err.contains("格式不对"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_non_jpeg_input() {
        let c = Captcha::new(None, 0.0, 250, 80).unwrap();
        assert!(c.solve(b"not a jpeg at all").is_err());
        assert!(c.solve(b"").is_err());
    }

    #[test]
    fn verify_code_format() {
        let points = [[1, 2], [3, 4], [5, 6], [7, 8]];
        assert_eq!(Captcha::to_verify_code(&points), "1-2,3-4,5-6,7-8");
    }
}
