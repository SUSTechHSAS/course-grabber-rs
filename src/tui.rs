//! `course-grabber tui` —— 交互式配置编辑器。
//!
//! 为什么要有它：config.json 里有四十来个键，其中十个接口路径、三组加密密钥、
//! 若干候选教学班，手写 JSON 很容易在逗号和引号上翻车，而配置写错的代价是
//! **放课那一刻打不出去**。这个界面让你在一个表单里上下走、按 Enter 改，
//! 底部随时告诉你"还差什么"，保存前还能先校验一遍。
//!
//! 第一次进来（或按 `w`）会直接进「配置向导」：它一步一步问 —— 学校地址 → 学号 → 密码 →
//! 课程名，每一步都把结果显示出来。地址那一步**只写域名也行**：程序会去问服务器要页面路径
//! （实测 `GET /` 会 302 到应用页），再拿真实的只读接口验证推导出来的前缀对不对。
//! 开荒那部分逻辑在 `onboard.rs`：URL 推导、自动发现、只读自检、拉候选教学班。
//!
//! 三条设计原则：
//!
//! 1. **原样保留**（round-trip）：界面只改你动过的那个键，`_comment` 说明、自定义键、
//!    旧配置里的陌生字段全部原样写回去。所以拿老配置进来编辑是安全的。
//! 2. **不猜**：显示的是**这份文件里实际写了什么**。没写的键显示成灰字 `（默认 …）`，
//!    告诉你"程序现在用的是这个值"，而不是替你写进文件。
//! 3. **保存 ≠ 生效**：不动磁盘直到你按 `s`；校验（`v`）只看不改。

use std::path::{Path as FsPath, PathBuf};
use std::sync::mpsc::{self, Receiver};

use crate::auth::Credentials;
use crate::captcha::Solver;
use crate::config::{self, Config};
use crate::json::{self, Json};
use crate::onboard::{self, Level, Note};
use crate::term::{self, Key};
use crate::textw;

// ---------------------------------------------------------------------------
// 样式
// ---------------------------------------------------------------------------

const S_BOLD: &str = "1";
const S_DIM: &str = "2";
const S_SEL: &str = "7";
const S_TITLE: &str = "1;36";
const S_ADD: &str = "1;33";
const S_OK: &str = "32";
const S_WARN: &str = "33";
const S_ERR: &str = "1;31";
const S_KEY: &str = "36";

/// 左栏（段）标题。顺序 = 键位 1..9,0。
const SEC_TITLES: [&str; 10] = [
    "学校",
    "凭据",
    "接口路径",
    "Cookie 名",
    "密码加密",
    "目标课程",
    "验证码",
    "节奏",
    "网络",
    "其它",
];

const LEFT_W: usize = 14;
const LABEL_W: usize = 30;

/// 后台任务那一屏的转轮（只有一个字符宽，所以用盲文点阵那套）。
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn level_style(l: Level) -> &'static str {
    match l {
        Level::Ok => S_OK,
        Level::Warn => S_WARN,
        Level::Err => S_ERR,
    }
}

// ---------------------------------------------------------------------------
// 可编辑位置：一条 JSON 路径
// ---------------------------------------------------------------------------

/// 界面只认这几种形状的路径 —— 正好覆盖 config.json 的全部结构。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Path {
    /// 顶层标量：`timezone_offset_hours`
    Top(String),
    /// `school.host`
    Sec(String, String),
    /// `cookies.session[2]`（元素本身是字符串）
    Item(String, String, usize),
    /// `course.candidates[1].group`（老格式，只有一门课时仍然用它）
    ItemKey(String, String, usize, String),
    /// `courses[i].<key>` —— 多课程：第 i 门课的某个字段
    Course(usize, &'static str),
    /// `courses[i].candidates[j].<key>`
    CourseCand(usize, usize, &'static str),
}

impl Path {
    fn sec(key: &str, sub: &str) -> Path {
        Path::Sec(key.to_string(), sub.to_string())
    }

    fn top(key: &str) -> Path {
        Path::Top(key.to_string())
    }

    fn get<'a>(&self, root: &'a Json) -> Option<&'a Json> {
        match self {
            Path::Top(k) => root.get(k),
            Path::Sec(s, k) => root.object(s)?.get(k),
            Path::Item(s, l, i) => root.object(s)?.array(l).get(*i),
            Path::ItemKey(s, l, i, k) => root.object(s)?.array(l).get(*i)?.get(k),
            Path::Course(i, k) => root.array("courses").get(*i)?.get(k),
            Path::CourseCand(i, j, k) => root
                .array("courses")
                .get(*i)?
                .array("candidates")
                .get(*j)?
                .get(k),
        }
    }

    fn set(&self, root: &mut Json, val: Json) {
        match self {
            Path::Top(k) => root.set_key(k, val),
            Path::Sec(s, k) => root.ensure_obj(s).set_key(k, val),
            Path::Item(s, l, i) => {
                if let Some(slot) = root.ensure_obj(s).ensure_arr(l).get_mut(*i) {
                    *slot = val;
                }
            }
            Path::ItemKey(s, l, i, k) => {
                if let Some(slot) = root.ensure_obj(s).ensure_arr(l).get_mut(*i) {
                    slot.set_key(k, val);
                }
            }
            Path::Course(i, k) => {
                if let Some(slot) = root.ensure_arr("courses").get_mut(*i) {
                    slot.set_key(k, val);
                }
            }
            Path::CourseCand(i, j, k) => {
                if let Some(slot) = root.ensure_arr("courses").get_mut(*i) {
                    if let Some(c) = slot.ensure_arr("candidates").get_mut(*j) {
                        c.set_key(k, val);
                    }
                }
            }
        }
    }

    /// 这一行属于哪个列表（`cookies.session` 这种），是第几项。
    fn list_pos(&self) -> Option<(String, String, usize)> {
        match self {
            Path::Item(s, l, i) | Path::ItemKey(s, l, i, _) => Some((s.clone(), l.clone(), *i)),
            _ => None,
        }
    }

    /// 这一行属于哪门课的候选列表（多课程时用来做"删/加"）。
    fn course_cand_pos(&self) -> Option<(usize, usize)> {
        match self {
            Path::CourseCand(i, j, _) => Some((*i, *j)),
            _ => None,
        }
    }

    fn indent(&self) -> usize {
        if self.list_pos().is_some() || self.course_cand_pos().is_some() {
            1
        } else {
            0
        }
    }
}

/// 改的是哪份文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Config,
    Cred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Text,
    Int,
    Float,
}

// ---------------------------------------------------------------------------
// 一行
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Field {
    target: Target,
    path: Path,
    ty: Ty,
    secret: bool,
}

/// 一行按 Enter 时做的事（不是填值，而是"跑一个动作"）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowAct {
    /// 往某个列表里追加一项
    Add { sec: String, list: String },
    /// 切到下一门目标课程
    NextCourse,
    /// 加一门新的目标课程
    AddCourse,
    /// 删掉当前这门课（至少留一门）
    DelCourse,
    /// 往"第 i 门课"的候选列表里追加一项
    AddCourseCand(usize),
    /// 进向导的"学校地址"这一步（粘网址 / 光域名都行）
    WizardUrl,
    /// 从第一步开始的配置向导
    Wizard,
    /// 只读探测一遍接口路径
    Probe,
    /// 用凭据登录，把课程目录里的教学班拉下来
    FetchCandidates,
}

#[derive(Debug, Clone)]
struct Row {
    label: String,
    value: String,
    /// 值是"程序当前的默认值"而不是文件里写着的 → 灰字显示
    dim: bool,
    indent: usize,
    field: Option<Field>,
    act: Option<RowAct>,
}

fn row_path(target: Target, path: Path, label: &str, ty: Ty) -> Row {
    Row {
        label: label.to_string(),
        value: String::new(),
        dim: false,
        indent: path.indent(),
        field: Some(Field {
            target,
            path,
            ty,
            secret: false,
        }),
        act: None,
    }
}

fn row_field(target: Target, sec: &str, key: &str, label: &str, ty: Ty) -> Row {
    row_path(target, Path::sec(sec, key), label, ty)
}

fn row_top(target: Target, key: &str, label: &str, ty: Ty, secret: bool) -> Row {
    let mut r = row_path(target, Path::top(key), label, ty);
    if let Some(f) = &mut r.field {
        f.secret = secret;
    }
    r
}

fn row_item(
    target: Target,
    sec: &str,
    list: &str,
    idx: usize,
    sub: Option<&str>,
    label: String,
    ty: Ty,
) -> Row {
    let path = match sub {
        Some(k) => Path::ItemKey(sec.to_string(), list.to_string(), idx, k.to_string()),
        None => Path::Item(sec.to_string(), list.to_string(), idx),
    };
    row_path(target, path, &label, ty)
}

fn row_add(sec: &str, list: &str, label: &str) -> Row {
    row_act(
        RowAct::Add {
            sec: sec.to_string(),
            list: list.to_string(),
        },
        label,
    )
}

/// 一个"动作行"：按 Enter 就跑，不需要填值。
fn row_act(act: RowAct, label: &str) -> Row {
    Row {
        label: label.to_string(),
        value: String::new(),
        dim: false,
        indent: 0,
        field: None,
        act: Some(act),
    }
}

fn row_note(label: &str, text: &str) -> Row {
    Row {
        label: label.to_string(),
        value: text.to_string(),
        dim: true,
        indent: 1,
        field: None,
        act: None,
    }
}

/// 列表的形状：元素是字符串，还是"含这几个键的对象"。空切片 = 字符串列表。
fn list_fields(sec: &str, list: &str) -> &'static [&'static str] {
    match (sec, list) {
        ("course", "candidates") => &["id", "label", "group"],
        _ => &[],
    }
}

/// 列表容量上限（`None` = 不限）。目前只有密码密钥有硬上限。
fn list_cap(sec: &str, list: &str) -> Option<usize> {
    match (sec, list) {
        ("password", "des_keys") => Some(3),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 输入行
// ---------------------------------------------------------------------------

/// 单行输入。用 `Vec<char>` 而不是 String：光标的"第几个字符"在中文下必须按字符算，
/// 按字节切片是灾难。
#[derive(Debug, Clone)]
struct Input {
    chars: Vec<char>,
    pos: usize,
}

impl Input {
    fn new(s: &str) -> Input {
        let chars: Vec<char> = s.chars().collect();
        Input {
            pos: chars.len(),
            chars,
        }
    }

    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars() {
            // 单行输入：换行和制表符直接丢掉（多行粘贴时会碰到）
            if c == '\n' || c == '\r' || c == '\t' {
                continue;
            }
            self.chars.insert(self.pos, c);
            self.pos += 1;
        }
    }

    fn backspace(&mut self) {
        if self.pos > 0 {
            self.pos -= 1;
            self.chars.remove(self.pos);
        }
    }

    fn delete(&mut self) {
        if self.pos < self.chars.len() {
            self.chars.remove(self.pos);
        }
    }

    fn left(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.pos < self.chars.len() {
            self.pos += 1;
        }
    }

    fn kill_to_end(&mut self) {
        self.chars.truncate(self.pos);
    }

    fn kill_to_start(&mut self) {
        self.chars.drain(..self.pos);
        self.pos = 0;
    }

    fn kill_word(&mut self) {
        while self.pos > 0 && self.chars[self.pos - 1].is_whitespace() {
            self.backspace();
        }
        while self.pos > 0 && !self.chars[self.pos - 1].is_whitespace() {
            self.backspace();
        }
    }

    /// 横向滚动后的可见片段，以及光标在片段里的列偏移。
    fn window(&self, avail: usize) -> (String, usize) {
        if avail == 0 {
            return (String::new(), 0);
        }
        // 光标右边留一列：否则光标会贴在右边界外
        let budget = avail.saturating_sub(1);
        let mut start = self.pos;
        let mut used = 0usize;
        while start > 0 {
            let w = textw::char_width(self.chars[start - 1]);
            if used + w > budget {
                break;
            }
            used += w;
            start -= 1;
        }
        let mut shown = String::new();
        let mut cols = 0usize;
        for c in &self.chars[start..] {
            let w = textw::char_width(*c);
            if cols + w > avail {
                break;
            }
            shown.push(*c);
            cols += w;
        }
        (shown, used)
    }
}

// ---------------------------------------------------------------------------
// 界面
// ---------------------------------------------------------------------------

struct Edit {
    target: Target,
    path: Path,
    label: String,
    ty: Ty,
    secret: bool,
    input: Input,
}

enum Act {
    Quit,
    Reload,
    /// 确认之后去联网探测
    RunProbe,
    /// 确认之后去登录 + 拉课程目录
    RunFetch,
}

/// 一页静态/半静态的长文本。
#[derive(Debug, Clone, PartialEq, Eq)]
enum InfoPage {
    /// 按键说明（静态）
    Help,
    /// 刚做完一件事的结果清单（自检报告、拉取日志）
    Notes(Vec<Note>),
    /// 校验结果（现算的）
    Problems,
}

// ---------------------------------------------------------------------------
// 向导：一步一步问，问完就配好了
// ---------------------------------------------------------------------------

/// 向导的四步。顺序就是用户的心智顺序：先告诉它学校在哪，再给身份，再挑课。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WStep {
    /// 选课页网址（或光域名）
    Url,
    /// 学号
    Student,
    /// 密码
    Password,
    /// 拉目录 → 挑课 → 写进配置
    PickCourse,
    /// 收尾：确认保存
    Done,
}

struct Wizard {
    step: WStep,
    input: Input,
    /// 输入框里是预填的旧值：第一次打字/粘贴时整段替换掉（不然输入会变成"追加"，
    /// 实测很容易得到「高等数学线性代数」这种怪东西）
    replace_on_type: bool,
    /// 上一步的结果（贴在界面上，让用户看得见"凭什么这么填"）
    notes: Vec<Note>,
    scroll: usize,
    err: String,
    /// 自动发现拿到的结果（里面有实测结论）
    discovered: Option<Box<onboard::Discovered>>,
    student: String,
    password: String,
    keyword: String,
    /// 拉回来的候选数（收尾页显示）
    picked: usize,
}

/// 后台任务跑完之后回到哪儿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobNext {
    /// 直接把结果当一页报告显示
    Info,
    /// 向导第 1 步：自动发现学校
    WizardDiscover,
    /// 向导第 4 步：登录拉课程目录
    WizardFetch,
}

enum Mode {
    Browse,
    Edit(Box<Edit>),
    Info {
        page: InfoPage,
        title: String,
        scroll: usize,
    },
    Confirm {
        prompt: String,
        act: Act,
    },
    /// 后台任务进行中：边跑边把日志显示出来
    Job(Box<Job>),
    /// 勾选拉回来的教学班
    Pick(Box<Pick>),
    /// 一步一步问的配置向导
    Wizard(Box<Wizard>),
}

/// 一个后台任务（联网的事都放这里，界面不卡）。
struct Job {
    title: String,
    rx: Receiver<JobMsg>,
    lines: Vec<Note>,
    spin: usize,
    /// 跑完之后把界面交给谁
    next: JobNext,
}

enum JobMsg {
    Log(String),
    /// Box 一下：JobDone 里装着整份拉取结果，不装盒子时这个枚举会胀到几百字节
    Done(Box<JobDone>),
}

enum JobDone {
    Probe(Vec<Note>),
    Fetch(Result<onboard::Fetched, String>),
    Discover(Result<onboard::Discovered, String>),
}

/// 挑课屏：**整类课都在这儿**，类型可见、可筛、可勾。
///
/// 为什么不"先打关键词再搜"：同一门课在方案内 / 方案外 / 校公选下都可能开，
/// 名字还一模一样 —— 只让用户打名字，他既分不清是哪一类，也看不到自己漏了什么。
struct Pick {
    rows: Vec<onboard::Found>,
    /// 过完"类型开关 + 筛选词"之后剩下的行下标（翻页、光标都走它）
    shown: Vec<usize>,
    /// 按 `rows` 的下标记勾选（换筛选词不会把勾掉）
    checked: Vec<bool>,
    /// 光标在 `shown` 里的位置
    cur: usize,
    top: usize,
    /// 筛选词：课程名 / 教师 / 教学班号 / 类型名都能搜
    filter: String,
    /// 正在输入筛选词（`/` 进，Enter/Esc 出）
    typing: bool,
    /// 每一类是否显示（与 `COURSE_TYPES` 对齐，数字键 1..N 切换）
    type_on: Vec<bool>,
    student: String,
    batch_name: String,
    campus: String,
    /// 顺路办成的事（比如"服务器下发的 Cookie 名比配置里全，已经补上"）
    note: String,
    /// 上一次回车没成功的提示（一个都没勾）—— 留在这一屏，不来回折腾
    warn: String,
    /// 是向导里进来的：Enter 之后回向导，而不是回编辑器
    from_wizard: bool,
}

impl Pick {
    fn new(fetched: onboard::Fetched, checked: Vec<bool>, from_wizard: bool, note: String) -> Pick {
        let mut p = Pick {
            rows: fetched.rows,
            shown: Vec::new(),
            checked,
            cur: 0,
            top: 0,
            filter: String::new(),
            typing: false,
            type_on: vec![true; onboard::COURSE_TYPES.len()],
            student: fetched.student_name,
            batch_name: fetched.batch_name,
            campus: fetched.campus,
            note,
            warn: String::new(),
            from_wizard,
        };
        p.rebuild();
        p
    }

    /// 重算"看得见的那些行"。
    fn rebuild(&mut self) {
        let filter = self.filter.trim().to_lowercase();
        let visible: Vec<bool> = self.type_on.clone();
        self.shown = (0..self.rows.len())
            .filter(|i| {
                let r = &self.rows[*i];
                self.type_index(&r.tc_type)
                    .map(|ti| visible.get(ti).copied().unwrap_or(true))
                    .unwrap_or(true)
                    && r.matches(&filter)
            })
            .collect();
        if self.cur >= self.shown.len() {
            self.cur = self.shown.len().saturating_sub(1);
        }
        if self.top > self.cur {
            self.top = 0;
        }
    }

    fn type_index(&self, code: &str) -> Option<usize> {
        onboard::COURSE_TYPES
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case(code.trim()))
    }

    fn cur_row(&self) -> Option<usize> {
        self.shown.get(self.cur).copied()
    }

    fn toggle(&mut self) {
        if let Some(i) = self.cur_row() {
            self.checked[i] = !self.checked[i];
        }
    }

    fn checked_count(&self) -> usize {
        self.checked.iter().filter(|c| **c).count()
    }

    fn picked(&self) -> Vec<&onboard::Found> {
        self.rows
            .iter()
            .zip(self.checked.iter())
            .filter(|(_, c)| **c)
            .map(|(r, _)| r)
            .collect()
    }

    /// 页面大小（`body_h` 里前两行是汇总与筛选行）。
    fn view(&self, body_h: usize) -> usize {
        body_h.saturating_sub(2).max(1)
    }

    fn clamp(&mut self, body_h: usize) {
        let view = self.view(body_h);
        if self.cur >= self.shown.len() {
            self.cur = self.shown.len().saturating_sub(1);
        }
        if self.cur < self.top {
            self.top = self.cur;
        }
        if self.cur >= self.top + view {
            self.top = self.cur + 1 - view;
        }
    }
}

pub struct Opts {
    pub config: Option<String>,
    pub credentials: Option<String>,
}

pub fn run(opts: Opts) -> Result<u8, String> {
    let mut app = App::load(opts)?;
    let mut t = term::enter()?;
    let mut drawn: (usize, usize) = (0, 0);
    loop {
        // 只在"有按键"或"窗口大小变了"的时候重画：空闲时一个字节都不往终端写
        // （挂在 SSH 上跑的时候，每 400ms 刷一屏是很浪费的）
        let sz = term::size();
        if sz != drawn {
            app.render(&mut t, sz);
            drawn = sz;
        }
        match t.next_key(400) {
            None => {
                // 超时：看看后台任务有没有新消息（有的话重画，顺便转一下转轮）
                if app.poll_job() {
                    app.render(&mut t, sz);
                    drawn = sz;
                }
            }
            Some(Key::Eof) => break,
            Some(k) => {
                if app.on_key(k, sz) {
                    break;
                }
                let sz = term::size();
                app.render(&mut t, sz);
                drawn = sz;
            }
        }
    }
    Ok(0)
}

struct App {
    path: PathBuf,
    raw: Json,
    defaults: Json,
    dirty: bool,
    /// 这份配置是新造出来的（文件还不存在）—— 标题栏上说清楚
    fresh: bool,

    cred_path: PathBuf,
    cred: Json,
    /// 凭据文件里实际用的两个键名（与 auth.rs 的别名表同一份）
    cred_keys: (String, String),
    /// 凭据文件是不是 `{"credentials": {...}}` 那种包了一层的写法
    cred_wrap: bool,
    cred_exists: bool,
    cred_dirty: bool,

    /// 向导状态：跑后台任务 / 勾选候选期间，向导本体先寄存在这儿（那时 Mode 是 Job/Pick）
    wiz: Option<Box<Wizard>>,
    sec: usize,
    cur: usize,
    /// 目标课程那一节正在编辑第几门课（`courses[cur_course]`）
    cur_course: usize,
    top: usize,
    mode: Mode,
    msg: (String, Level),
    quit: bool,
    color: bool,
}

impl App {
    fn load(opts: Opts) -> Result<App, String> {
        let explicit = opts.config.as_deref().map(FsPath::new);
        // 走的是配置自己的查找顺序（`~` 也在这里展开），
        // 于是"界面改的这份"和"程序跑起来读的那份"一定是同一个文件
        let candidates = config::config_candidates(explicit);
        let found = candidates.iter().find(|p| p.is_file()).cloned();

        let (path, mut raw, fresh) = match (explicit, found) {
            (Some(_), Some(p)) => {
                let text = read_text(&p)?;
                let raw = parse_config(&text, &p)?;
                (p, raw, false)
            }
            // --config 指了一个还不存在的文件：按模板新造（这是"给我建一份"的意思）
            (Some(_), None) => (
                candidates
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| PathBuf::from("config.json")),
                json::parse_or_empty(config::CONFIG_EXAMPLE),
                true,
            ),
            (None, Some(p)) => {
                let text = read_text(&p)?;
                let raw = parse_config(&text, &p)?;
                (p, raw, false)
            }
            // 一条都没有：按模板新造一份，落在**当前目录**（README 里那句
            // `cp config.example.json config.json` 的位置）。标题栏会写明路径。
            (None, None) => {
                let p = std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("config.json");
                (p, json::parse_or_empty(config::CONFIG_EXAMPLE), true)
            }
        };

        // 目标课程统一成 `courses` 数组（老配置只写单个 `course` 也照读）。
        // **界面只认 `courses`** —— 不然老配置进来是一张空表单，保存时又把 `course`
        // 原样写回去，用户以为改好了，而程序读的是 `courses`。
        config::normalize_courses(&mut raw);

        let mut app = App {
            path,
            raw,
            defaults: config::engine_defaults(),
            dirty: false,
            fresh,
            cred_path: PathBuf::new(),
            cred: Json::Obj(Vec::new()),
            cred_keys: ("student_id".into(), "password".into()),
            cred_wrap: false,
            cred_exists: false,
            cred_dirty: false,
            wiz: None,
            sec: 0,
            cur: 0,
            cur_course: 0,
            top: 0,
            mode: Mode::Browse,
            msg: (String::new(), Level::Ok),
            quit: false,
            color: use_color(),
        };
        app.reload_credentials(opts.credentials.as_deref());
        app.msg = if app.fresh {
            (
                format!("新配置：按模板起步，填好按 s 保存到 {}", app.path.display()),
                Level::Warn,
            )
        } else {
            (format!("已载入 {}", app.path.display()), Level::Ok)
        };
        // 学校那一节还没配好：直接进配置向导，别让人对着一屏灰字发呆
        if app.needs_setup() {
            app.sec = 0;
            app.start_wizard(WStep::Url);
            app.msg = (
                "第一次用：向导会一步步问 —— 粘一条选课页网址（只写域名也行），剩下的我来"
                    .to_string(),
                Level::Ok,
            );
        }
        Ok(app)
    }

    /// 候选教学班还是模板里那几条示例（用户直接 cp 了 config.example.json 就会这样）。
    fn candidates_are_the_template(&self) -> bool {
        let ids = |v: &Json| -> Vec<String> {
            v.object("course")
                .map(|c| c.array("candidates").iter().map(|x| x.text("id")).collect())
                .unwrap_or_default()
        };
        let sample = ids(&json::parse_or_empty(config::CONFIG_EXAMPLE));
        let mine = ids(&self.raw);
        !mine.is_empty() && mine.iter().all(|id| sample.contains(id))
    }

    /// 还没配好"学校"这一节 —— 第一次进来的人应该先看到向导。
    fn needs_setup(&self) -> bool {
        let host = self
            .raw
            .object("school")
            .map(|s| s.text("host"))
            .unwrap_or_default();
        host.trim().is_empty() || host == "course.example.edu.cn"
    }

    fn root(&self, t: Target) -> &Json {
        match t {
            Target::Config => &self.raw,
            Target::Cred => &self.cred,
        }
    }

    fn root_mut(&mut self, t: Target) -> &mut Json {
        match t {
            Target::Config => &mut self.raw,
            Target::Cred => &mut self.cred,
        }
    }

    /// 凭据文件路径：跟着配置里的 `credentials_path` 走（改了那一项就重新读）。
    fn reload_credentials(&mut self, override_path: Option<&str>) {
        let p = match override_path {
            Some(s) if !s.is_empty() => config::expand_tilde(s),
            _ => {
                let from_cfg = self
                    .raw
                    .get("credentials_path")
                    .map(|v| v.as_text())
                    .filter(|s| !s.is_empty());
                let d = from_cfg.unwrap_or_else(|| format!("{}/credentials.json", config::APP_DIR));
                config::expand_tilde(&d)
            }
        };
        self.cred_path = p.clone();
        self.cred_exists = p.is_file();
        self.cred = Json::Obj(Vec::new());
        self.cred_wrap = false;
        if let Ok(text) = std::fs::read_to_string(&p) {
            let mut v = json::parse_or_empty(&text);
            if let Some(inner) = v.get("credentials").filter(|x| matches!(x, Json::Obj(_))) {
                self.cred_wrap = true;
                v = inner.clone();
            }
            if matches!(v, Json::Obj(_)) {
                self.cred = v;
            }
        }
        let pick = |names: &[&str], dflt: &str| -> String {
            names
                .iter()
                .find(|n| self.cred.get(n).is_some())
                .map(|s| (*s).to_string())
                .unwrap_or_else(|| dflt.to_string())
        };
        self.cred_keys = (
            pick(config::STUDENT_KEY_ALIASES, "student_id"),
            pick(config::PASSWORD_KEY_ALIASES, "password"),
        );
    }

    // -----------------------------------------------------------------------
    // 构造行
    // -----------------------------------------------------------------------

    fn rows(&self, si: usize) -> Vec<Row> {
        match si {
            0 => self.rows_school(),
            1 => self.rows_cred(),
            2 => self.rows_paths(),
            3 => self.rows_cookies(),
            4 => self.rows_password(),
            5 => self.rows_course(),
            6 => self.rows_captcha(),
            7 => self.rows_pacing(),
            8 => self.rows_http(),
            _ => self.rows_misc(),
        }
    }

    fn rows_school(&self) -> Vec<Row> {
        let s = "school";
        let mut v = vec![
            row_act(RowAct::WizardUrl, "⤓ 粘贴选课网址（自动填这一节）"),
            row_act(RowAct::Wizard, "❓ 配置向导（从零到能跑，一步一步问）"),
            row_field(Target::Config, s, "host", "域名 host", Ty::Text),
            row_field(Target::Config, s, "port", "端口 port", Ty::Int),
            row_field(
                Target::Config,
                s,
                "base_path",
                "接口前缀 base_path",
                Ty::Text,
            ),
            row_field(Target::Config, s, "page_path", "业务页 page_path", Ty::Text),
            row_field(Target::Config, s, "time_path", "对时页 time_path", Ty::Text),
            row_field(Target::Config, s, "referer_path", "referer_path", Ty::Text),
        ];
        if self
            .raw
            .object("school")
            .map(|s| s.get("login_page").is_some())
            .unwrap_or(false)
        {
            v.push(row_note(
                "login_page",
                "老配置带过来的键，程序不读它（留着也不影响）",
            ));
        }
        v.push(row_act(
            RowAct::Probe,
            "⇅ 联网自检（只读探测这些路径是否真的存在）",
        ));
        v
    }

    fn rows_cred(&self) -> Vec<Row> {
        let (sk, pk) = (&self.cred_keys.0, &self.cred_keys.1);
        let state = if self.cred_exists {
            "文件已存在，只改你动过的那两个键"
        } else {
            "文件还不存在，填好按 s 才会创建（0600 权限）"
        };
        vec![
            row_note("文件", &self.cred_path.display().to_string()),
            row_note("状态", state),
            row_top(Target::Cred, sk, &format!("学号 {sk}"), Ty::Text, false),
            row_top(Target::Cred, pk, &format!("密码 {pk}"), Ty::Text, true),
            row_note("提醒", "密码只存在凭据文件里，界面不回显；建议 chmod 600"),
        ]
    }

    fn rows_paths(&self) -> Vec<Row> {
        let mut v: Vec<Row> = Vec::new();
        for (key, name) in PATH_KEYS {
            v.push(row_field(
                Target::Config,
                "paths",
                key,
                &format!("{key} {name}"),
                Ty::Text,
            ));
        }
        // 配置里多出来的路径键也列出来（原样保留，不丢）
        if let Some(Json::Obj(pairs)) = self.raw.object("paths") {
            for (k, _) in pairs {
                if k.starts_with('_') || PATH_KEYS.iter().any(|(pk, _)| pk == k) {
                    continue;
                }
                v.push(row_field(
                    Target::Config,
                    "paths",
                    k,
                    &format!("{k}（自定义）"),
                    Ty::Text,
                ));
            }
        }
        v
    }

    fn rows_cookies(&self) -> Vec<Row> {
        let mut v = vec![row_note(
            "默认",
            "这两个列表留空就用 JSESSIONID / route,insert_cookie",
        )];
        v.extend(self.string_list_rows("cookies", "session", "登录态 Cookie"));
        v.extend(self.string_list_rows("cookies", "captcha", "验证码 Cookie"));
        v
    }

    fn rows_password(&self) -> Vec<Row> {
        let mut v = vec![row_note(
            "说明",
            "学校前端提交密码前用它加密，顺序敏感；最多 3 组",
        )];
        v.extend(self.string_list_rows("password", "des_keys", "密钥"));
        v
    }

    fn rows_course(&self) -> Vec<Row> {
        let n = self.raw.array("courses").len().max(1);
        let ci = self.cur_course.min(n - 1);
        let cur = self
            .raw
            .array("courses")
            .get(ci)
            .cloned()
            .unwrap_or(Json::Null);
        let name = cur.text("name");
        let kw = cur.text("keyword");
        let title = if !name.is_empty() {
            name.clone()
        } else if !kw.is_empty() {
            kw.clone()
        } else {
            "（还没填名字）".to_string()
        };

        let mut v = vec![
            row_note(&format!("目标课程 {}/{}：{title}", ci + 1, n), ""),
            row_act(RowAct::NextCourse, "⇄ 编辑下一门（在几门课之间轮换）"),
            row_act(RowAct::AddCourse, "＋ 再加一门目标课程"),
            row_act(RowAct::DelCourse, "✕ 删掉当前这门课（至少留一门）"),
            row_path(
                Target::Config,
                Path::Course(ci, "name"),
                "名字 name",
                Ty::Text,
            ),
            row_path(
                Target::Config,
                Path::Course(ci, "keyword"),
                "课程名 keyword",
                Ty::Text,
            ),
            row_path(
                Target::Config,
                Path::Course(ci, "class_type"),
                "课程类型 class_type",
                Ty::Text,
            ),
            row_path(
                Target::Config,
                Path::Course(ci, "is_major"),
                "是否专业课 is_major",
                Ty::Text,
            ),
            row_path(
                Target::Config,
                Path::Course(ci, "query_content"),
                "查询模板 query_content",
                Ty::Text,
            ),
        ];

        let cands: Vec<Json> = cur.array("candidates").to_vec();
        if cands.is_empty() {
            v.push(row_note(
                "候选教学班",
                "（这一门还没挑 —— 按下面那行拉课程目录）",
            ));
        }
        let mut shown_group = String::new();
        for (i, item) in cands.iter().enumerate() {
            let group = item.text("group");
            // 同一个冲突组只提示一次（看着像分组小标题）
            if !group.is_empty() && group != shown_group {
                shown_group = group.clone();
                v.push(row_note(&format!("冲突组 {group}"), ""));
            }
            for key in ["id", "label", "group"] {
                v.push(row_path(
                    Target::Config,
                    Path::CourseCand(ci, i, key),
                    &format!("[{i}].{key}"),
                    Ty::Text,
                ));
            }
        }
        v.push(row_act(
            RowAct::FetchCandidates,
            "⇣ 拉课程目录（挑完写进**当前这门课**）",
        ));
        v.push(row_act(
            RowAct::AddCourseCand(ci),
            "＋ 添加候选教学班（手工填 ID）",
        ));
        v
    }

    fn rows_captcha(&self) -> Vec<Row> {
        vec![
            row_field(Target::Config, "captcha", "width", "宽 width", Ty::Int),
            row_field(Target::Config, "captcha", "height", "高 height", Ty::Int),
            row_field(
                Target::Config,
                "captcha",
                "min_margin",
                "置信度下限 min_margin",
                Ty::Float,
            ),
            row_field(
                Target::Config,
                "captcha",
                "model_file",
                "模型文件 model_file",
                Ty::Text,
            ),
            row_note("说明", "模型已编在程序里，model_file 留空即用内编的"),
        ]
    }

    fn rows_pacing(&self) -> Vec<Row> {
        vec![
            row_field(
                Target::Config,
                "pacing",
                "per_window",
                "每窗口请求数 per_window",
                Ty::Int,
            ),
            row_field(
                Target::Config,
                "pacing",
                "window",
                "窗口秒数 window",
                Ty::Float,
            ),
            row_field(
                Target::Config,
                "pacing",
                "margin",
                "安全余量 margin",
                Ty::Float,
            ),
            row_field(
                Target::Config,
                "pacing",
                "min_gap",
                "最小间隔 min_gap",
                Ty::Float,
            ),
            row_field(
                Target::Config,
                "pacing",
                "overlap_wait",
                "重叠等待 overlap_wait",
                Ty::Float,
            ),
            row_note(
                "说明",
                "默认值来自实测；换学校务必自己重测，调太快会被限流作废会话",
            ),
        ]
    }

    fn rows_http(&self) -> Vec<Row> {
        vec![
            row_field(Target::Config, "http", "user_agent", "User-Agent", Ty::Text),
            row_field(
                Target::Config,
                "http",
                "read_timeout",
                "读超时 read_timeout",
                Ty::Float,
            ),
            row_field(
                Target::Config,
                "http",
                "write_timeout",
                "写超时 write_timeout",
                Ty::Float,
            ),
            row_field(
                Target::Config,
                "http",
                "connect_timeout",
                "连接超时 connect_timeout",
                Ty::Float,
            ),
            row_note("说明", "write_timeout 会被命令行的 --write-timeout 覆盖"),
        ]
    }

    fn rows_misc(&self) -> Vec<Row> {
        vec![
            row_top(
                Target::Config,
                "timezone_offset_hours",
                "时区偏移 timezone_offset_hours",
                Ty::Float,
                false,
            ),
            row_top(
                Target::Config,
                "credentials_path",
                "凭据文件 credentials_path",
                Ty::Text,
                false,
            ),
            row_note(
                "查找顺序",
                "读取顺序：COURSE_GRABBER_CONFIG → 程序旁边 → 当前目录 → ~/.config/course-grabber",
            ),
        ]
    }

    fn string_list_rows(&self, sec: &str, list: &str, name: &str) -> Vec<Row> {
        let mut v: Vec<Row> = Vec::new();
        let items: Vec<Json> = self
            .raw
            .object(sec)
            .map(|s| s.array(list).to_vec())
            .unwrap_or_default();
        for (i, item) in items.iter().enumerate() {
            let label = if item.as_str().map(|s| s.is_empty()).unwrap_or(true) {
                format!("{name} [{i}]（空）")
            } else {
                format!("{name} [{i}]")
            };
            v.push(row_item(
                Target::Config,
                sec,
                list,
                i,
                None,
                label,
                Ty::Text,
            ));
        }
        match list_cap(sec, list) {
            Some(cap) if items.len() >= cap => {
                v.push(row_note("上限", &format!("最多 {cap} 组，先删再加")));
            }
            _ => v.push(row_add(sec, list, &format!("＋ 添加{name}"))),
        }
        v
    }

    fn rows_with_values(&self, si: usize) -> Vec<Row> {
        let mut rows = self.rows(si);
        for r in rows.iter_mut() {
            let f = match &r.field {
                Some(f) => f.clone(),
                None => continue,
            };
            let cur = f.path.get(self.root(f.target));
            let def = self.default_at(f.target, &f.path);
            let (text, dim) = display_value(cur, f.secret, def.as_ref());
            r.value = text;
            r.dim = dim;
        }
        rows
    }

    fn default_at(&self, t: Target, p: &Path) -> Option<Json> {
        if t != Target::Config {
            return None;
        }
        match p {
            Path::Top(k) => self.defaults.get(k).cloned(),
            Path::Sec(s, k) => self.defaults.object(s)?.get(k).cloned(),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // 渲染
    // -----------------------------------------------------------------------

    fn render(&mut self, t: &mut term::Term, sz: (usize, usize)) {
        let (cols, rows_h) = sz;
        // 最后一列不用：写满一行会让终端自动换行，光标就多跳一行。
        // 窄到不成样子也不许"画得比屏幕宽"—— 该被截掉的是内容，不是边框。
        let w = cols.saturating_sub(1).max(1);
        let h = rows_h.max(4);
        let body_h = body_rows(h);

        let rows = self.rows_with_values(self.sec);
        self.clamp(rows.len(), body_h);

        let mut out: Vec<String> = Vec::with_capacity(h);
        let mut cursor: Option<(usize, usize)> = None;
        out.push(self.title_line(w));

        match &self.mode {
            Mode::Info { page, scroll, .. } => {
                let lines = self.info_lines(page, w);
                let max = lines.len().saturating_sub(body_h);
                let sc = (*scroll).min(max);
                for i in 0..body_h {
                    out.push(match lines.get(sc + i) {
                        Some(l) => self.line(l, w),
                        None => String::new(),
                    });
                }
            }
            Mode::Job(job) => {
                out.push(self.line(
                    &format!("{}▏ {}", SPIN[job.spin % SPIN.len()], job.title),
                    w,
                ));
                // 日志尾部的若干行（像 tail -f，最新的一直在下面）
                let view = body_h.saturating_sub(1);
                let skip = job.lines.len().saturating_sub(view);
                for i in 0..view {
                    match job.lines.get(skip + i) {
                        Some((lv, text)) => {
                            let mut l = Line::new(w, self.color);
                            l.seg(self.color, level_style(*lv), text);
                            out.push(l.finish());
                        }
                        None => out.push(String::new()),
                    }
                }
            }
            Mode::Pick(pick) => {
                // 第 1 行：总量 + 各类型计数 + 类型开关状态
                let counts: Vec<String> = onboard::COURSE_TYPES
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (code, name))| {
                        let n = pick.rows.iter().filter(|r| r.tc_type == *code).count();
                        if n == 0 {
                            return None;
                        }
                        let on = if pick.type_on.get(i).copied().unwrap_or(true) {
                            "✓"
                        } else {
                            "✗"
                        };
                        Some(format!("{}{on} {name} {n}", i + 1))
                    })
                    .collect();
                let mut head = Line::new(w, self.color);
                head.seg(
                    self.color,
                    S_BOLD,
                    &format!(
                        "拉到了 {} 个教学班 · 空格勾选 · Enter 写入",
                        pick.rows.len()
                    ),
                );
                head.seg(
                    self.color,
                    S_DIM,
                    &format!(
                        "　批次 {} · 校区 {} · 学生 {}　类型：{}",
                        pick.batch_name,
                        pick.campus,
                        pick.student,
                        counts.join("  ")
                    ),
                );
                out.push(head.finish());

                // 第 2 行：筛选词（`/` 进输入；这里是按显示宽度算的光标位置）
                let prompt = "筛选 ▏ ";
                let mut fl = Line::new(w, self.color);
                fl.seg(self.color, S_BOLD, prompt);
                if pick.typing || !pick.filter.is_empty() {
                    let avail = w.saturating_sub(textw::width(prompt));
                    let mut inp = Input::new(&pick.filter);
                    inp.pos = pick.filter.chars().count();
                    let (shown, col) = inp.window(avail);
                    fl.seg(self.color, "", &shown);
                    if pick.typing {
                        cursor = Some((
                            3, // 标题(1) + 汇总(2) + 这一行
                            (textw::width(prompt) + col).min(w.saturating_sub(1)) + 1,
                        ));
                    }
                } else {
                    fl.seg(
                        self.color,
                        S_DIM,
                        &format!(
                            "按 / 输入筛选词（课程名 / 教师 / 班号 / 类型都能搜）· 现在显示 {} 个",
                            pick.shown.len()
                        ),
                    );
                }
                out.push(fl.finish());

                // 其余：列表
                let view = pick.view(body_h);
                for i in 0..view {
                    let idx = pick.top + i;
                    match pick.shown.get(idx) {
                        Some(ri) => {
                            let r = &pick.rows[*ri];
                            let mark = if pick.checked[*ri] { "[x]" } else { "[ ]" };
                            let text = format!("{mark} {}", r.display());
                            let mut l = Line::new(w, self.color);
                            if idx == pick.cur && !pick.typing {
                                l.seg(self.color, S_SEL, &text);
                            } else {
                                l.seg(self.color, "", &text);
                            }
                            out.push(l.finish());
                        }
                        None => out.push(String::new()),
                    }
                }
            }
            Mode::Wizard(wz) => out.extend(self.wizard_body(wz, w, body_h)),
            _ => self.push_panes(&mut out, &rows, w, body_h),
        }

        // 编辑态 / 向导：状态行变成输入框，并把硬件光标摆进去
        match &self.mode {
            Mode::Edit(e) => {
                let prompt = format!(" {} ▏ ", e.label);
                let avail = w.saturating_sub(textw::width(&prompt));
                let (shown, col) = e.input.window(avail);
                let mut l = Line::new(w, self.color);
                l.seg(self.color, S_BOLD, &prompt);
                l.seg(self.color, "", &shown);
                out.push(l.finish());
                cursor = Some((
                    h - 1,
                    (textw::width(&prompt) + col).min(w.saturating_sub(1)) + 1,
                ));
            }
            Mode::Confirm { prompt, .. } => {
                out.push(self.line(&format!("{prompt} [y/N]"), w));
            }
            Mode::Info { title, .. } => {
                out.push(self.line(&format!("{title} —— ↑↓ 翻页，Esc 返回"), w));
            }
            Mode::Job(job) => {
                out.push(self.line(&format!("{} 进行中…", job.title), w));
            }
            Mode::Pick(pick) => {
                let (text, style) = if !pick.warn.is_empty() {
                    (pick.warn.clone(), S_WARN)
                } else if !pick.note.is_empty() {
                    (pick.note.clone(), S_OK)
                } else {
                    (
                        "勾选完按 Enter 写入（会整段替换现有候选）".to_string(),
                        S_DIM,
                    )
                };
                let mut l = Line::new(w, self.color);
                l.seg(self.color, style, &text);
                out.push(l.finish());
            }
            Mode::Wizard(wz) => match self.wizard_prompt(wz) {
                Some(prompt) => {
                    let avail = w.saturating_sub(textw::width(&prompt));
                    let (shown, col) = self.wizard_shown_input(wz, avail);
                    let mut l = Line::new(w, self.color);
                    l.seg(self.color, S_BOLD, &prompt);
                    l.seg(self.color, "", &shown);
                    out.push(l.finish());
                    cursor = Some((
                        h - 1,
                        (textw::width(&prompt) + col).min(w.saturating_sub(1)) + 1,
                    ));
                }
                None => out.push(self.line("回车保存并完成 · Esc 先不保存", w)),
            },
            Mode::Browse => {
                let (m, lv) = (&self.msg.0, self.msg.1);
                let mut line = Line::new(w, self.color);
                line.seg(self.color, level_style(lv), m);
                out.push(line.finish());
            }
        }

        let keys = match &self.mode {
            Mode::Info { .. } => "↑↓ 翻页 · Esc/q 返回",
            Mode::Confirm { .. } => "y 确认 · n/Esc 取消",
            Mode::Edit(_) => "Enter 确定 · Esc 取消 · Ctrl-U 清空 · Ctrl-W 删词",
            Mode::Job { .. } => "任务在跑（只读请求）· Esc 离开这一屏 · q 退出",
            Mode::Pick(pick) => {
                if pick.typing {
                    "正在输入筛选词 · Enter/Esc 回到列表 · Ctrl-U 清空"
                } else {
                    "空格 勾选 · A 全选 · / 筛选 · 1-9 类型开关 · Enter 写入 · Esc 放弃"
                }
            }
            Mode::Wizard(wz) => match wz.step {
                WStep::Url => "Enter 去问服务器（只读 GET）· Esc 退出向导 · Ctrl-C 放弃",
                WStep::Student | WStep::Password => "Enter 下一步 · Esc 上一步 · Ctrl-C 放弃向导",
                WStep::PickCourse => "Enter 登录并把课程目录拉下来（只读）· Esc 上一步 · Ctrl-C 放弃",
                WStep::Done => "Enter 保存并完成 · Esc 上一步（先不保存）",
            },
            Mode::Browse => {
                "↑↓ 移动 · ←→/Tab 换节 · Enter 编辑/执行 · a 加 d 删 · s 保存 · v 校验 · w 向导 · ? 帮助 · q 退出"
            }
        };
        let mut kl = Line::new(w, self.color);
        kl.seg(self.color, S_KEY, keys);
        out.push(kl.finish());

        while out.len() < h {
            out.insert(out.len() - 2, String::new());
        }
        t.draw(&out.join("\n"));
        match cursor {
            Some((row, col)) => t.show_cursor_at(row, col),
            None => t.hide_cursor(),
        }
    }

    fn push_panes(&self, out: &mut Vec<String>, rows: &[Row], w: usize, body_h: usize) {
        for i in 0..body_h {
            let mut l = Line::new(w, self.color);
            // 左栏：段列表
            match SEC_TITLES.get(i) {
                Some(title) => {
                    let text = format!(" {} {} ", i + 1, title);
                    let style = if i == self.sec { S_SEL } else { "" };
                    l.seg(self.color, style, &textw::pad_right(&text, LEFT_W));
                }
                None => l.raw(&" ".repeat(LEFT_W)),
            }
            l.seg(self.color, S_DIM, " │ ");
            // 右栏：第一行是说明，其余是行
            if i == 0 {
                l.seg(self.color, S_DIM, &self.sec_desc(self.sec));
            } else if let Some(r) = rows.get(self.top + i - 1) {
                self.push_row(&mut l, r, self.top + i - 1 == self.cur);
            }
            out.push(l.finish());
        }
    }

    fn push_row(&self, l: &mut Line, r: &Row, selected: bool) {
        let mut head = String::new();
        head.push_str(if selected { "▸ " } else { "  " });
        head.push_str(&" ".repeat(r.indent * 2));
        let used = textw::width(&head);
        head.push_str(&textw::pad_right(&r.label, LABEL_W.saturating_sub(used)));

        if selected {
            // 选中行整行反显（含值）
            l.seg(self.color, S_SEL, &format!("{head}{}", r.value));
            return;
        }
        let head_style = if r.act.is_some() { S_ADD } else { "" };
        let val_style = if r.dim { S_DIM } else { "" };
        l.seg(self.color, head_style, &head);
        l.seg(self.color, val_style, &r.value);
    }

    fn sec_desc(&self, si: usize) -> String {
        match si {
            0 => "学校域名与页面路径 —— 粘一条选课网址就填好了（第一行是向导）".to_string(),
            1 => format!("自动登录用的学号密码 → {}", self.cred_path.display()),
            2 => "各接口的路径模板；{base} 会被上面的 base_path 替换".to_string(),
            3 => "登录态与验证码环节下发的 Cookie 名".to_string(),
            4 => "前端提交密码前用的加密密钥（顺序敏感）".to_string(),
            5 => "目标课程（可以多门）：加课 / 删课 / 切换，各自挑候选教学班".to_string(),
            6 => "点选验证码识别参数".to_string(),
            7 => "写接口限流参数".to_string(),
            8 => "HTTP 头与超时".to_string(),
            _ => "时区、凭据路径等零碎项".to_string(),
        }
    }

    fn line(&self, text: &str, w: usize) -> String {
        let mut l = Line::new(w, self.color);
        l.raw(text);
        l.finish()
    }

    /// 长文本页面的内容。向导与校验是**现算**的，所以勾和问题都是活的。
    ///
    /// 按终端宽度折行：这些是给人读的整句话，被裁成「…或者用「联网自检」…」就没意义了。
    fn info_lines(&self, page: &InfoPage, w: usize) -> Vec<String> {
        let raw: Vec<String> = match page {
            InfoPage::Help => help_lines().iter().map(|s| (*s).to_string()).collect(),
            InfoPage::Problems => self
                .problems()
                .iter()
                .map(|(l, s)| format!("{} {s}", l.mark()))
                .collect(),
            InfoPage::Notes(notes) => notes
                .iter()
                .map(|(l, s)| format!("{} {s}", l.mark()))
                .collect(),
        };
        raw.iter().flat_map(|l| textw::wrap(l, w, "    ")).collect()
    }

    // -----------------------------------------------------------------------
    // 配置向导
    // -----------------------------------------------------------------------

    /// 开始（或继续）向导：从"第一件还没做的事"开始问。
    fn start_wizard(&mut self, from: WStep) {
        let (sk, pk) = (&self.cred_keys.0, &self.cred_keys.1);
        let student = self.cred.text(sk).trim().to_string();
        let keyword = self
            .raw
            .object("course")
            .map(|c| c.text("keyword"))
            .unwrap_or_default();
        let cands_empty = self
            .raw
            .object("course")
            .map(|c| c.array("candidates").is_empty())
            .unwrap_or(true);

        let step = match from {
            WStep::Url => {
                if self.needs_setup() {
                    WStep::Url
                } else if student.is_empty() || self.cred.text(pk).trim().is_empty() {
                    WStep::Student
                } else if keyword.trim().is_empty()
                    || cands_empty
                    || self.candidates_are_the_template()
                {
                    WStep::PickCourse
                } else {
                    // 全都齐了：那就是想重来一遍学校那一节
                    WStep::Url
                }
            }
            other => other,
        };
        let prefill = match step {
            WStep::Url => String::new(),
            WStep::Student => student.clone(),
            WStep::Password => String::new(),
            WStep::PickCourse => String::new(),
            _ => String::new(),
        };
        self.mode = Mode::Wizard(Box::new(Wizard {
            step,
            input: Input::new(&prefill),
            replace_on_type: !prefill.is_empty(),
            notes: Vec::new(),
            scroll: 0,
            err: String::new(),
            discovered: None,
            student,
            password: String::new(),
            keyword,
            picked: 0,
        }));
    }

    fn wizard_step_title(&self, w: &Wizard) -> String {
        let n = match w.step {
            WStep::Url => 1,
            WStep::Student => 2,
            WStep::Password => 3,
            WStep::PickCourse => 4,
            WStep::Done => 4,
        };
        let name = match w.step {
            WStep::Url => "学校地址",
            WStep::Student | WStep::Password => "登录身份",
            WStep::PickCourse => "挑课",
            WStep::Done => "完成",
        };
        format!("第 {n}/4 步 · {name}")
    }

    fn wizard_desc(&self, w: &Wizard) -> String {
        match w.step {
            WStep::Url => {
                "把浏览器地址栏里选课页的网址粘进来（Ctrl-L 全选、Ctrl-C，然后 Ctrl-V）。\n\
                 只写域名也行 —— 我会去问服务器页面在哪。"
                    .to_string()
            }
            WStep::Student => {
                "学号。自动登录、以及拉课程目录都用它。只写进凭据文件，不进 config.json。"
                    .to_string()
            }
            WStep::Password => {
                "密码。只写进凭据文件（保存时 0600），界面不回显，也不会进 config.json。"
                    .to_string()
            }
            WStep::PickCourse => {
                let host = self.host_port();
                format!(
                    "回车就用你的学号登录 {host}，把**每一类课都拉下来**（方案内 / 方案外 / \n\
                     校公选 / 体育 / 慕课），然后你在列表里自己挑 —— 不用先想关键词。\n\
                     只读查询，不发选课请求。"
                )
            }
            WStep::Done => "看一眼要写什么，回车就保存。".to_string(),
        }
    }

    /// 当前这一步的输入框前缀（`Done` 步没有输入框）。
    fn wizard_prompt(&self, w: &Wizard) -> Option<String> {
        match w.step {
            WStep::Url => Some("网址 ▏ ".to_string()),
            WStep::Student => Some("学号 ▏ ".to_string()),
            WStep::Password => Some("密码 ▏ ".to_string()),
            WStep::PickCourse => None, // 这一步没有输入框：回车就拉目录
            WStep::Done => None,
        }
    }

    /// 输入框里显示什么：密码那一步用圆点掩码。
    fn wizard_shown_input(&self, w: &Wizard, avail: usize) -> (String, usize) {
        if w.step == WStep::Password {
            // 密码不回显：同样的字符数，全是圆点
            let masked: String = w.input.chars.iter().map(|_| '•').collect();
            let mut shown = Input::new(&masked);
            shown.pos = w.input.pos;
            shown.window(avail)
        } else {
            w.input.window(avail)
        }
    }

    /// 向导里的回车。返回 true = 留在向导（否则已经切走了）。
    fn wizard_enter(&mut self, w: &mut Wizard) -> bool {
        w.err.clear();
        match w.step {
            WStep::Url => {
                let text = w.input.text();
                if text.trim().is_empty() {
                    w.err = "先粘一条网址 —— 只写域名也行，比如 jw.example.edu.cn".to_string();
                    return true;
                }
                self.start_discover(text);
                false
            }
            WStep::Student => {
                let v = w.input.text().trim().to_string();
                if v.is_empty() {
                    w.err = "学号不能空".to_string();
                    return true;
                }
                w.student = v;
                w.step = WStep::Password;
                w.input = Input::new("");
                w.replace_on_type = false;
                w.notes = vec![(
                    Level::Ok,
                    format!(
                        "学号 {} 记下了。密码只写进凭据文件，界面不回显。",
                        crate::auth::mask_id(&w.student)
                    ),
                )];
                w.scroll = 0;
                true
            }
            WStep::Password => {
                if w.input.is_empty() {
                    w.err = "密码不能空（不想填就按 Ctrl-C 退出向导，候选手填）".to_string();
                    return true;
                }
                w.password = w.input.text();
                // 先放进内存里的凭据，等收尾那一步的 s 才落盘
                let (sk, pk) = (self.cred_keys.0.clone(), self.cred_keys.1.clone());
                self.cred.set_key(&sk, Json::str(w.student.clone()));
                self.cred.set_key(&pk, Json::str(w.password.clone()));
                self.cred_dirty = true;
                w.step = WStep::PickCourse;
                w.notes = vec![(
                    Level::Ok,
                    format!(
                        "登录身份齐了（{}）—— 保存时会写成 {}（0600）",
                        crate::auth::mask_id(&w.student),
                        self.cred_path.display()
                    ),
                )];
                w.scroll = 0;
                let prefill = w.keyword.clone();
                w.input = Input::new(&prefill);
                w.replace_on_type = !prefill.is_empty();
                true
            }
            WStep::PickCourse => {
                // 回车就把目录整个拉下来，拉完进挑课屏
                match self.start_fetch(true) {
                    Ok(()) => false,
                    Err(e) => {
                        w.err = e;
                        true
                    }
                }
            }
            WStep::Done => {
                self.wizard_finish();
                false
            }
        }
    }

    /// 向导里按 Esc：退一步；在第一步按就退出向导。
    fn wizard_back(&mut self, w: &mut Wizard) -> bool {
        let prev = match w.step {
            WStep::Url => None,
            WStep::Student => Some(WStep::Url),
            WStep::Password => Some(WStep::Student),
            WStep::PickCourse => Some(WStep::Password),
            WStep::Done => Some(WStep::PickCourse),
        };
        match prev {
            Some(step) => {
                let prefill = match step {
                    WStep::Url => String::new(),
                    WStep::Student => w.student.clone(),
                    WStep::Password => String::new(),
                    WStep::PickCourse => String::new(),
                    WStep::Done => String::new(),
                };
                w.step = step;
                w.input = Input::new(&prefill);
                w.replace_on_type = !prefill.is_empty();
                w.err.clear();
                w.notes.clear();
                w.scroll = 0;
                true
            }
            None => {
                self.msg = (
                    "已退出向导。已经填好的东西还在内存里 —— 按 s 保存，或者按 w 重新进向导"
                        .to_string(),
                    Level::Warn,
                );
                false
            }
        }
    }

    /// 第 1 步：去问服务器。全程只读 GET。
    fn start_discover(&mut self, text: String) {
        self.start_job(
            "问服务器要页面路径（只读 GET）",
            JobNext::WizardDiscover,
            move || JobDone::Discover(onboard::discover(&text, &|m: &str| crate::log::log(m))),
        );
    }

    fn finish_discover(&mut self, result: Result<onboard::Discovered, String>, lines: Vec<Note>) {
        let Some(mut w) = self.wiz.take() else {
            // 不是向导发起的（理论上不会发生）：当一页报告显示
            match result {
                Ok(d) => self.show_info(InfoPage::Notes(d.notes), "自动发现结果"),
                Err(e) => self.show_info(InfoPage::Notes(vec![(Level::Err, e)]), "自动发现失败"),
            }
            return;
        };
        match result {
            Ok(d) => {
                let mut notes = lines;
                notes.extend(onboard::apply(
                    &mut self.raw,
                    &d.derived,
                    d.time_path.as_deref(),
                ));
                notes.extend(d.notes.iter().cloned());
                self.dirty = true;
                w.err.clear();
                w.notes = notes;
                w.scroll = 0;
                w.step = WStep::Student;
                let prefill = w.student.clone();
                w.input = Input::new(&prefill);
                w.replace_on_type = !prefill.is_empty();
                w.discovered = Some(Box::new(d));
                self.mode = Mode::Wizard(w);
            }
            Err(e) => {
                w.err = e;
                self.mode = Mode::Wizard(w);
            }
        }
    }

    /// 向导主体的那几行（步骤标题、说明、上一步的结果）。
    fn wizard_body(&self, wz: &Wizard, width: usize, body_h: usize) -> Vec<String> {
        let mut head: Vec<(String, &'static str)> = Vec::new();
        head.push((self.wizard_step_title(wz), S_TITLE));
        head.push((String::new(), ""));
        for l in textw::wrap(&self.wizard_desc(wz), width, "   ") {
            head.push((l, S_DIM));
        }
        head.push((String::new(), ""));
        if !wz.err.is_empty() {
            for l in textw::wrap(&format!("✗ {}", wz.err), width, "     ") {
                head.push((l, S_ERR));
            }
            head.push((String::new(), ""));
        }

        let mut out: Vec<String> = Vec::new();
        let push = |text: &str, style: &'static str, out: &mut Vec<String>| {
            let mut l = Line::new(width, self.color);
            l.seg(self.color, style, text);
            out.push(l.finish());
        };
        for (text, style) in head.iter().take(body_h) {
            push(text, style, &mut out);
        }
        let room = body_h.saturating_sub(out.len());
        if room > 0 {
            let notes = self.wizard_note_lines(wz, width);
            let skip = wz.scroll.min(notes.len().saturating_sub(room));
            for i in 0..room {
                match notes.get(skip + i) {
                    Some((text, style)) => push(text, style, &mut out),
                    None => out.push(String::new()),
                }
            }
        }
        out.truncate(body_h);
        while out.len() < body_h {
            out.push(String::new());
        }
        out
    }

    /// 向导里的"过程/结果"区（已折行、带色）。
    fn wizard_note_lines(&self, wz: &Wizard, width: usize) -> Vec<(String, &'static str)> {
        if wz.step == WStep::Done {
            let (sk, pk) = (&self.cred_keys.0, &self.cred_keys.1);
            // 多课程：把所有课的候选合起来数，课程名也一起列出来
            let courses: Vec<Json> = self.raw.array("courses").to_vec();
            let mut cands: Vec<Json> = Vec::new();
            let mut course_names: Vec<String> = Vec::new();
            for c in &courses {
                let kw = c.text("keyword");
                let nm = c.text("name");
                course_names.push(if !nm.is_empty() {
                    nm
                } else if !kw.is_empty() {
                    kw
                } else {
                    "（未命名）".to_string()
                });
                cands.extend(c.array("candidates").iter().cloned());
            }
            let n = cands.len();
            let groups: Vec<String> = {
                let mut g: Vec<String> = Vec::new();
                for c in &cands {
                    let name = c.text("group");
                    if !name.is_empty() && !g.contains(&name) {
                        g.push(name);
                    }
                }
                g
            };
            let host = self.host_port();
            let course_label = if course_names.is_empty() {
                "（未命名）".to_string()
            } else {
                course_names.join("、")
            };
            let mut out: Vec<(String, &'static str)> = Vec::new();
            let add = |t: String, style: &'static str, out: &mut Vec<(String, &'static str)>| {
                for l in textw::wrap(&t, width, "        ") {
                    out.push((l, style));
                }
            };
            add("回车之后会写两个文件：".to_string(), S_BOLD, &mut out);
            add(
                format!(
                    "  · config.json —— 学校 {host}、课程「{course_label}」、候选教学班 {} 个（冲突组 {}）",
                    n,
                    if groups.is_empty() {
                        "没写".to_string()
                    } else {
                        groups.join("、")
                    }
                ),
                S_OK,
                &mut out,
            );
            add(
                format!(
                    "  · {} —— 学号 {}（0600，只有你能读）",
                    self.cred_path.display(),
                    crate::auth::mask_id(&wz.student)
                ),
                S_OK,
                &mut out,
            );
            add(String::new(), "", &mut out);
            add(
                "写完之后：跑 `course-grabber`（不加 --live）做只读预检，预检全过再等放课时间跑 --live。"
                    .to_string(),
                S_DIM,
                &mut out,
            );
            let _ = (sk, pk);
            return out;
        }
        wz.notes
            .iter()
            .flat_map(|(lv, t)| {
                let lv = *lv;
                textw::wrap(&format!("{} {}", lv.mark(), t), width, "   ")
                    .into_iter()
                    .map(move |l| (l, level_style(lv)))
            })
            .collect()
    }

    /// 收尾：把两份文件写下去。
    fn wizard_finish(&mut self) {
        self.save();
        let saved = self.msg.clone();
        // 跳回编辑器，光标停在「目标课程」（用户大概率想核对候选）
        self.sec = 5;
        self.cur = 0;
        self.top = 0;
        self.mode = Mode::Browse;
        self.msg = match saved.1 {
            Level::Err => saved,
            _ => (
                format!(
                    "{} —— 下一步：跑 `course-grabber`（不加 --live）做只读预检",
                    saved.0
                ),
                Level::Ok,
            ),
        };
    }

    fn title_line(&self, w: usize) -> String {
        let problems = self.problems();
        let bad = problems.iter().filter(|(l, _)| *l == Level::Err).count();
        let warn = problems.iter().filter(|(l, _)| *l == Level::Warn).count();
        let mut s = format!(" course-grabber 配置 · {}", self.path.display());
        if self.fresh {
            s.push_str(" · 新文件");
        }
        if self.dirty || self.cred_dirty {
            s.push_str(" · ●未保存");
        } else if !self.fresh {
            s.push_str(" · 已同步");
        }
        if bad > 0 {
            s.push_str(&format!(" · ✗{bad} 处待修"));
        } else if warn > 0 {
            s.push_str(&format!(" · ⚠{warn} 处提醒"));
        } else {
            s.push_str(" · ✓ 完整");
        }
        let mut l = Line::new(w, self.color);
        l.seg(self.color, S_TITLE, &s);
        l.finish()
    }

    // -----------------------------------------------------------------------
    // 校验
    // -----------------------------------------------------------------------

    fn problems(&self) -> Vec<(Level, String)> {
        let mut v: Vec<(Level, String)> = Vec::new();
        let host = self
            .raw
            .object("school")
            .map(|s| s.text("host"))
            .unwrap_or_default();
        if host.trim().is_empty() {
            v.push((
                Level::Err,
                "school.host 没填。最快：按 1 到「学校」一节 → Enter 执行「⤓ 粘贴选课网址」，\n              把浏览器地址栏里那条选课页 URL 粘进来，这一节就齐了".to_string(),
            ));
        } else if host == "course.example.edu.cn" {
            v.push((
                Level::Warn,
                format!("school.host 还是模板里的示例域名 {host}"),
            ));
        }

        let missing: Vec<&str> = PATH_KEYS
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| {
                self.raw
                    .object("paths")
                    .map(|p| p.text(k).trim().is_empty())
                    .unwrap_or(true)
            })
            .collect();
        if !missing.is_empty() {
            v.push((
                Level::Err,
                format!(
                    "接口路径没填齐 paths.*：{}（粘一条选课网址会自动照参考实现补齐）",
                    missing.join(", ")
                ),
            ));
        }

        let keys: Vec<Json> = self
            .raw
            .object("password")
            .map(|p| p.array("des_keys").to_vec())
            .unwrap_or_default();
        if keys.is_empty() {
            v.push((
                Level::Err,
                "password.des_keys 没填：登录提交密码要用它（粘一条选课网址会自动补齐）"
                    .to_string(),
            ));
        } else if keys.len() > 3 {
            v.push((
                Level::Err,
                format!("password.des_keys 有 {} 组，前端协议最多 3 组", keys.len()),
            ));
        } else if keys.iter().any(|k| k.as_text().is_empty()) {
            v.push((Level::Err, "password.des_keys 里有空串".to_string()));
        }

        // 多课程：逐门课校验候选。报问题时带上"第几门课 + 叫什么"，
        // 否则两门课各有一个缺 id 的候选，用户根本分不清该去哪一门里改。
        let courses: Vec<Json> = self.raw.array("courses").to_vec();
        let mut all_courses_empty = true;
        for (ci, course) in courses.iter().enumerate() {
            let c = course.array("candidates");
            if !c.is_empty() {
                all_courses_empty = false;
            }
            let who = {
                let n = course.text("name");
                let k = course.text("keyword");
                if !n.is_empty() {
                    n
                } else if !k.is_empty() {
                    k
                } else {
                    format!("第 {} 门", ci + 1)
                }
            };
            // query_content 和类别对不上是很隐蔽的错：慕课之外带着 `MOOC:` 开关，
            // 目录查询会返回空 —— 表现是"目录里查不到这门课"，从报错里看不出原因。
            // （2026-09-27：用户那门线性代数的 query_content 就是 `MOOC:2,{keyword}`，
            //  于是预检查目录一直是 0 条。）
            let qt = course.text("query_content");
            if qt.contains("MOOC:") && !course.text("class_type").eq_ignore_ascii_case("MOOC") {
                v.push((
                    Level::Warn,
                    format!(
                        "「{who}」的 query_content 里带着 MOOC: 开关，但类别填的是 {} —— 目录查询大概会返回空（慕课之外不该带它）",
                        course.text("class_type")
                    ),
                ));
            }
            for (i, item) in c.iter().enumerate() {
                if item.text("id").trim().is_empty() {
                    v.push((Level::Err, format!("「{who}」候选 [{i}] 缺 id")));
                }
                if item.text("group").trim().is_empty() {
                    v.push((
                        Level::Warn,
                        format!("「{who}」候选 [{i}] 没写 group（冲突组），跨组串行判断会不准"),
                    ));
                }
            }
        }
        if all_courses_empty {
            v.push((
                Level::Warn,
                "所有目标课程都还没有候选教学班。按 6 到「目标课程」一节，光标移到「⇣ 拉课程目录」按 Enter ——\n              会登录把每一类课都拉下来让你挑（只读）；不想联网也可以手工填 ID（或命令行 --priority）"
                    .to_string(),
            ));
        }
        if self.candidates_are_the_template() {
            v.push((
                Level::Warn,
                "候选教学班还是模板里的示例（000…001 那几个），不是你学校的班 —— \
                 按 6 →「⇣ 拉课程目录」重挑一遍"
                    .to_string(),
            ));
        }

        let port = self
            .raw
            .object("school")
            .and_then(|s| s.get("port"))
            .and_then(|v| v.as_i64())
            .unwrap_or(80);
        if !(1..=65535).contains(&port) {
            v.push((Level::Err, format!("school.port 不是有效端口：{port}")));
        }

        for (key, name) in [
            ("read_timeout", "读超时"),
            ("write_timeout", "写超时"),
            ("connect_timeout", "连接超时"),
        ] {
            if let Some(x) = self
                .raw
                .object("http")
                .and_then(|h| h.get(key))
                .and_then(|v| v.as_f64())
            {
                if x <= 0.0 {
                    v.push((Level::Warn, format!("{name} http.{key} 不是正数：{x}")));
                }
            }
        }
        if let Some(w) = self
            .raw
            .object("pacing")
            .and_then(|p| p.get("window"))
            .and_then(|v| v.as_f64())
        {
            if w <= 0.0 {
                v.push((Level::Warn, format!("pacing.window 不是正数：{w}")));
            }
        }

        // 凭据
        let (sk, pk) = (&self.cred_keys.0, &self.cred_keys.1);
        let sid = self.cred.text(sk);
        let pwd = self.cred.text(pk);
        if !self.cred_exists && sid.is_empty() && pwd.is_empty() {
            v.push((
                Level::Warn,
                format!(
                    "还没有凭据文件 {} —— 不填就没法自动登录（只能用 --cookie / --url）",
                    self.cred_path.display()
                ),
            ));
        } else if sid.is_empty() || pwd.is_empty() {
            v.push((
                Level::Err,
                format!("凭据文件里缺 {sk} 或 {pk}：{}", self.cred_path.display()),
            ));
        } else if !(6..=12).contains(&sid.len()) || !sid.bytes().all(|b| b.is_ascii_digit()) {
            v.push((Level::Warn, format!("凭据里的学号不像学号：{sid}")));
        }
        self.check_cred_perms(&mut v);

        // 上面都没发现问题，再过一遍程序自己的校验（抓漏网之鱼）
        if v.iter().all(|(l, _)| *l != Level::Err) {
            let src = self.path.display().to_string();
            if let Err(e) = Config::from_raw(self.raw.clone(), &src).endpoints() {
                v.push((Level::Err, first_line(&e.0)));
            }
        }
        if v.is_empty() {
            v.push((Level::Ok, "预检全部通过，可以保存了".to_string()));
        }
        v
    }

    #[cfg(unix)]
    fn check_cred_perms(&self, v: &mut Vec<(Level, String)>) {
        use std::os::unix::fs::PermissionsExt;
        if !self.cred_exists {
            return;
        }
        if let Ok(m) = std::fs::metadata(&self.cred_path) {
            let mode = m.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                v.push((
                    Level::Warn,
                    format!(
                        "{} 权限是 {mode:o}，建议 chmod 600",
                        self.cred_path.display()
                    ),
                ));
            }
        }
    }

    #[cfg(not(unix))]
    fn check_cred_perms(&self, _v: &mut Vec<(Level, String)>) {}

    // -----------------------------------------------------------------------
    // 交互
    // -----------------------------------------------------------------------

    fn clamp(&mut self, n: usize, view_h: usize) {
        if n == 0 {
            self.cur = 0;
            self.top = 0;
            return;
        }
        if self.cur >= n {
            self.cur = n - 1;
        }
        let view = view_h.saturating_sub(1).max(1); // 右栏第一行是说明
        if self.cur < self.top {
            self.top = self.cur;
        }
        if self.cur >= self.top + view {
            self.top = self.cur + 1 - view;
        }
    }

    /// 返回 true 表示要退出程序。
    fn on_key(&mut self, k: Key, sz: (usize, usize)) -> bool {
        let body_h = body_rows(sz.1);
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Browse => {
                self.browse_key(k, body_h);
                self.quit
            }
            Mode::Edit(mut e) => {
                match k {
                    Key::Enter => {
                        if let Some(err) = self.commit(&mut e) {
                            self.msg = (err, Level::Err);
                            self.mode = Mode::Edit(e); // 留在编辑态，改完再说
                        }
                    }
                    Key::Esc | Key::Ctrl('c') => {
                        self.msg = ("已放弃这一处修改".into(), Level::Warn);
                    }
                    Key::Char(c) => {
                        e.input.insert(&c.to_string());
                        self.mode = Mode::Edit(e);
                    }
                    Key::Paste(s) => {
                        e.input.insert(&s);
                        self.mode = Mode::Edit(e);
                    }
                    Key::Backspace => {
                        e.input.backspace();
                        self.mode = Mode::Edit(e);
                    }
                    Key::Delete => {
                        e.input.delete();
                        self.mode = Mode::Edit(e);
                    }
                    Key::Left => {
                        e.input.left();
                        self.mode = Mode::Edit(e);
                    }
                    Key::Right => {
                        e.input.right();
                        self.mode = Mode::Edit(e);
                    }
                    Key::Home | Key::Ctrl('a') => {
                        e.input.pos = 0;
                        self.mode = Mode::Edit(e);
                    }
                    Key::End | Key::Ctrl('e') => {
                        e.input.pos = e.input.chars.len();
                        self.mode = Mode::Edit(e);
                    }
                    Key::Ctrl('u') => {
                        e.input.kill_to_start();
                        self.mode = Mode::Edit(e);
                    }
                    Key::Ctrl('k') => {
                        e.input.kill_to_end();
                        self.mode = Mode::Edit(e);
                    }
                    Key::Ctrl('w') => {
                        e.input.kill_word();
                        self.mode = Mode::Edit(e);
                    }
                    // 其它键（PgUp 之类）在编辑态没有意义，留在编辑态即可
                    _ => self.mode = Mode::Edit(e),
                }
                false
            }
            Mode::Info {
                page,
                title,
                scroll,
            } => {
                let view = body_h;
                let n = self.info_lines(&page, sz.0.saturating_sub(1).max(1)).len();
                let max = n.saturating_sub(view);
                let sc = match k {
                    Key::Up | Key::Char('k') => scroll.saturating_sub(1),
                    Key::Down | Key::Char('j') => (scroll + 1).min(max),
                    Key::PageUp | Key::Char('b') => scroll.saturating_sub(view),
                    Key::PageDown | Key::Char('f') | Key::Char(' ') => (scroll + view).min(max),
                    Key::Home | Key::Char('g') => 0,
                    Key::End | Key::Char('G') => max,
                    Key::Enter => return false,
                    _ => {
                        return false; // 其它键都当作"返回"
                    }
                };
                self.mode = Mode::Info {
                    page,
                    title,
                    scroll: sc,
                };
                false
            }
            Mode::Confirm { prompt, act } => {
                match k {
                    Key::Char('y') | Key::Char('Y') => match act {
                        Act::Quit => {
                            self.quit = true;
                            return true;
                        }
                        Act::Reload => self.reload(),
                        Act::RunProbe => {
                            if let Err(e) = self.start_probe() {
                                self.msg = (e, Level::Err);
                            }
                        }
                        Act::RunFetch => {
                            if let Err(e) = self.start_fetch(false) {
                                self.msg = (e, Level::Err);
                            }
                        }
                    },
                    _ => {
                        self.msg = ("已取消（什么都没发出去）".into(), Level::Warn);
                        let _ = prompt;
                    }
                }
                false
            }
            Mode::Job(mut job) => {
                // 任务跑着的时候只让转轮转 —— 但 Ctrl-C / q 仍然可以放弃（线程会自己跑完，
                // 反正都是只读请求，不会留下副作用）
                if matches!(k, Key::Esc | Key::Ctrl('c') | Key::Char('q')) {
                    self.msg = (
                        "已离开任务界面（后台那次只读请求还会自己跑完）".into(),
                        Level::Warn,
                    );
                    return false;
                }
                job.spin = job.spin.wrapping_add(1);
                self.mode = Mode::Job(job);
                false
            }
            Mode::Wizard(mut wz) => {
                let mut keep = true;
                match k {
                    Key::Enter => keep = self.wizard_enter(&mut wz),
                    Key::Esc => keep = self.wizard_back(&mut wz),
                    Key::Ctrl('c') | Key::Ctrl('q') => {
                        self.msg = (
                            "已退出向导（什么都没保存；按 w 可以重来）".into(),
                            Level::Warn,
                        );
                        keep = false;
                    }
                    Key::Char(c) => {
                        if wz.replace_on_type {
                            wz.input = Input::new("");
                            wz.replace_on_type = false;
                        }
                        wz.input.insert(&c.to_string());
                    }
                    Key::Paste(t) => {
                        if wz.replace_on_type {
                            wz.input = Input::new("");
                            wz.replace_on_type = false;
                        }
                        wz.input.insert(&t);
                    }
                    Key::Backspace => {
                        wz.replace_on_type = false;
                        wz.input.backspace();
                    }
                    Key::Delete => wz.input.delete(),
                    Key::Left => {
                        wz.replace_on_type = false;
                        wz.input.left();
                    }
                    Key::Right => {
                        wz.replace_on_type = false;
                        wz.input.right();
                    }
                    Key::Home | Key::Ctrl('a') => wz.input.pos = 0,
                    Key::End | Key::Ctrl('e') => wz.input.pos = wz.input.chars.len(),
                    Key::Ctrl('u') => wz.input.kill_to_start(),
                    Key::Ctrl('w') => wz.input.kill_word(),
                    Key::Up | Key::PageUp => wz.scroll = wz.scroll.saturating_sub(1),
                    Key::Down | Key::PageDown => wz.scroll += 1,
                    _ => {}
                }
                if keep {
                    self.mode = Mode::Wizard(wz);
                } else if matches!(self.mode, Mode::Job(_)) {
                    // 任务期间向导本体先寄存起来，跑完再接着问
                    self.wiz = Some(wz);
                }
                false
            }
            Mode::Pick(mut pick) => {
                let from_wizard = pick.from_wizard;
                let view = pick.view(body_h);
                if pick.typing {
                    // 输入筛选词：所有可打印字符都进筛选框
                    match k {
                        Key::Enter | Key::Esc | Key::Tab => pick.typing = false,
                        Key::Char(c) => {
                            pick.filter.push(c);
                            pick.rebuild();
                            pick.cur = 0;
                            pick.top = 0;
                        }
                        Key::Paste(s) => {
                            pick.filter.push_str(&s);
                            pick.rebuild();
                            pick.cur = 0;
                            pick.top = 0;
                        }
                        Key::Backspace => {
                            pick.filter.pop();
                            pick.rebuild();
                            pick.cur = 0;
                            pick.top = 0;
                        }
                        Key::Ctrl('u') => {
                            pick.filter.clear();
                            pick.rebuild();
                            pick.cur = 0;
                            pick.top = 0;
                        }
                        _ => {}
                    }
                    self.mode = Mode::Pick(pick);
                    return false;
                }
                pick.warn.clear();
                match k {
                    Key::Char('/') => pick.typing = true,
                    Key::Up | Key::Char('k') => pick.cur = pick.cur.saturating_sub(1),
                    Key::Down | Key::Char('j') => {
                        pick.cur = (pick.cur + 1).min(pick.shown.len().saturating_sub(1));
                    }
                    Key::PageUp => pick.cur = pick.cur.saturating_sub(view),
                    Key::PageDown => {
                        pick.cur = (pick.cur + view).min(pick.shown.len().saturating_sub(1))
                    }
                    Key::Home => pick.cur = 0,
                    Key::End => pick.cur = pick.shown.len().saturating_sub(1),
                    Key::Char(' ') => {
                        pick.toggle();
                        pick.cur = (pick.cur + 1).min(pick.shown.len().saturating_sub(1));
                    }
                    Key::Char('A') => {
                        // 全选/全不选**看得见的那些**（筛选后剩什么就勾什么）
                        let all = pick
                            .shown
                            .iter()
                            .all(|i| pick.checked.get(*i).copied().unwrap_or(false));
                        for i in &pick.shown {
                            if let Some(c) = pick.checked.get_mut(*i) {
                                *c = !all;
                            }
                        }
                    }
                    Key::Char(c @ '1'..='9') => {
                        // 数字键切换某一类的显示（对应上面"类型："那一行的编号）
                        let i = (c as usize) - ('1' as usize);
                        if let Some(on) = pick.type_on.get_mut(i) {
                            *on = !*on;
                            pick.rebuild();
                            pick.cur = 0;
                            pick.top = 0;
                        }
                    }
                    Key::Enter => {
                        let wrote = self.apply_pick(&pick);
                        if !wrote {
                            // 一个都没勾：**留在这一屏**（不用重新登录去重新拉），
                            // 也别往下走 —— 否则收尾页会显示一个假的候选数
                            pick.warn = "一个都没勾 —— 空格勾选，或者 Esc 放弃这次拉取".to_string();
                            self.mode = Mode::Pick(pick);
                            return false;
                        }
                        if from_wizard {
                            if let Some(mut wz) = self.wiz.take() {
                                wz.step = WStep::Done;
                                wz.picked = pick.checked_count();
                                wz.notes = Vec::new();
                                wz.err.clear();
                                self.mode = Mode::Wizard(wz);
                            }
                        }
                        return false;
                    }
                    _ => {
                        self.msg = ("已放弃这次拉取（配置没动）".into(), Level::Warn);
                        if from_wizard {
                            if let Some(mut wz) = self.wiz.take() {
                                wz.step = WStep::PickCourse;
                                wz.err = "这次拉取放弃了，配置没动".to_string();
                                self.mode = Mode::Wizard(wz);
                            }
                        }
                        return false;
                    }
                }
                pick.clamp(body_h);
                self.mode = Mode::Pick(pick);
                false
            }
        }
    }

    fn browse_key(&mut self, k: Key, body_h: usize) {
        let rows = self.rows_with_values(self.sec);
        let n = rows.len();
        match k {
            Key::Char('q') | Key::Esc | Key::Ctrl('c') => {
                if self.dirty || self.cred_dirty {
                    self.mode = Mode::Confirm {
                        prompt: "有没保存的修改，确定退出吗？".into(),
                        act: Act::Quit,
                    };
                } else {
                    self.quit = true;
                }
            }
            Key::Up | Key::Char('k') => {
                self.cur = self.cur.saturating_sub(1);
                self.clamp(n, body_h);
            }
            Key::Down | Key::Char('j') => {
                self.cur += 1;
                self.clamp(n, body_h);
            }
            Key::PageUp => {
                self.cur = self.cur.saturating_sub(body_h);
                self.clamp(n, body_h);
            }
            Key::PageDown => {
                self.cur += body_h;
                self.clamp(n, body_h);
            }
            Key::Home => {
                self.cur = 0;
                self.clamp(n, body_h);
            }
            Key::End => {
                self.cur = n.saturating_sub(1);
                self.clamp(n, body_h);
            }
            Key::Left | Key::BackTab => {
                self.sec = (self.sec + SEC_TITLES.len() - 1) % SEC_TITLES.len();
                self.reset_pos();
            }
            Key::Right | Key::Tab => {
                self.sec = (self.sec + 1) % SEC_TITLES.len();
                self.reset_pos();
            }
            Key::Char(c @ '1'..='9') => {
                self.sec = (c as usize) - ('1' as usize);
                self.reset_pos();
            }
            Key::Char('0') => {
                self.sec = 9;
                self.reset_pos();
            }
            Key::Enter => {
                if let Some(r) = rows.get(self.cur).cloned() {
                    if let Some(act) = r.act {
                        self.run_act(act);
                    } else if let Some(f) = r.field {
                        let text = f
                            .path
                            .get(self.root(f.target))
                            .map(|v| v.as_text())
                            .unwrap_or_default();
                        self.mode = Mode::Edit(Box::new(Edit {
                            input: Input::new(&text),
                            target: f.target,
                            path: f.path,
                            label: r.label.clone(),
                            ty: f.ty,
                            secret: f.secret,
                        }));
                    } else {
                        self.msg = ("这一行只是说明，不能编辑".into(), Level::Warn);
                    }
                }
            }
            Key::Char('a') => match self.target_list(&rows) {
                Some((sec, list)) if list.is_empty() && sec.starts_with("courses[") => {
                    let ci = sec
                        .trim_start_matches("courses[")
                        .split(']')
                        .next()
                        .and_then(|t| t.parse().ok())
                        .unwrap_or(self.cur_course);
                    self.add_course_cand(ci)
                }
                Some((sec, list)) => self.add_item(&sec, &list),
                None => self.msg = ("这一节没有可添加的列表".into(), Level::Warn),
            },
            Key::Char('d') | Key::Delete => {
                let pos = rows
                    .get(self.cur)
                    .and_then(|r| r.field.as_ref())
                    .and_then(|f| f.path.list_pos());
                let cand_pos = rows
                    .get(self.cur)
                    .and_then(|r| r.field.as_ref())
                    .and_then(|f| f.path.course_cand_pos());
                if let Some((ci, j)) = cand_pos {
                    if let Some(c) = self.raw.ensure_arr("courses").get_mut(ci) {
                        let a = c.ensure_arr("candidates");
                        if j < a.len() {
                            a.remove(j);
                            self.dirty = true;
                            self.msg = (
                                format!("已删掉第 {} 门课的候选[{j}]（记得 s 保存）", ci + 1),
                                Level::Warn,
                            );
                        }
                    }
                    let n = self.rows_with_values(self.sec).len();
                    self.clamp(n, body_h);
                    return;
                }
                match pos {
                    Some((sec, list, i)) => {
                        let a = self.raw.ensure_obj(&sec).ensure_arr(&list);
                        if i < a.len() {
                            a.remove(i);
                            self.dirty = true;
                            self.msg = (
                                format!("已删掉 {sec}.{list}[{i}]（记得 s 保存）"),
                                Level::Warn,
                            );
                        }
                        let n = self.rows_with_values(self.sec).len();
                        self.clamp(n, body_h);
                    }
                    None => {
                        self.msg = (
                            "这一行不是列表项，删不了（按 Enter 可以清空它）".into(),
                            Level::Warn,
                        )
                    }
                }
            }
            Key::Char('s') => self.save(),
            Key::Char('v') => self.show_info(InfoPage::Problems, "配置校验"),
            Key::Char('w') => self.start_wizard(WStep::Url),
            Key::Char('r') => {
                if self.dirty || self.cred_dirty {
                    self.mode = Mode::Confirm {
                        prompt: format!("从磁盘重读 {}，丢弃当前修改？", self.path.display()),
                        act: Act::Reload,
                    };
                } else {
                    self.reload();
                }
            }
            Key::Char('?') | Key::Char('h') => self.show_info(InfoPage::Help, "帮助"),
            _ => {}
        }
    }

    /// `a` 键往哪个列表加：优先光标所在的那一行，其次这一节里的第一个列表。
    fn target_list(&self, rows: &[Row]) -> Option<(String, String)> {
        let from_row = |r: &Row| -> Option<(String, String)> {
            match &r.act {
                Some(RowAct::Add { sec, list }) => Some((sec.clone(), list.clone())),
                // 课程候选列表：走 add_course_cand，不用 {sec}.{list} 那套字符串路径
                Some(RowAct::AddCourseCand(ci)) => {
                    Some((format!("courses[{ci}].candidates"), String::new()))
                }
                _ => r
                    .field
                    .as_ref()
                    .and_then(|f| f.path.list_pos())
                    .map(|(s, l, _)| (s, l)),
            }
        };
        rows.get(self.cur)
            .and_then(from_row)
            .or_else(|| rows.iter().find_map(from_row))
    }

    fn reset_pos(&mut self) {
        self.sec = self.sec.min(SEC_TITLES.len() - 1);
        self.cur = 0;
        self.top = 0;
    }

    /// 往第 `ci` 门课的候选列表里追加一项（并在界面上跳到它）。
    ///
    /// 和 `add_item` 同一套做法，只是路径多一层课程下标。
    fn add_course_cand(&mut self, ci: usize) {
        let entry = Json::obj(vec![
            ("id", Json::Str(String::new())),
            ("label", Json::Str(String::new())),
            ("group", Json::Str(String::new())),
        ]);
        let idx = {
            let courses = self.raw.ensure_arr("courses");
            if ci >= courses.len() {
                return;
            }
            let a = courses[ci].ensure_arr("candidates");
            a.push(entry);
            a.len() - 1
        };
        self.dirty = true;
        let want = Path::CourseCand(ci, idx, "id");
        let rows = self.rows_with_values(self.sec);
        if let Some(pos) = rows
            .iter()
            .position(|r| r.field.as_ref().map(|f| f.path.clone()) == Some(want.clone()))
        {
            self.cur = pos;
        }
        self.msg = (
            format!(
                "已给第 {} 门课加了候选[{idx}]：填 id / label / group",
                ci + 1
            ),
            Level::Warn,
        );
    }

    fn add_item(&mut self, sec: &str, list: &str) {
        let fields = list_fields(sec, list);
        let new = if fields.is_empty() {
            Json::Str(String::new())
        } else {
            Json::Obj(
                fields
                    .iter()
                    .map(|k| ((*k).to_string(), Json::Str(String::new())))
                    .collect(),
            )
        };
        let idx = {
            let a = self.raw.ensure_obj(sec).ensure_arr(list);
            a.push(new);
            a.len() - 1
        };
        self.dirty = true;

        let sub = fields.first().copied().unwrap_or("");
        let want = if sub.is_empty() {
            Path::Item(sec.to_string(), list.to_string(), idx)
        } else {
            Path::ItemKey(sec.to_string(), list.to_string(), idx, sub.to_string())
        };
        let rows = self.rows_with_values(self.sec);
        self.cur = rows
            .iter()
            .position(|r| r.field.as_ref().map(|f| f.path.clone()) == Some(want.clone()))
            .unwrap_or(0);
        self.top = 0;
        // 新加的一项本来就是空的，直接进编辑态 —— 没理由让用户再按一次 Enter
        let label = if sub.is_empty() {
            format!("{list}[{idx}]")
        } else {
            format!("{list}[{idx}].{sub}")
        };
        self.mode = Mode::Edit(Box::new(Edit {
            target: Target::Config,
            path: want,
            label,
            ty: Ty::Text,
            secret: false,
            input: Input::new(""),
        }));
        self.msg = (format!("已在 {sec}.{list} 末尾新增一项"), Level::Ok);
    }

    // -----------------------------------------------------------------------
    // 动作行：向导 / 粘网址 / 联网自检 / 拉候选教学班
    // -----------------------------------------------------------------------

    fn run_act(&mut self, act: RowAct) {
        match act {
            RowAct::Add { sec, list } => self.add_item(&sec, &list),
            RowAct::AddCourseCand(ci) => self.add_course_cand(ci),
            RowAct::NextCourse => {
                let n = self.raw.array("courses").len().max(1);
                self.cur_course = (self.cur_course + 1) % n;
                self.msg = if n == 1 {
                    (
                        "只有一门目标课程；要加就按「＋ 再加一门」".into(),
                        Level::Warn,
                    )
                } else {
                    (
                        format!("正在编辑第 {}/{} 门课程", self.cur_course + 1, n),
                        Level::Ok,
                    )
                };
            }
            RowAct::AddCourse => {
                self.raw.ensure_arr("courses").push(Json::obj(vec![
                    ("name", Json::str("")),
                    ("keyword", Json::str("")),
                    ("class_type", Json::str("")),
                    ("is_major", Json::str("1")),
                    ("query_content", Json::str("{keyword}")),
                    ("candidates", Json::Arr(Vec::new())),
                ]));
                let n = self.raw.array("courses").len();
                self.cur_course = n - 1;
                self.dirty = true;
                self.msg = (
                    format!("已加第 {n} 门课程：填好名字/关键词，再「⇣ 拉课程目录」挑班"),
                    Level::Warn,
                );
            }
            RowAct::DelCourse => {
                let n = self.raw.array("courses").len();
                if n <= 1 {
                    self.msg = ("至少要留一门目标课程".into(), Level::Warn);
                } else {
                    let i = self.cur_course.min(n - 1);
                    self.raw.ensure_arr("courses").remove(i);
                    self.cur_course = i.min(n - 2);
                    self.dirty = true;
                    self.msg = (
                        format!("已删掉第 {} 门课程（记得 s 保存）", i + 1),
                        Level::Warn,
                    );
                }
            }
            RowAct::Wizard => self.start_wizard(WStep::Url),
            RowAct::WizardUrl => self.start_wizard(WStep::Url),
            RowAct::Probe => {
                if self
                    .raw
                    .object("school")
                    .map(|s| s.text("host"))
                    .unwrap_or_default()
                    .is_empty()
                {
                    self.msg = (
                        "先把域名填上（或者粘一条选课网址）再探测".into(),
                        Level::Warn,
                    );
                    return;
                }
                let host = self.host_port();
                self.mode = Mode::Confirm {
                    prompt: format!("要访问 {host} 做只读探测吗？不登录、不提交，只发几个 GET"),
                    act: Act::RunProbe,
                };
            }
            RowAct::FetchCandidates => {
                let (sk, pk) = (&self.cred_keys.0, &self.cred_keys.1);
                if self.cred.text(sk).trim().is_empty() || self.cred.text(pk).trim().is_empty() {
                    self.msg = (
                        "先去「凭据」一节填学号和密码 —— 拉课程目录要先登录".into(),
                        Level::Warn,
                    );
                    return;
                }
                let host = self.host_port();
                let who = crate::auth::mask_id(&self.cred.text(sk));
                self.mode = Mode::Confirm {
                    prompt: format!("会用 {who} 登录 {host} 查课程目录（只读），继续吗？"),
                    act: Act::RunFetch,
                };
            }
        }
    }

    fn host_port(&self) -> String {
        self.raw
            .object("school")
            .map(|s| {
                let h = s.text("host");
                let p = s.get("port").and_then(|v| v.as_i64()).unwrap_or(80);
                if p == 80 {
                    h
                } else {
                    format!("{h}:{p}")
                }
            })
            .unwrap_or_else(|| "（域名还没填）".to_string())
    }

    /// 当前配置能不能凑出端点 —— 联网动作都要先过这一关。
    fn endpoints(&self) -> Result<std::sync::Arc<crate::config::Endpoints>, String> {
        let cfg = Config::from_raw(self.raw.clone(), &self.path.display().to_string());
        cfg.endpoints()
            .map(std::sync::Arc::new)
            .map_err(|e| format!("配置还不完整，联网前先补上：{}", first_line(&e.0)))
    }

    fn show_info(&mut self, page: InfoPage, title: &str) {
        self.mode = Mode::Info {
            page,
            title: title.to_string(),
            scroll: 0,
        };
    }

    /// 开始一个后台任务：库里的 `info()` 日志会实时流回界面，界面不卡。
    fn start_job<F>(&mut self, title: &str, next: JobNext, work: F)
    where
        F: FnOnce() -> JobDone + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let log_tx = tx.clone();
        let done_tx = tx;
        std::thread::spawn(move || {
            let done = crate::log::with_sink(
                move |m: &str| {
                    let _ = log_tx.send(JobMsg::Log(m.to_string()));
                },
                work,
            );
            let _ = done_tx.send(JobMsg::Done(Box::new(done)));
        });
        self.mode = Mode::Job(Box::new(Job {
            title: title.to_string(),
            rx,
            lines: Vec::new(),
            spin: 0,
            next,
        }));
    }

    fn start_probe(&mut self) -> Result<(), String> {
        let ep = self.endpoints()?;
        self.start_job("联网自检（只读）", JobNext::Info, move || {
            JobDone::Probe(onboard::probe(&ep))
        });
        Ok(())
    }

    fn start_fetch(&mut self, from_wizard: bool) -> Result<(), String> {
        let ep = self.endpoints()?;
        let (sk, pk) = (&self.cred_keys.0, &self.cred_keys.1);
        let creds = Credentials {
            student_id: self.cred.text(sk).trim().to_string(),
            password: self.cred.text(pk),
            source: self.cred_path.display().to_string(),
        };
        if self.cred.text(sk).trim().is_empty() || self.cred.text(pk).trim().is_empty() {
            return Err("先去「凭据」一节填学号和密码 —— 拉课程目录要先登录".to_string());
        }
        let (model, min_margin, w, h) = self.captcha_settings();
        let next = if from_wizard {
            JobNext::WizardFetch
        } else {
            JobNext::Info
        };
        self.start_job(
            "拉课程目录（把每一类都拉下来）",
            next,
            move || {
                let solver = match crate::captcha::Captcha::new(model.as_deref(), min_margin, w, h)
                {
                    Ok(c) => c,
                    Err(e) => return JobDone::Fetch(Err(e)),
                };
                crate::log::log(&format!("识别模型：{}", solver.describe()));
                JobDone::Fetch(onboard::fetch_catalog(&ep, &creds, &solver, 6, 2.0))
            },
        );
        Ok(())
    }

    /// 从配置里取验证码参数（拉目录时要用内编模型解验证码）。
    fn captcha_settings(&self) -> (Option<String>, f64, i64, i64) {
        let c = self.raw.object("captcha");
        let get = |k: &str| c.and_then(|c| c.get(k));
        let model = get("model_file")
            .map(|v| v.as_text())
            .filter(|s| !s.is_empty());
        (
            model,
            get("min_margin").and_then(|v| v.as_f64()).unwrap_or(0.0),
            get("width").and_then(|v| v.as_i64()).unwrap_or(250),
            get("height").and_then(|v| v.as_i64()).unwrap_or(80),
        )
    }

    /// 收后台任务的消息。返回 true 表示"该重画了"。
    fn poll_job(&mut self) -> bool {
        let Mode::Job(job) = &mut self.mode else {
            return false;
        };
        let mut done: Option<JobDone> = None;
        let mut disconnected = false;
        loop {
            match job.rx.try_recv() {
                Ok(JobMsg::Log(l)) => job.lines.push((Level::Ok, l)),
                Ok(JobMsg::Done(d)) => {
                    done = Some(*d);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.msg = ("后台任务意外结束（线程没了）".into(), Level::Err);
            self.mode = Mode::Browse;
            return true;
        }
        if let Some(done) = done {
            // 把 Job 换出来，顺便拿到它攒下的日志
            let job = match std::mem::replace(&mut self.mode, Mode::Browse) {
                Mode::Job(j) => j,
                other => {
                    self.mode = other;
                    return false;
                }
            };
            self.finish_job(done, job.lines, job.next);
            return true;
        }
        if let Mode::Job(job) = &mut self.mode {
            job.spin = job.spin.wrapping_add(1); // 转轮动一下
        }
        true
    }

    fn finish_job(&mut self, done: JobDone, lines: Vec<Note>, next: JobNext) {
        match done {
            JobDone::Discover(result) => {
                self.finish_discover(result, lines);
            }
            JobDone::Probe(notes) => {
                // probe 自己也把每一行写了日志 —— 两条路都兜住，别丢报告
                let lines = if notes.is_empty() { lines } else { notes };
                self.show_info(InfoPage::Notes(lines), "联网自检结果");
            }
            JobDone::Fetch(Ok(fetched)) => {
                let from_wizard = next == JobNext::WizardFetch;
                let n = fetched.rows.len();
                // 服务器实际下发的会话 Cookie 名可能比配置里列的全（脱敏后的示例配置
                // 就少了 `_WEU`）：现场发现就写进配置，并在勾选页上说出来。
                let note = self.apply_session_cookies(&fetched.session_cookies);
                let mut checked = vec![false; n];
                // 已经在配置里的教学班默认勾上（方便确认/取消）
                let existing: Vec<String> = self
                    .raw
                    .object("course")
                    .map(|c| c.array("candidates").iter().map(|x| x.text("id")).collect())
                    .unwrap_or_default();
                for (i, r) in fetched.rows.iter().enumerate() {
                    if existing.iter().any(|id| id == &r.tc_id) {
                        checked[i] = true;
                    }
                }
                self.raw
                    .ensure_obj("course")
                    .set_key("campus", Json::str(fetched.campus.clone()));
                self.mode = Mode::Pick(Box::new(Pick::new(fetched, checked, from_wizard, note)));
            }
            JobDone::Fetch(Err(e)) => {
                if let Some(mut w) = self.wiz.take() {
                    w.err = e;
                    self.mode = Mode::Wizard(w);
                    return;
                }
                let mut lines = lines;
                lines.push((Level::Err, e));
                lines.push((
                    Level::Warn,
                    "常见原因：学号密码不对、学校把自动登录关了、现在不是选课时间、\
                     或者域名/接口前缀填错了（先在「学校」一节按「联网自检」探一下）"
                        .to_string(),
                ));
                self.show_info(InfoPage::Notes(lines), "拉取失败");
            }
        }
    }

    /// 把登录时实测到的会话 Cookie 名单写进配置。返回一句"干了什么"（没动就空串）。
    fn apply_session_cookies(&mut self, observed: &[String]) -> String {
        if observed.is_empty() {
            return String::new();
        }
        let cur: Vec<String> = self
            .raw
            .object("cookies")
            .map(|c| c.array("session").iter().map(|x| x.as_text()).collect())
            .unwrap_or_default();
        if cur == observed {
            return String::new();
        }
        self.raw.ensure_obj("cookies").set_key(
            "session",
            Json::Arr(observed.iter().cloned().map(Json::Str).collect()),
        );
        self.dirty = true;
        format!(
            "登录时服务器实际下发了这些会话 Cookie：{} —— 已把 cookies.session 补成这份名单",
            observed.join("、")
        )
    }

    /// 勾选结果写进配置。返回是否真的写了（一个都没勾就是 false）。
    fn apply_pick(&mut self, pick: &Pick) -> bool {
        let picked = pick.picked();
        if picked.is_empty() {
            self.msg = (
                "一个都没勾 —— 空格勾选，或者 Esc 放弃这次拉取".into(),
                Level::Warn,
            );
            return false;
        }
        // 选了哪几类，就把 `course.class_type` 定成第一类 —— 抢课前那次目录查询用它
        // （每个候选自己还带着 `type`，所以跨类型也能提交）
        self.raw
            .ensure_obj("course")
            .set_key("class_type", Json::str(picked[0].tc_type.clone()));
        // 关键词：勾的课名字都一样就写进去（抢课时的白名单标签用它）
        let names: Vec<String> = {
            let mut v: Vec<String> = Vec::new();
            for f in &picked {
                if !v.contains(&f.course_name) {
                    v.push(f.course_name.clone());
                }
            }
            v
        };
        let keyword = if names.len() == 1 {
            names[0].clone()
        } else {
            String::new()
        };
        // 写进"当前正在编辑的那门课"（多课程时这里曾经会写错门）
        let ci = self.cur_course;
        let n = onboard::write_candidates(&mut self.raw, ci, &picked, &keyword);
        self.dirty = true;
        let need_group = picked.iter().filter(|r| r.group.is_empty()).count();
        let mut msg = format!("已写入第 {} 门课的 {n} 个候选教学班", ci + 1);
        if need_group > 0 {
            msg.push_str(&format!(
                "；其中 {need_group} 个推不出冲突组（上课地点里没写星期节次），抢课时会被当成各自独立的一组"
            ));
        }
        self.sec = 5;
        self.cur = 0;
        self.top = 0;
        self.mode = Mode::Browse;
        true
    }

    /// 提交一次编辑。返回 `Some(错误)` 表示要留在编辑态。
    fn commit(&mut self, e: &mut Edit) -> Option<String> {
        let text = e.input.text();
        let val = match e.ty {
            Ty::Text => Json::Str(text.clone()),
            Ty::Int => match text.trim().parse::<i64>() {
                Ok(v) => Json::Int(v),
                Err(_) => return Some(format!("「{}」不是整数", text.trim())),
            },
            Ty::Float => match text.trim().parse::<f64>() {
                Ok(v) => Json::Float(v),
                Err(_) => return Some(format!("「{}」不是数字", text.trim())),
            },
        };
        let creds_path_changed =
            e.target == Target::Config && e.path == Path::top("credentials_path");
        e.path.set(self.root_mut(e.target), val);
        match e.target {
            Target::Config => self.dirty = true,
            Target::Cred => self.cred_dirty = true,
        }
        if creds_path_changed {
            self.reload_credentials(None); // 指向别的文件了，跟着换一份
        }
        let what = if e.secret {
            format!("{} 已更新", e.label)
        } else if e.input.is_empty() {
            format!("{} 已清空 → 回到程序默认值", e.label)
        } else {
            format!("{} = {}", e.label, textw::clip(&text, 48).0)
        };
        self.msg = (format!("{what}（s 保存）"), Level::Ok);
        None
    }

    fn save(&mut self) {
        if let Err(e) = config::write_config(&self.path, &self.raw) {
            self.msg = (format!("保存失败：{e}"), Level::Err);
            return;
        }
        self.dirty = false;
        let mut written = vec![self.path.display().to_string()];
        let mut backup = None;
        if self.cred_dirty {
            match self.write_credentials() {
                Ok(bak) => {
                    self.cred_dirty = false;
                    self.cred_exists = true;
                    written.push(self.cred_path.display().to_string());
                    backup = bak;
                }
                Err(e) => {
                    self.msg = (format!("配置已保存，但凭据没写成：{e}"), Level::Err);
                    return;
                }
            }
        }
        self.fresh = false;
        let bad = self
            .problems()
            .iter()
            .filter(|(l, _)| *l == Level::Err)
            .count();
        let tail = if bad == 0 {
            "（校验通过）".to_string()
        } else {
            format!("（还有 {bad} 处待修，按 v 看）")
        };
        let lv = if bad == 0 { Level::Ok } else { Level::Warn };
        let bak = match backup {
            Some(p) => format!("（原来那份凭据留在了 {}）", p.display()),
            None => String::new(),
        };
        self.msg = (format!("已保存 {} {tail}{bak}", written.join(" 与 ")), lv);
    }

    /// 写凭据文件。返回被备份下来的那个路径（没备份就是 None）。
    fn write_credentials(&self) -> Result<Option<PathBuf>, String> {
        if let Some(dir) = self.cred_path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("建目录失败 {}: {e}", dir.display()))?;
            }
        }
        let body = if self.cred_wrap {
            Json::obj(vec![("credentials", self.cred.clone())])
        } else {
            self.cred.clone()
        };
        let text = format!("{}\n", body.to_pretty());

        // 覆盖前先留一份 .bak：密码不像配置，写错了没法凭记忆恢复。
        // 备份失败不阻断保存（只是少一层保险），但会把路径报给用户。
        let mut backup = None;
        if let Ok(old) = std::fs::read_to_string(&self.cred_path) {
            if old != text {
                let bak = self.cred_path.with_extension("json.bak");
                if std::fs::write(&bak, &old).is_ok() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&bak, std::fs::Permissions::from_mode(0o600));
                    }
                    backup = Some(bak);
                }
            }
        }
        write_secret(&self.cred_path, &text)
            .map_err(|e| format!("写不了 {}: {e}", self.cred_path.display()))?;
        Ok(backup)
    }

    fn reload(&mut self) {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        match json::parse(&text) {
            Ok(v @ Json::Obj(_)) => {
                self.raw = v;
                self.dirty = false;
                self.reload_credentials(None);
                self.cred_dirty = false;
                self.msg = (format!("已重新载入 {}", self.path.display()), Level::Ok);
            }
            _ => {
                self.msg = (
                    format!("{} 读不出来或不是 JSON 对象", self.path.display()),
                    Level::Err,
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 渲染工具
// ---------------------------------------------------------------------------

/// 拼一行时的游标容器：按显示宽度自动截断，所以排版代码不用自己数格子。
struct Line {
    out: String,
    cols: usize,
    max: usize,
    color: bool,
}

impl Line {
    fn new(max: usize, color: bool) -> Line {
        Line {
            out: String::new(),
            cols: 0,
            max,
            color,
        }
    }

    fn raw(&mut self, text: &str) {
        self.seg(self.color, "", text);
    }

    fn seg(&mut self, color: bool, sgr: &str, text: &str) {
        let left = self.max.saturating_sub(self.cols);
        if left == 0 || text.is_empty() {
            return;
        }
        // 只剩一列：放不下内容，也放不下"这里被截断了"的省略号 —— 干脆不写，
        // 否则行尾会挂出一串莫名其妙的「……」
        if left < 2 && textw::width(text) > left {
            return;
        }
        let (shown, used) = textw::clip(text, left);
        self.cols += used;
        if color && !sgr.is_empty() {
            self.out.push_str("\x1b[");
            self.out.push_str(sgr);
            self.out.push('m');
            self.out.push_str(&shown);
            self.out.push_str("\x1b[0m");
        } else {
            self.out.push_str(&shown);
        }
    }

    fn finish(self) -> String {
        self.out
    }
}

/// 主体区（左栏 + 右栏）有几行。标题、状态行、按键提示各占一行。
fn body_rows(rows_h: usize) -> usize {
    rows_h.max(4).saturating_sub(3)
}

fn display_value(cur: Option<&Json>, secret: bool, def: Option<&Json>) -> (String, bool) {
    let raw = cur.map(|v| v.as_text()).unwrap_or_default();
    if secret {
        return if raw.is_empty() {
            ("（未设置）".to_string(), true)
        } else {
            ("••••••••".to_string(), false)
        };
    }
    if raw.is_empty() {
        let d = def.map(|d| d.as_text()).unwrap_or_default();
        return if d.is_empty() {
            ("（未设置）".to_string(), true)
        } else {
            (format!("（默认 {d}）"), true)
        };
    }
    (raw, false)
}

/// 取错误信息的第一行（config.rs 的完整信息里带"照模板填一份"那种续行）。
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn read_text(p: &FsPath) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("读不了配置 {}: {e}", p.display()))
}

fn parse_config(text: &str, p: &FsPath) -> Result<Json, String> {
    match json::parse(text) {
        Ok(v @ Json::Obj(_)) => Ok(v),
        Ok(_) => Err(format!("{} 应该是一个 JSON 对象", p.display())),
        Err(_) => Err(format!(
            "{} 不是合法的 JSON。修好它，或者删掉该文件让界面按模板重建",
            p.display()
        )),
    }
}

fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// 凭据文件按 0600 写（`mode` 只在创建时生效，已存在的再 chmod 一次）。
#[cfg(unix)]
fn write_secret(path: &FsPath, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

#[cfg(not(unix))]
fn write_secret(path: &FsPath, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
}

// ---------------------------------------------------------------------------
// 静态表格
// ---------------------------------------------------------------------------

/// `paths` 里程序真正会用到的键。顺序与 config.rs 的报错列表一致。
const PATH_KEYS: &[(&str, &str)] = &[
    ("volunteer", "选课提交"),
    ("capacity", "余量查询"),
    ("result", "选课结果"),
    ("status", "学生状态"),
    ("sysparam", "系统参数"),
    ("program", "课程目录"),
    ("student", "学生信息"),
    ("vcode_token", "验证码 token"),
    ("vcode_image", "验证码图片"),
    ("login", "登录"),
];

fn help_lines() -> &'static [&'static str] {
    &[
        "course-grabber 配置编辑器",
        "",
        "只做一件事：帮你把 config.json 填对。",
        "联网的动作只有两个（联网自检、拉课程目录），每次都会先问你。",
        "",
        "第一次用：按 w 进「配置向导」—— 一步一步问（学校地址 → 学号 → 密码 → 课程名）。",
        "地址那一步粘网址或**只写域名**都行，不需要知道任何接口路径。",
        "",
        "按键",
        "  ↑ ↓ / j k     上下移动（光标停在哪一行，改的就是哪一行）",
        "  ← → / Tab     切换左边的段落（1-9、0 直接跳）",
        "  PgUp / PgDn   翻页      Home/End 到首/尾",
        "  Enter         编辑这一行；在「⤓ / ⇣ / ＋」开头的行上按就是执行那个动作",
        "  a             往当前列表追加一项",
        "  d             删除光标所在的列表项（列表外的行删不了）",
        "  s             保存（config.json 和改过的凭据文件一起写）",
        "  v             校验：列出还差什么、哪里可疑",
        "  w             配置向导（一步一步问，联网前会先确认）",
        "  r             丢弃修改，从磁盘重新读",
        "  q / Esc       退出（有没保存的修改会先问你）",
        "  ?             本页",
        "",
        "编辑时",
        "  Enter 确定 · Esc 取消 · Ctrl-U 清空 · Ctrl-W 删一个词",
        "  支持终端括号粘贴：整段 User-Agent、网址、几十位教学班 ID 都能一次贴进来",
        "  数字项在确定时检查格式，写错了不会退出编辑",
        "",
        "三条联网的路（都会先问，而且都是只读的）",
        "  ⤓ 粘贴选课网址   从浏览器地址栏粘一条 URL（只写域名也行），自动填出域名/端口/",
        "                    接口前缀/页面路径/对时页 —— 每条都实测验证过，不是猜的；",
        "                    文件里缺的键还会照参考实现补齐（你写过的键一个字都不动）",
        "  ⇣ 拉课程目录     用你的学号密码登录，把**每一类课都拉下来**（方案内/方案外/",
        "                    校公选/体育/慕课），按类型或名字筛，空格勾选、Enter 写入；",
        "                    冲突组自动从上课地点推出来，类型跟着每个候选一起写进配置",
        "  ⇅ 联网自检       只发几个 GET，探一下这些接口路径真的存在 —— 不登录、不提交",
        "",
        "几条要知道的",
        "  · 只有「学校」这一节必须改：域名、base_path、page_path。粘一条网址就能填好。",
        "  · 接口路径（paths）不能空；粘网址时如果缺，会自动照参考实现补上。",
        "  · 候选教学班的 group 是冲突组（星期-节次）：每个时段最多中一个，",
        "    每门课也最多中一个（跨课程的时间冲突就落在同一个组名里）—— 所以不会双选。",
        "  · 多门课：抢到一门之后其余继续轮转，全部抢齐才退出。",
        "  · password.des_keys 是学校前端加密密码用的密钥，顺序敏感，最多 3 组。",
        "  · 灰字的「（默认 …）」表示文件里没写这个键、程序现在用的就是这个值；",
        "    按 Enter 改成别的才会写进文件，清空则回到默认。",
        "  · 说明书性质的键（_comment 之类）原样保留，界面不显示也不改动。",
        "",
        "编辑完建议：",
        "  1) 先跑 `course-grabber`（不加 --live）做只读预检；",
        "  2) 预检通过再 `course-grabber --live`。",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_of(s: &str) -> Json {
        json::parse(s).unwrap()
    }

    fn app_with(raw: Json, cred: Json, cred_exists: bool) -> App {
        App {
            path: PathBuf::from("/tmp/cg-test/config.json"),
            raw,
            defaults: config::engine_defaults(),
            dirty: false,
            fresh: false,
            cred_path: PathBuf::from("/tmp/cg-test/credentials.json"),
            cred,
            cred_keys: ("student_id".into(), "password".into()),
            cred_wrap: false,
            cred_exists,
            cred_dirty: false,
            wiz: None,
            sec: 0,
            cur: 0,
            cur_course: 0,
            top: 0,
            mode: Mode::Browse,
            msg: (String::new(), Level::Ok),
            quit: false,
            color: false,
        }
    }

    fn example_app() -> App {
        app_with(
            json::parse(config::CONFIG_EXAMPLE).unwrap(),
            json_of(r#"{"student_id":"2026000001","password":"p"}"#),
            true,
        )
    }

    /// 覆盖已有凭据之前要留 .bak —— 密码写错了没法凭记忆恢复。
    ///
    /// （这不是假想：写这个功能时，一次端到端测试真的把测试用的假学号写进了
    ///  `~/.config/course-grabber/credentials.json` —— 目录是那次测试新建的，所以没伤到人，
    ///  但换成真实场景就是"密码被覆盖且无法恢复"。）
    fn found(tc_type: &str, course: &str, i: usize) -> onboard::Found {
        onboard::Found {
            tc_id: format!("{tc_type}-{i}"),
            tc_type: tc_type.to_string(),
            course_name: course.to_string(),
            index: format!("0{i}"),
            teacher: "张老师".to_string(),
            place: "星期三第3-5节 一教101".to_string(),
            group: "星期三-3-5".to_string(),
            credit: "3.0".to_string(),
            is_full: "0".to_string(),
            is_conflict: "0".to_string(),
        }
    }

    fn pick_of(rows: Vec<onboard::Found>) -> Pick {
        let n = rows.len();
        let fetched = onboard::Fetched {
            student_name: "测试同学".to_string(),
            batch_code: "B1".to_string(),
            batch_name: "正选".to_string(),
            campus: "01".to_string(),
            rows,
            session_cookies: vec![],
            types: vec![],
        };
        Pick::new(fetched, vec![false; n], true, String::new())
    }

    /// 挑课屏的三件事：按类型筛、按名字筛、勾选状态不受筛选影响。
    #[test]
    fn pick_filters_and_keeps_selection() {
        let mut p = pick_of(vec![
            found("FANKC", "线性代数", 0),
            found("FAWKC", "线性代数", 1),
            found("XGXK", "高等数学", 2),
        ]);
        assert_eq!(p.shown.len(), 3);

        // 按名字筛：只剩两门叫"线性代数"的（跨两个类型）
        p.filter = "线性".to_string();
        p.rebuild();
        assert_eq!(p.shown, vec![0, 1]);
        // 教师名也能搜
        p.filter = "张老师".to_string();
        p.rebuild();
        assert_eq!(p.shown.len(), 3);
        // 类型名也能搜
        p.filter = "校公选".to_string();
        p.rebuild();
        assert_eq!(p.shown, vec![2]);

        // 勾选跟着"行"走，不跟着"筛选结果"走
        p.checked[0] = true;
        p.filter = "高等数学".to_string();
        p.rebuild();
        assert_eq!(p.shown, vec![2]);
        assert!(p.checked[0], "换了筛选词，之前的勾不能丢");

        // 关掉某一类：那一类的行就不显示了，但勾还在
        p.filter.clear();
        p.rebuild();
        p.type_on[2] = false; // 关掉校公选
        p.rebuild();
        assert_eq!(p.shown, vec![0, 1]);
        assert!(p.checked[0]);
    }

    /// `A` 只作用于**看得见的**那些行；空格只切换光标那一行。
    #[test]
    fn pick_selects_only_what_is_visible() {
        let mut p = pick_of(vec![
            found("FANKC", "线性代数", 0),
            found("FAWKC", "线性代数", 1),
            found("XGXK", "高等数学", 2),
        ]);
        // 只看方案内 → A 只勾那一行
        p.type_on[1] = false;
        p.type_on[2] = false;
        p.rebuild();
        assert_eq!(p.shown, vec![0]);
        let all = p
            .shown
            .iter()
            .all(|i| p.checked.get(*i).copied().unwrap_or(false));
        for i in &p.shown {
            if let Some(c) = p.checked.get_mut(*i) {
                *c = !all;
            }
        }
        assert_eq!(p.checked, vec![true, false, false]);

        // 空格切换光标那一行（并且自动往下走一格）
        p.cur = 0;
        p.toggle();
        assert!(!p.checked[0]);
        p.toggle();
        assert!(p.checked[0]);

        // 勾了跨类型的两个：picked() 按**全量顺序**返回，类型各自带着
        p.checked = vec![true, false, true];
        let picked = p.picked();
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].tc_type, "FANKC");
        assert_eq!(picked[1].tc_type, "XGXK");
        assert_eq!(p.checked_count(), 2);
    }

    #[test]
    fn writing_credentials_backs_up_the_old_one() {
        let dir = std::env::temp_dir().join(format!("cg-cred-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(
            &path,
            "{\n  \"student_id\": \"2026000001\",\n  \"password\": \"old\"\n}\n",
        )
        .unwrap();

        let mut app = app_with(json::parse("{}").unwrap(), json_of("{}"), true);
        app.cred_path = path.clone();
        app.cred = json_of(r#"{"student_id":"2026999999","password":"new"}"#);
        let bak = app
            .write_credentials()
            .expect("写得下去")
            .expect("该留备份");

        // 旧内容进 .bak，新内容进正式文件，两份都是 0600
        let old = std::fs::read_to_string(&bak).unwrap();
        assert!(old.contains("2026000001"), "{old}");
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(new.contains("2026999999"), "{new}");
        assert_eq!(bak.file_name().unwrap(), "credentials.json.bak");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&path, &bak] {
                assert_eq!(
                    std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{} 应当只有本人可读",
                    p.display()
                );
            }
        }

        // 内容没变就不该再备份一次
        assert!(
            app.write_credentials().expect("写得下去").is_none(),
            "内容一样时不必留备份"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_get_and_set() {
        let mut raw = json_of(r#"{"school":{"host":"h"},"course":{"candidates":[{"id":"a"}]}}"#);
        let p = Path::sec("school", "host");
        assert_eq!(p.get(&raw).unwrap().as_text(), "h");
        p.set(&mut raw, Json::str("h2"));
        assert_eq!(raw.object("school").unwrap().text("host"), "h2");

        let p = Path::ItemKey("course".into(), "candidates".into(), 0, "group".into());
        assert!(p.get(&raw).is_none());
        p.set(&mut raw, Json::str("星期一-3-5"));
        assert_eq!(p.get(&raw).unwrap().as_text(), "星期一-3-5");
        // 只动了这一个键，同一个对象里的别的键原样
        let id = Path::ItemKey("course".into(), "candidates".into(), 0, "id".into());
        assert_eq!(id.get(&raw).unwrap().as_text(), "a");
    }

    #[test]
    fn set_repairs_broken_shapes() {
        // 手写坏了的配置（school 是个字符串、session 不是数组）也要能在界面里改回来：
        // 类型不对的会被换成空容器，然后就能照常添加/编辑了。
        let mut raw = json_of(r#"{"school":"oops","cookies":{"session":"nope"}}"#);
        Path::sec("school", "host").set(&mut raw, Json::str("h"));
        assert_eq!(raw.object("school").unwrap().text("host"), "h");

        let a = raw.ensure_obj("cookies").ensure_arr("session");
        assert!(a.is_empty(), "坏掉的字符串应该被换成空数组");
        a.push(Json::str("JSESSIONID"));
        Path::Item("cookies".into(), "session".into(), 0).set(&mut raw, Json::str("SID2"));
        assert_eq!(
            raw.object("cookies").unwrap().array("session")[0].as_str(),
            Some("SID2")
        );
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let mut raw = json_of(r#"{"_comment":["别提交"],"school":{"host":"h","weird":7}}"#);
        Path::sec("school", "host").set(&mut raw, Json::str("h2"));
        assert_eq!(
            raw.to_compact(),
            r#"{"_comment":["别提交"],"school":{"host":"h2","weird":7}}"#
        );
    }

    #[test]
    fn every_section_builds_usable_rows() {
        let app = example_app();
        for si in 0..SEC_TITLES.len() {
            let rows = app.rows_with_values(si);
            assert!(!rows.is_empty(), "第 {si} 节一行都没有");
            for r in &rows {
                if let Some(f) = &r.field {
                    // 模板里写着的键必须真的取到值（否则显示会全是"未设置"）
                    assert!(
                        f.path.get(app.root(f.target)).is_some()
                            || app.default_at(f.target, &f.path).is_some()
                            || r.dim,
                        "{} 既没值也没默认值",
                        r.label
                    );
                }
            }
        }
        // 学校那一节显示模板里的值（第 0 行是向导动作行，所以按 label 找）
        let school = app.rows_with_values(0);
        let host = school.iter().find(|r| r.label.contains("host")).unwrap();
        assert_eq!(host.value, "course.example.edu.cn");
        // 前两行是向导和粘网址，按 Enter 就能跑
        assert!(matches!(school[0].act, Some(RowAct::WizardUrl)));
        assert!(matches!(school[1].act, Some(RowAct::Wizard)));
        assert!(school.iter().any(|r| matches!(r.act, Some(RowAct::Probe))));
        // 候选教学班那一节：三个候选 × 三个键 = 9 行 + 添加行
        let course = app.rows_with_values(5);
        assert_eq!(
            course.iter().filter(|r| r.field.is_some()).count(),
            5 + 9,
            "{:?}",
            course.iter().map(|r| &r.label).collect::<Vec<_>>()
        );
        // 凭据那一节：密码不回显
        let pwd = app
            .rows_with_values(1)
            .into_iter()
            .find(|r| r.label.contains("密码"))
            .unwrap();
        assert_eq!(pwd.value, "••••••••");
        // 接口路径那一节：十个键都在
        let paths = app.rows_with_values(2);
        for (k, _) in PATH_KEYS {
            assert!(
                paths.iter().any(|r| r.label.starts_with(k)),
                "paths.{k} 没出现在界面上"
            );
        }
    }

    #[test]
    fn defaults_show_up_dimmed_when_key_missing() {
        let defaults = config::engine_defaults();
        assert_eq!(
            display_value(None, false, defaults.get("timezone_offset_hours")),
            ("（默认 8）".to_string(), true)
        );
        assert_eq!(
            display_value(Some(&Json::str("")), false, None),
            ("（未设置）".to_string(), true)
        );
        assert_eq!(
            display_value(Some(&Json::Int(80)), false, None),
            ("80".to_string(), false)
        );
        assert_eq!(
            display_value(Some(&Json::str("p")), true, None),
            ("••••••••".to_string(), false)
        );
    }

    #[test]
    fn input_editing_is_by_char() {
        let mut i = Input::new("课A");
        assert_eq!(i.pos, 2);
        i.backspace();
        assert_eq!(i.text(), "课");
        i.insert("程名");
        assert_eq!(i.text(), "课程名");
        i.left();
        i.insert("X");
        assert_eq!(i.text(), "课程X名");
        i.kill_word();
        assert_eq!(i.text(), "名", "kill_word 只删光标左边的非空白字符");
        // Ctrl-W 的典型场景：删掉最后一个词，前面的原样留着
        let mut i = Input::new("");
        i.insert("abc def");
        i.kill_word();
        assert_eq!(i.text(), "abc ");
        i.kill_word();
        assert_eq!(i.text(), "");
        i.insert("x");
        i.pos = 0;
        i.kill_to_end();
        assert_eq!(i.text(), "");
        // 多行粘贴只留内容，换行/制表符丢掉
        let mut i = Input::new("");
        i.insert("a\nb\tc");
        assert_eq!(i.text(), "abc");
    }

    #[test]
    fn input_window_scrolls_to_cursor() {
        let mut i = Input::new(&"字".repeat(20));
        let (shown, col) = i.window(10);
        assert!(textw::width(&shown) <= 10);
        assert!(col <= 9);
        assert!(shown.ends_with('字'));
        i.pos = 0;
        let (shown, col) = i.window(10);
        assert_eq!(col, 0);
        assert!(shown.starts_with('字'));
        assert_eq!(i.window(0), (String::new(), 0));
    }

    #[test]
    fn list_specs_match_the_config_schema() {
        assert_eq!(
            list_fields("course", "candidates"),
            &["id", "label", "group"]
        );
        assert!(list_fields("cookies", "session").is_empty());
        assert_eq!(list_cap("password", "des_keys"), Some(3));
        assert_eq!(list_cap("cookies", "session"), None);
    }

    #[test]
    fn adding_and_deleting_list_items() {
        let mut app = example_app();
        app.sec = 5; // 目标课程
                     // 多课程：候选挂在 courses[<当前课程>] 下
        app.add_course_cand(0);
        assert_eq!(app.raw.array("courses")[0].array("candidates").len(), 4);
        assert_eq!(
            Path::CourseCand(0, 3, "id")
                .get(&app.raw)
                .map(|v| v.as_text()),
            Some(String::new())
        );

        // 删掉一项：走 'd' 那条路（课程候选的位置由 path 认出来）
        let before = app.raw.array("courses")[0].array("candidates").len();
        let a = app.raw.ensure_arr("courses")[0].ensure_arr("candidates");
        a.remove(0);
        assert_eq!(
            app.raw.array("courses")[0].array("candidates").len(),
            before - 1
        );
    }

    /// `query_content` 和类别对不上要报出来 —— 这是"目录里查不到这门课"的常见原因，
    /// 从任何报错里都看不出来（2026-09-27 实战：线代那门带着 MOOC:2, 却填的是 FAWKC）。
    #[test]
    fn mooc_query_on_a_non_mooc_course_is_flagged() {
        let mut app = example_app();
        assert!(
            !app.problems().iter().any(|(_, s)| s.contains("MOOC: 开关")),
            "模板本身是干净的，不该报"
        );
        app.raw.ensure_arr("courses")[0].set_key("class_type", Json::str("FAWKC"));
        app.raw.ensure_arr("courses")[0].set_key("query_content", Json::str("MOOC:2,{keyword}"));
        let hit = app
            .problems()
            .iter()
            .any(|(_, s)| s.contains("MOOC: 开关") && s.contains("FAWKC"));
        assert!(hit, "该报出来：{:?}", app.problems());

        // 真是慕课的话就不该报
        app.raw.ensure_arr("courses")[0].set_key("class_type", Json::str("MOOC"));
        assert!(
            !app.problems().iter().any(|(_, s)| s.contains("MOOC: 开关")),
            "类别就是 MOOC，带着开关是对的"
        );
    }

    /// 老配置（只有单个 `course`）进界面时会被搬成 `courses` 数组 ——
    /// 否则表单是空的、保存时又把 `course` 写回去，用户以为改好了而程序读的却是 `courses`。
    #[test]
    fn legacy_course_is_migrated_to_courses() {
        let mut raw = json_of(r#"{"course":{"keyword":"线代","candidates":[{"id":"A"}]}}"#);
        config::normalize_courses(&mut raw);
        assert!(raw.get("course").is_none(), "旧键该被清掉");
        assert_eq!(raw.array("courses").len(), 1);
        assert_eq!(raw.array("courses")[0].text("keyword"), "线代");
        assert_eq!(raw.array("courses")[0].array("candidates").len(), 1);
    }

    #[test]
    fn des_keys_are_capped_at_three() {
        let mut app = example_app();
        let rows = app.rows_with_values(4);
        // 模板里已经是 3 组：不该再出现"＋ 添加"
        assert!(
            rows.iter()
                .all(|r| !matches!(r.act, Some(RowAct::Add { .. }))),
            "{:?}",
            rows.len()
        );
        assert!(rows.iter().any(|r| r.value.contains("最多 3 组")));

        // 减到 2 组就会出现添加行
        let a = app.raw.ensure_obj("password").ensure_arr("des_keys");
        a.pop();
        let rows = app.rows_with_values(4);
        assert!(rows
            .iter()
            .any(|r| matches!(r.act, Some(RowAct::Add { .. }))));
    }

    #[test]
    fn validation_flags_the_template_domain() {
        let mut app = app_with(
            json::parse(config::CONFIG_EXAMPLE).unwrap(),
            Json::Obj(Vec::new()),
            false,
        );
        let msgs: Vec<String> = app.problems().into_iter().map(|(_, s)| s).collect();
        assert!(msgs.iter().any(|m| m.contains("示例域名")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("凭据文件")), "{msgs:?}");
        // 模板里的候选教学班是齐的，不该报错
        assert!(msgs.iter().all(|m| !m.contains("候选 [")), "{msgs:?}");

        // 补齐必填项后不该再有 Err
        app.raw = json_of(
            r#"{"school":{"host":"jw.example.edu.cn"},
                "paths":{"volunteer":"/v","capacity":"/c","result":"/r","status":"/s",
                         "sysparam":"/y","program":"/p","student":"/st",
                         "vcode_token":"/vt","vcode_image":"/vi","login":"/l"},
                "password":{"des_keys":["a","b","c"]},
                "course":{"candidates":[{"id":"1","label":"01班","group":"星期一-1-2"}]}}"#,
        );
        app.cred = json_of(r#"{"student_id":"2026000001","password":"p"}"#);
        app.cred_exists = true;
        let got = app.problems();
        assert!(got.iter().all(|(l, _)| *l != Level::Err), "{got:?}");
    }

    #[test]
    fn validation_catches_what_the_ui_checks_do_not() {
        // 界面没单独检查的坑（4 组密钥）也要被最后那道程序自检兜住
        let raw = json_of(
            r#"{"school":{"host":"h"},
                "paths":{"volunteer":"/v","capacity":"/c","result":"/r","status":"/s",
                         "sysparam":"/y","program":"/p","student":"/st",
                         "vcode_token":"/vt","vcode_image":"/vi","login":"/l"},
                "password":{"des_keys":["a","b","c","d"]},
                "course":{"candidates":[{"id":"1","label":"x","group":"g"}]}}"#,
        );
        let app = app_with(
            raw,
            json_of(r#"{"student_id":"2026000001","password":"p"}"#),
            true,
        );
        let got = app.problems();
        assert!(
            got.iter()
                .any(|(l, m)| *l == Level::Err && m.contains("最多 3 组")),
            "{got:?}"
        );
    }

    #[test]
    fn missing_host_is_an_error_not_a_default() {
        let app = app_with(json_of(r#"{}"#), json_of(r#"{}"#), false);
        let got = app.problems();
        assert!(
            got.iter()
                .any(|(l, m)| *l == Level::Err && m.contains("school.host")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(l, m)| *l == Level::Err && m.contains("paths")),
            "{got:?}"
        );
    }

    #[test]
    fn pretty_print_matches_the_example_style() {
        let v = json_of(r#"{"a":{"b":[1,2],"c":{}},"d":[]}"#);
        assert_eq!(
            v.to_pretty(),
            "{\n  \"a\": {\n    \"b\": [\n      1,\n      2\n    ],\n    \"c\": {}\n  },\n  \"d\": []\n}"
        );
    }

    #[test]
    fn line_clips_to_width() {
        let mut l = Line::new(6, false);
        l.seg(false, "", "目标课程名");
        l.seg(false, "", "后面还有内容");
        let got = l.finish();
        assert!(textw::width(&got) <= 6, "{got:?}");
        // 第一段截到 5 列，剩下的 1 列放不下东西 —— 不能再补一个省略号
        assert_eq!(got, "目标…");
    }

    #[test]
    fn line_keeps_colors_out_of_width_math() {
        let mut l = Line::new(10, true);
        l.seg(true, S_SEL, "abc");
        l.seg(true, S_DIM, "de");
        let got = l.finish();
        assert!(got.contains("\x1b[7m"), "{got:?}");
        // 去掉 SGR 之后才是可见宽度
        let plain: String = {
            let mut out = String::new();
            let mut it = got.split('\x1b');
            out.push_str(it.next().unwrap());
            for part in it {
                if let Some(i) = part.find('m') {
                    out.push_str(&part[i + 1..]);
                }
            }
            out
        };
        assert_eq!(plain, "abcde");
    }
}
