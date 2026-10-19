//! 键编解码。**这是整个代码库里唯一允许拼 RocksDB key 的地方。**
//!
//! 键空间散落是这类项目后期最难改的债：一旦前缀字面量出现在三四个模块里，
//! 想调整键格式就得全库搜索并祈祷没漏。所以这里只对外暴露函数，
//! 前缀常量一律私有。
//!
//! 键格式见 `notes/架构设计-存储与KV键空间.md` 第 5 节。
//!
//! # 两类组件与各自的解码方式
//!
//! 分隔符是 `0x00`，但**不能无脑按 `0x00` 切分**——有些组件本身就含 `0x00`。
//! 所以组件分两类，解码方式不同：
//!
//! | 类别 | 举例 | 是否含 `0x00` | 解码方式 |
//! |------|------|---------------|----------|
//! | 文本组件 | username、path、ULID、slug | 否（编码时校验并拒绝） | 可按分隔符切分 |
//! | 定长二进制 | 时间戳 u64 BE、内容哈希 32 字节、分片号 u32 BE | **是**（前导零就是 `0x00`） | 只能按偏移定位 |
//!
//! 含定长二进制组件的键（`blobmeta` / `chunkmeta` / `gc` / `stage`）一律按偏移解码。
//! 时间戳用大端是为了让字典序等于时间序，GC 队列才能顺序扫到期项。

use std::fmt;

use anyhow::{Result, bail};

/// 键分隔符。
const SEP: u8 = 0x00;

/// ULID 的文本长度（Crockford base32）。字典序 = 创建时间序。
pub const ID_LEN: usize = 26;

/// 画布 slug 的长度：12 字符 base62，约 71 bit 熵。
pub const SLUG_LEN: usize = 12;

/// BLAKE3 输出长度。
pub const HASH_LEN: usize = 32;

/// path 长度上限（字节）。
pub const MAX_PATH_LEN: usize = 512;

/// path 段数上限。
pub const MAX_PATH_SEGMENTS: usize = 16;

/// 画布内目录路径的段数上限。
///
/// 比文件 path 少一段：留那一段给文件名。否则能建出一个永远放不进文件的目录。
pub const MAX_DIR_SEGMENTS: usize = MAX_PATH_SEGMENTS - 1;

/// 用户名长度上限（字节）。
pub const MAX_USERNAME_LEN: usize = 64;

// ---------------------------------------------------------------------------
// id 类型
// ---------------------------------------------------------------------------

/// 定义一个 ULID 新类型。
///
/// 为什么不用统一的 `Id` 类型：`canvas_file_key(canvas_id, path)` 与
/// `folder_key(uid, folder_id)` 的参数如果都是同一个类型，把 uid 和 canvas_id
/// 传反了编译器不会报错，而这种 bug 会写出一条永远查不到的 key。
/// 分开定型是拿一点样板代码换编译期检查。
macro_rules! define_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(ulid::Ulid);

        impl $name {
            /// 按当前时间生成一个新 id。
            pub fn new() -> Self {
                Self(ulid::Ulid::generate())
            }

            /// 解析 26 字符 ULID 文本。
            pub fn parse(s: &str) -> Result<Self> {
                if s.len() != ID_LEN {
                    bail!("id 长度应为 {ID_LEN}，实际 {}: {s}", s.len());
                }
                let ulid = ulid::Ulid::from_string(s)
                    .map_err(|e| anyhow::anyhow!("id 不是合法 ULID: {s} ({e})"))?;
                Ok(Self(ulid))
            }

            /// 26 字符文本形式。
            pub fn to_text(self) -> String {
                self.0.to_string()
            }

            /// 写进 key 的字节（ASCII，不含 `0x00`）。
            fn as_key_bytes(self) -> [u8; ID_LEN] {
                let mut buf = [0u8; ID_LEN];
                // ulid 的 array_to_str 保证正好写满 26 个 ASCII 字符。
                self.0.array_to_str(&mut buf);
                buf
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0.to_string())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(Uid, "用户 id。");
define_id!(FolderId, "文件夹 id。");
define_id!(CanvasId, "画布 id。");
define_id!(ProjectId, "项目 id。");
define_id!(ShareId, "一轮画廊共享 id。");
define_id!(UploadId, "分块上传会话 id。");
define_id!(TaskId, "公共队列任务 id。");

define_id!(GenerationId, "一次生成尝试 id。");

pub fn gallery_preference_key(uid: Uid) -> Vec<u8> {
    KeyBuf::new(b"ugpref", ID_LEN)
        .then(&uid.as_key_bytes())
        .done()
}
pub fn gallery_control_key(pid: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"gpctrl", ID_LEN)
        .then(&pid.as_key_bytes())
        .done()
}
pub fn gallery_ready_key(pid: ProjectId, cid: CanvasId) -> Vec<u8> {
    KeyBuf::new(b"gready", ID_LEN * 2 + 1)
        .then(&pid.as_key_bytes())
        .then(&cid.as_key_bytes())
        .done()
}
pub fn gallery_ready_prefix(pid: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"gready", ID_LEN + 1)
        .then(&pid.as_key_bytes())
        .then(b"")
        .done()
}
pub fn gallery_done_key(pid: ProjectId, gid: GenerationId) -> Vec<u8> {
    KeyBuf::new(b"gdone", ID_LEN * 2 + 1)
        .then(&pid.as_key_bytes())
        .then(&gid.as_key_bytes())
        .done()
}
pub fn gallery_done_prefix_all() -> Vec<u8> {
    b"gdone\0".to_vec()
}

pub fn gallery_done_prefix(pid: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"gdone", ID_LEN + 1)
        .then(&pid.as_key_bytes())
        .then(b"")
        .done()
}

pub fn project_share_key(id: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"gshare", ID_LEN)
        .then(&id.as_key_bytes())
        .done()
}

pub fn gallery_feed_key(id: ShareId) -> Vec<u8> {
    KeyBuf::new(b"gfeed", ID_LEN)
        .then(&id.as_key_bytes())
        .done()
}

pub fn gallery_feed_prefix() -> Vec<u8> {
    b"gfeed\0".to_vec()
}

pub fn parse_gallery_feed_key(key: &[u8]) -> Result<ShareId> {
    anyhow::ensure!(
        key.len() == 6 + ID_LEN && key.starts_with(b"gfeed\0"),
        "画廊索引键非法"
    );
    let raw = std::str::from_utf8(&key[6..])?;
    let id = ShareId::parse(raw)?;
    anyhow::ensure!(id.to_text() == raw, "画廊共享 ID 非规范编码");
    Ok(id)
}

/// task_queue 独立列族的键。按 ULID 排序，同毫秒的任务不承诺严格先后。
pub fn task_queue_key(id: TaskId) -> Vec<u8> {
    id.as_key_bytes().to_vec()
}

/// 树节点的父节点。根层没有父文件夹，所以要能表达「无」。
///
/// 编码成 26 个 `0` 字符：ULID 的 Crockford base32 字母表里 `0` 是最小字符，
/// 所以根层的子节点索引排在所有具名父节点之前，且长度与真实 id 一致，
/// 按偏移解码时不用分情况。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentId {
    /// 根层。
    Root,
    /// 某个文件夹。
    Folder(FolderId),
}

impl ParentId {
    fn as_key_bytes(self) -> [u8; ID_LEN] {
        match self {
            Self::Root => [b'0'; ID_LEN],
            Self::Folder(id) => id.as_key_bytes(),
        }
    }
}

impl From<Option<FolderId>> for ParentId {
    fn from(v: Option<FolderId>) -> Self {
        v.map_or(Self::Root, Self::Folder)
    }
}

// ---------------------------------------------------------------------------
// 内容哈希
// ---------------------------------------------------------------------------

/// BLAKE3 内容哈希。blob 与 chunk 都用它做内容寻址。
///
/// key 里存原始 32 字节而不是 64 字符 hex：千万级条目下每条省 32 字节，
/// 累计几百 MB。代价是哈希**只能作为键的末尾或定长位置**出现，
/// 因为它可能含 `0x00`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; HASH_LEN]);

impl ContentHash {
    pub fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    /// 完整 64 字符小写 hex。用于 ETag 与接口返回。
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(HASH_LEN * 2);
        for b in self.0 {
            use fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// hex 前 8 字符，用于静态 URL 的 `?v=` 缓存版本参数。
    pub fn to_hex8(self) -> String {
        let mut s = String::with_capacity(8);
        for b in &self.0[..4] {
            use fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// 解析 64 字符 hex。
    pub fn parse_hex(s: &str) -> Result<Self> {
        if s.len() != HASH_LEN * 2 {
            bail!("哈希 hex 长度应为 {}，实际 {}", HASH_LEN * 2, s.len());
        }
        let mut out = [0u8; HASH_LEN];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_val(chunk[0])?;
            let lo = hex_val(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => bail!("非法 hex 字符: {}", c as char),
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 日志里打全长哈希没意义，前 8 位足够区分。
        write!(f, "ContentHash({}…)", self.to_hex8())
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl serde::Serialize for ContentHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // bincode 下序列化成 32 字节定长数组，比 hex 省一半。
        self.0.serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for ContentHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        <[u8; HASH_LEN]>::deserialize(d).map(Self)
    }
}

// ---------------------------------------------------------------------------
// 文本组件校验
// ---------------------------------------------------------------------------

/// 校验用户名可以进 key。
///
/// 含 `0x00` 一律**拒绝而不转义**：转义会让「key 的字节长度」与「名字的字符数」
///脱钩，前缀扫描的边界判断随之变得难以推理。用户名本来就该是受限字符集。
pub fn validate_username(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("用户名不能为空");
    }
    if name.len() > MAX_USERNAME_LEN {
        bail!("用户名超长: {} > {MAX_USERNAME_LEN} 字节", name.len());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        bail!("用户名只允许字母、数字、下划线、短横线与点: {name}");
    }
    Ok(())
}

/// 校验画布内相对路径可以进 key。
///
/// 规则来自接口文档第 5 节。规范化在 HTTP 层做完，**进到这里的 path 只做最后一道
/// 拦截**——这道拦截存在的意义是：万一哪天 HTTP 层漏了一条规则，
/// 脏 path 也不会污染键空间。
pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("路径不能为空");
    }
    if path.len() > MAX_PATH_LEN {
        bail!("路径超长: {} > {MAX_PATH_LEN} 字节", path.len());
    }
    if path.starts_with('/') {
        bail!("路径不能是绝对路径: {path}");
    }
    if path.ends_with('/') {
        bail!("路径不能以斜杠结尾: {path}");
    }
    // 控制字符包含 0x00，所以这一条同时挡掉了分隔符注入。
    if let Some(c) = path.chars().find(|c| c.is_control()) {
        bail!("路径含控制字符 U+{:04X}: {path}", c as u32);
    }
    if path.contains('\\') {
        bail!("路径不能含反斜杠: {path}");
    }

    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > MAX_PATH_SEGMENTS {
        bail!("路径段数超限: {} > {MAX_PATH_SEGMENTS}", segments.len());
    }
    for seg in &segments {
        if seg.is_empty() {
            bail!("路径含空段: {path}");
        }
        if *seg == "." || *seg == ".." {
            bail!("路径含相对段: {path}");
        }
    }
    Ok(())
}

/// 校验画布内目录路径可以进 key。
///
/// 规则完全沿用 [`validate_path`]（**不带前后斜杠**，所以 `scene01` 合法、
/// `scene01/` 不合法），只把段数上限收紧到 [`MAX_DIR_SEGMENTS`]。
///
/// 共用一套校验函数是有意的：目录路径与文件 path 落在同一个字符串空间里，
/// 两套规则会让「文件 a/b 与目录 a/b 冲突」这类判断无从下手。
pub fn validate_dir_path(path: &str) -> Result<()> {
    validate_path(path)?;
    let segments = path.split('/').count();
    if segments > MAX_DIR_SEGMENTS {
        bail!("目录路径段数超限: {segments} > {MAX_DIR_SEGMENTS}（要留一段给文件名）");
    }
    Ok(())
}

/// 校验文件夹或画布的显示名。
///
/// 名字不进 key（只存在 value 里），所以规则比用户名宽松得多，
/// 但仍然拒绝控制字符——它们在日志与 JSON 里都是麻烦来源。
pub fn validate_display_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("名称不能为空或只有空白");
    }
    if name.len() > 256 {
        bail!("名称超长: {} > 256 字节", name.len());
    }
    if let Some(c) = name.chars().find(|c| c.is_control()) {
        bail!("名称含控制字符 U+{:04X}", c as u32);
    }
    Ok(())
}

/// 校验画布 slug。
pub fn validate_slug(slug: &str) -> Result<()> {
    if slug.len() != SLUG_LEN {
        bail!("slug 长度应为 {SLUG_LEN}，实际 {}", slug.len());
    }
    if !slug.bytes().all(|b| b.is_ascii_alphanumeric()) {
        bail!("slug 只允许 base62 字符: {slug}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 键构造：meta 列族
// ---------------------------------------------------------------------------

/// 拼键的小工具，避免每个函数都手写 `Vec::with_capacity` + `extend`。
struct KeyBuf(Vec<u8>);

impl KeyBuf {
    fn new(tag: &[u8], extra: usize) -> Self {
        let mut v = Vec::with_capacity(tag.len() + 1 + extra);
        v.extend_from_slice(tag);
        v.push(SEP);
        Self(v)
    }

    /// 追加一个组件，前面补分隔符。
    fn sep_then(mut self, bytes: &[u8]) -> Self {
        self.0.push(SEP);
        self.0.extend_from_slice(bytes);
        self
    }

    /// 追加一个组件，不补分隔符（用于紧跟 tag 的第一个组件）。
    fn then(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    fn done(self) -> Vec<u8> {
        self.0
    }
}

/// `u|{uid}` → User
pub fn user_key(uid: Uid) -> Vec<u8> {
    KeyBuf::new(b"u", ID_LEN).then(&uid.as_key_bytes()).done()
}

/// 用户套餐与任务次数，独立于旧版 bincode 用户记录。
pub fn user_plan_key(uid: Uid) -> Vec<u8> {
    KeyBuf::new(b"up", ID_LEN).then(&uid.as_key_bytes()).done()
}

/// 北京时间日期对应的全站注册成功数。
pub fn registration_day_key(date: &str) -> Vec<u8> {
    KeyBuf::new(b"rd", date.len()).then(date.as_bytes()).done()
}

/// `d|{uid}|{folder_id}` → Folder
pub fn folder_key(uid: Uid, folder_id: FolderId) -> Vec<u8> {
    KeyBuf::new(b"d", ID_LEN * 2 + 1)
        .then(&uid.as_key_bytes())
        .sep_then(&folder_id.as_key_bytes())
        .done()
}

pub fn project_key(uid: Uid, project_id: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"p", ID_LEN * 2 + 1)
        .then(&uid.as_key_bytes())
        .sep_then(&project_id.as_key_bytes())
        .done()
}
pub fn project_request_key(uid: Uid, request: &str) -> Vec<u8> {
    KeyBuf::new(b"pir", ID_LEN + 1 + request.len())
        .then(&uid.as_key_bytes())
        .sep_then(request.as_bytes())
        .done()
}
pub fn project_request_prefix(uid: Uid) -> Vec<u8> {
    KeyBuf::new(b"pir", ID_LEN + 1)
        .then(&uid.as_key_bytes())
        .sep_then(b"")
        .done()
}
pub fn canvas_request_key(project_id: ProjectId, request: &str) -> Vec<u8> {
    KeyBuf::new(b"cir", ID_LEN + 1 + request.len())
        .then(&project_id.as_key_bytes())
        .sep_then(request.as_bytes())
        .done()
}
pub fn canvas_request_prefix(project_id: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"cir", ID_LEN + 1)
        .then(&project_id.as_key_bytes())
        .sep_then(b"")
        .done()
}

pub fn project_member_key(project_id: ProjectId, canvas_id: CanvasId) -> Vec<u8> {
    KeyBuf::new(b"pc", ID_LEN * 2 + 1)
        .then(&project_id.as_key_bytes())
        .sep_then(&canvas_id.as_key_bytes())
        .done()
}

pub fn project_member_prefix(project_id: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"pc", ID_LEN + 1)
        .then(&project_id.as_key_bytes())
        .sep_then(b"")
        .done()
}

pub fn parse_project_member_key(key: &[u8]) -> Result<CanvasId> {
    let prefix_len = 3 + ID_LEN + 1;
    if key.len() != prefix_len + ID_LEN || !key.starts_with(b"pc\0") || key[3 + ID_LEN] != SEP {
        bail!("不是合法的项目画布索引");
    }
    CanvasId::parse(std::str::from_utf8(&key[prefix_len..])?)
}

pub fn project_prefix() -> Vec<u8> {
    b"p\0".to_vec()
}

pub fn user_project_prefix(uid: Uid) -> Vec<u8> {
    KeyBuf::new(b"p", ID_LEN + 1)
        .then(&uid.as_key_bytes())
        .sep_then(b"")
        .done()
}

pub fn parse_project_key(key: &[u8]) -> Result<(Uid, ProjectId)> {
    if key.len() != 2 + ID_LEN + 1 + ID_LEN || !key.starts_with(b"p\0") || key[2 + ID_LEN] != SEP {
        bail!("不是合法的项目键");
    }
    Ok((
        Uid::parse(std::str::from_utf8(&key[2..2 + ID_LEN])?)?,
        ProjectId::parse(std::str::from_utf8(&key[3 + ID_LEN..])?)?,
    ))
}

/// `c|{uid}|{canvas_id}` → Canvas
pub fn canvas_key(uid: Uid, canvas_id: CanvasId) -> Vec<u8> {
    KeyBuf::new(b"c", ID_LEN * 2 + 1)
        .then(&uid.as_key_bytes())
        .sep_then(&canvas_id.as_key_bytes())
        .done()
}

/// 某个用户全部画布的扫描前缀。
pub fn user_canvas_prefix(uid: Uid) -> Vec<u8> {
    KeyBuf::new(b"c", ID_LEN + 1)
        .then(&uid.as_key_bytes())
        .sep_then(b"")
        .done()
}

/// 全部用户记录的扫描前缀（admin 的用户列表用）。
pub fn user_prefix() -> Vec<u8> {
    vec![b'u', SEP]
}

/// 全部画布记录的扫描前缀（统计与「按 canvas_id 找所有者」用）。
pub fn canvas_prefix() -> Vec<u8> {
    vec![b'c', SEP]
}

/// 全部 agent token 主记录的扫描前缀。
pub fn agent_token_prefix() -> Vec<u8> {
    vec![b'a', b't', SEP]
}

/// 全部 agent token 画布索引的扫描前缀。
pub fn agent_token_index_prefix() -> Vec<u8> {
    vec![b'a', b't', b'p', SEP]
}

/// 全部上传会话的扫描前缀。
pub fn stage_info_prefix() -> Vec<u8> {
    vec![b's', b'i', SEP]
}

/// 从 `chunk` / `blob_txt` / `blob_bin` 列族的裸键里取回哈希。
///
/// 这三个列族的键就是 32 字节哈希本身，没有前缀（它们只存内容，
/// 键空间不与别的东西共享）。对账扫描要用它。
pub fn parse_content_key(key: &[u8]) -> Result<ContentHash> {
    let bytes: [u8; HASH_LEN] = key
        .try_into()
        .map_err(|_| anyhow::anyhow!("内容键长度应为 {HASH_LEN}，实际 {}", key.len()))?;
    Ok(ContentHash(bytes))
}

/// 从 `u|{uid}` 里取回 uid。
pub fn parse_user_key(key: &[u8]) -> Result<Uid> {
    if key.len() != 2 + ID_LEN || key[0] != b'u' || key[1] != SEP {
        bail!("不是合法的 user key");
    }
    let text = std::str::from_utf8(&key[2..]).map_err(|_| anyhow::anyhow!("uid 不是 UTF-8"))?;
    Uid::parse(text)
}

/// 从 `c|{uid}|{canvas_id}` 里取回两个 id。
pub fn parse_canvas_key(key: &[u8]) -> Result<(Uid, CanvasId)> {
    let mid = 2 + ID_LEN;
    if key.len() != mid + 1 + ID_LEN || key[0] != b'c' || key[1] != SEP || key[mid] != SEP {
        bail!("不是合法的 canvas key");
    }
    let uid_text =
        std::str::from_utf8(&key[2..mid]).map_err(|_| anyhow::anyhow!("uid 不是 UTF-8"))?;
    let cid_text = std::str::from_utf8(&key[mid + 1..])
        .map_err(|_| anyhow::anyhow!("canvas_id 不是 UTF-8"))?;
    Ok((Uid::parse(uid_text)?, CanvasId::parse(cid_text)?))
}

/// 从 `si|{upload_id}` 里取回会话 id。
pub fn parse_stage_info_key(key: &[u8]) -> Result<UploadId> {
    if key.len() != 3 + ID_LEN || &key[..2] != b"si" || key[2] != SEP {
        bail!("不是合法的 stage info key");
    }
    let text =
        std::str::from_utf8(&key[3..]).map_err(|_| anyhow::anyhow!("upload_id 不是 UTF-8"))?;
    UploadId::parse(text)
}

/// 从 `at|{hash}` 里取回 token 哈希。
///
/// 单独一个函数是因为 [`parse_hash_key`] 只认单字符前缀（`b|` / `r|` / `k|`），
/// 两字符前缀会被它拒掉——这个区别曾经让清理任务整个报错。
pub fn parse_agent_token_key(key: &[u8]) -> Result<ContentHash> {
    let head = 3;
    if key.len() != head + HASH_LEN || &key[..2] != b"at" || key[2] != SEP {
        bail!("不是合法的 agent token key");
    }
    let mut out = [0u8; HASH_LEN];
    out.copy_from_slice(&key[head..]);
    Ok(ContentHash(out))
}

/// 从 `atp|{project_id}|{hash}` 里取回 token 哈希。
pub fn parse_agent_token_index_key(key: &[u8]) -> Result<ContentHash> {
    let head = 3 + 1 + ID_LEN + 1;
    if key.len() != head + HASH_LEN || &key[..3] != b"atp" {
        bail!("不是合法的 agent token 索引 key");
    }
    let mut out = [0u8; HASH_LEN];
    out.copy_from_slice(&key[head..]);
    Ok(ContentHash(out))
}

/// `f|{canvas_id}|{path}` → FileEntry
///
/// path 是变长文本且在末尾，所以前缀扫描能拿到一个画布的全部文件，
/// 且按路径字典序返回——正好是 manifest 需要的顺序。
pub fn file_key(canvas_id: CanvasId, path: &str) -> Result<Vec<u8>> {
    validate_path(path)?;
    Ok(KeyBuf::new(b"f", ID_LEN + 1 + path.len())
        .then(&canvas_id.as_key_bytes())
        .sep_then(path.as_bytes())
        .done())
}

/// 某个画布全部文件条目的扫描前缀。
pub fn file_prefix(canvas_id: CanvasId) -> Vec<u8> {
    // 结尾必须带分隔符：否则 canvas_id 是另一个 id 前缀时会扫到隔壁。
    // ULID 定长 26 所以实际不会撞，但前缀里带上分隔符是零成本的保险。
    KeyBuf::new(b"f", ID_LEN + 1)
        .then(&canvas_id.as_key_bytes())
        .sep_then(b"")
        .done()
}

/// 某个画布内**某个目录子树**全部文件条目的扫描前缀：`f|{canvas_id}|{dir_path}/`。
///
/// 末尾的斜杠是关键：不带它，`scene01` 会连 `scene010/x.jpg` 一起扫到。
pub fn file_prefix_under(canvas_id: CanvasId, dir_path: &str) -> Result<Vec<u8>> {
    validate_dir_path(dir_path)?;
    Ok(KeyBuf::new(b"f", ID_LEN + 1 + dir_path.len() + 1)
        .then(&canvas_id.as_key_bytes())
        .sep_then(dir_path.as_bytes())
        .then(b"/")
        .done())
}

/// 从 `f|{canvas_id}|{path}` 里取回 path。
pub fn parse_file_key(key: &[u8]) -> Result<(CanvasId, String)> {
    // 按偏移解析：`f` `0x00` {26} `0x00` {path...}
    let head = 1 + 1 + ID_LEN + 1;
    if key.len() <= head || key[0] != b'f' || key[1] != SEP || key[2 + ID_LEN] != SEP {
        bail!("不是合法的 file key");
    }
    let id_text = std::str::from_utf8(&key[2..2 + ID_LEN])
        .map_err(|_| anyhow::anyhow!("file key 里的 canvas_id 不是 UTF-8"))?;
    let canvas_id = CanvasId::parse(id_text)?;
    let path = std::str::from_utf8(&key[head..])
        .map_err(|_| anyhow::anyhow!("file key 里的 path 不是 UTF-8"))?;
    Ok((canvas_id, path.to_owned()))
}

/// `fd|{canvas_id}|{dir_path}` → CanvasDir
///
/// 画布**内**的目录记录。注意与画布**外**的文件夹（`d|{uid}|{folder_id}`）
/// 是两个不同的东西：文件夹的身份是 ULID，目录的身份就是这条 key 里的路径字符串。
///
/// # 为什么 `fd|` 与 `f|` 不冲突
///
/// 文件键是 `'f' 0x00 …`，目录键是 `'f' 'd' 0x00 …`。**第二个字节已经把两者分开**
/// （`0x00` vs `'d'`），所以 [`file_prefix`] 扫不到目录记录，[`canvas_dir_prefix`]
/// 也扫不到文件记录。两者都以 canvas_id 作为第二段，所以「按画布前缀扫」这件事
/// 在两边都成立。
///
/// `dir_path` **不带前后斜杠**（`scene01`、`scene01/raw`），这样不会出现
/// `scene01` 与 `scene01/` 两条指向同一个目录的记录。
pub fn canvas_dir_key(canvas_id: CanvasId, dir_path: &str) -> Result<Vec<u8>> {
    validate_dir_path(dir_path)?;
    Ok(KeyBuf::new(b"fd", ID_LEN + 1 + dir_path.len())
        .then(&canvas_id.as_key_bytes())
        .sep_then(dir_path.as_bytes())
        .done())
}

/// 某个画布全部目录记录的扫描前缀。
pub fn canvas_dir_prefix(canvas_id: CanvasId) -> Vec<u8> {
    KeyBuf::new(b"fd", ID_LEN + 1)
        .then(&canvas_id.as_key_bytes())
        .sep_then(b"")
        .done()
}

/// 某个目录**子树下**全部目录记录的扫描前缀：`fd|{canvas_id}|{dir_path}/`。
///
/// 不含 `dir_path` 自己那条记录（它的 key 末尾没有斜杠），所以要判断
/// 「这个目录有没有显式记录」得单独 get 一次 [`canvas_dir_key`]。
pub fn canvas_dir_prefix_under(canvas_id: CanvasId, dir_path: &str) -> Result<Vec<u8>> {
    validate_dir_path(dir_path)?;
    Ok(KeyBuf::new(b"fd", ID_LEN + 1 + dir_path.len() + 1)
        .then(&canvas_id.as_key_bytes())
        .sep_then(dir_path.as_bytes())
        .then(b"/")
        .done())
}

/// 全部画布目录记录的扫描前缀（统计用）。
pub fn canvas_dir_prefix_all() -> Vec<u8> {
    vec![b'f', b'd', SEP]
}

/// 从 `fd|{canvas_id}|{dir_path}` 里取回 canvas_id 与目录路径。
pub fn parse_canvas_dir_key(key: &[u8]) -> Result<(CanvasId, String)> {
    // 按偏移解析：`f` `d` `0x00` {26} `0x00` {dir_path...}
    let head = 2 + 1 + ID_LEN + 1;
    if key.len() <= head || &key[..2] != b"fd" || key[2] != SEP || key[3 + ID_LEN] != SEP {
        bail!("不是合法的 canvas dir key");
    }
    let id_text = std::str::from_utf8(&key[3..3 + ID_LEN])
        .map_err(|_| anyhow::anyhow!("dir key 里的 canvas_id 不是 UTF-8"))?;
    let canvas_id = CanvasId::parse(id_text)?;
    let dir_path = std::str::from_utf8(&key[head..])
        .map_err(|_| anyhow::anyhow!("dir key 里的 dir_path 不是 UTF-8"))?;
    Ok((canvas_id, dir_path.to_owned()))
}

// ---------------------------------------------------------------------------
// 键构造：idx 列族
// ---------------------------------------------------------------------------

/// `un|{username}` → uid，登录时按用户名查用户。
pub fn username_key(username: &str) -> Result<Vec<u8>> {
    validate_username(username)?;
    Ok(KeyBuf::new(b"un", username.len())
        .then(username.as_bytes())
        .done())
}

/// 子节点类型标记。文件夹排在画布前面（`c` < `d` 反了，所以显式用 `c`/`d` 之外的顺序说明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    Folder,
    Project,
}

impl ChildKind {
    fn tag(self) -> u8 {
        match self {
            // 与 meta 列族的 `d|` / `c|` 保持一致，便于对照。
            Self::Folder => b'd',
            Self::Project => b'p',
        }
    }

    fn from_tag(b: u8) -> Result<Self> {
        match b {
            b'd' => Ok(Self::Folder),
            b'p' => Ok(Self::Project),
            other => bail!("未知的子节点类型标记: {}", other as char),
        }
    }
}

/// `ch|{uid}|{parent_id}|{kind}|{child_id}` → 空
///
/// 子节点索引。值为空，信息全在 key 里——列目录只需一次前缀扫描，
/// 不用回查 meta。
pub fn child_key(uid: Uid, parent: ParentId, kind: ChildKind, child_id: &str) -> Result<Vec<u8>> {
    if child_id.len() != ID_LEN {
        bail!("子节点 id 长度应为 {ID_LEN}，实际 {}", child_id.len());
    }
    Ok(KeyBuf::new(b"ch", ID_LEN * 3 + 4)
        .then(&uid.as_key_bytes())
        .sep_then(&parent.as_key_bytes())
        .sep_then(&[kind.tag()])
        .sep_then(child_id.as_bytes())
        .done())
}

/// 某个父节点下全部子节点的扫描前缀（文件夹与画布混在一起，按类型标记区分）。
pub fn child_prefix(uid: Uid, parent: ParentId) -> Vec<u8> {
    KeyBuf::new(b"ch", ID_LEN * 2 + 2)
        .then(&uid.as_key_bytes())
        .sep_then(&parent.as_key_bytes())
        .sep_then(b"")
        .done()
}

/// 从子节点索引 key 里取回类型与 id。
pub fn parse_child_key(key: &[u8]) -> Result<(ChildKind, String)> {
    // `ch` `0x00` {26} `0x00` {26} `0x00` {1} `0x00` {26}
    let expect = 2 + 1 + ID_LEN + 1 + ID_LEN + 1 + 1 + 1 + ID_LEN;
    if key.len() != expect || !key.starts_with(b"ch\0") {
        bail!("不是合法的 child key（长度 {} 应为 {expect}）", key.len());
    }
    let kind_at = 2 + 1 + ID_LEN + 1 + ID_LEN + 1;
    let kind = ChildKind::from_tag(key[kind_at])?;
    let id_at = kind_at + 2;
    let id = std::str::from_utf8(&key[id_at..])
        .map_err(|_| anyhow::anyhow!("child key 里的 id 不是 UTF-8"))?;
    Ok((kind, id.to_owned()))
}

/// `cs|{canvas_slug}` → SlugTarget{uid, canvas_id}
///
/// 静态 URL 里按 slug 定位画布。**slug 全局唯一**，键里不含所有者信息——
/// 这样 `/f/{slug}/{path}` 就不必在 URL 里暴露用户名或 uid。
///
/// 代价是 value 要同时存 uid 与 canvas_id：画布记录的键是
/// `c|{uid}|{canvas_id}`，少了 uid 就读不到画布。
pub fn canvas_slug_key(slug: &str) -> Result<Vec<u8>> {
    validate_slug(slug)?;
    Ok(KeyBuf::new(b"cs", SLUG_LEN).then(slug.as_bytes()).done())
}

// ---------------------------------------------------------------------------
// 键构造：内容相关列族
// ---------------------------------------------------------------------------

/// `blob_txt` / `blob_bin` / `chunk` 列族的键就是裸哈希，没有前缀。
///
/// 这三个列族只存内容，键空间不与任何别的东西共享，所以不需要前缀区分。
pub fn content_key(hash: ContentHash) -> Vec<u8> {
    hash.0.to_vec()
}

/// `b|{hash}` → BlobMeta（**不可变**，写一次不再更新）
pub fn blob_meta_key(hash: ContentHash) -> Vec<u8> {
    KeyBuf::new(b"b", HASH_LEN).then(&hash.0).done()
}

/// `r|{hash}` → blob 引用计数
///
/// # 为什么 refcount 不放在 BlobMeta 里面
///
/// 设计文档第 5 节把 refcount 写在 `BlobMeta{size, mime_class, layout, refcount}`
/// 结构体里，这里把它拆成了独立的计数键，理由三条：
///
/// 1. **BlobMeta 的其余字段由内容唯一决定**（同一个哈希的 size 与 layout 永远相同），
///    所以它天然不可变，应该写一次就再也不碰——留在结构体里会让每次引用变化都
///    重写一遍元数据。
/// 2. **merge operator 只能对单一值类型做原子加减。** 计数独立成键之后，
///    blob 与 chunk 的计数用同一个加法 operator，不需要为结构体设计
///    「整值 / 增量」双形态操作数。
/// 3. 避免文档第 7 节自己警告的读-改-写竞争：结构体里的计数没法 merge，
///    只能读出来改再写回，并发下会丢更新。
pub fn blob_ref_key(hash: ContentHash) -> Vec<u8> {
    KeyBuf::new(b"r", HASH_LEN).then(&hash.0).done()
}

/// `k|{chunk_hash}` → chunk 引用计数
pub fn chunk_ref_key(hash: ContentHash) -> Vec<u8> {
    KeyBuf::new(b"k", HASH_LEN).then(&hash.0).done()
}

/// blobmeta 列族里全部 BlobMeta 的扫描前缀（对账用）。
pub fn blob_meta_prefix() -> Vec<u8> {
    vec![b'b', SEP]
}

/// chunkmeta 列族里全部计数的扫描前缀（孤儿对账用）。
pub fn chunk_ref_prefix() -> Vec<u8> {
    vec![b'k', SEP]
}

/// 从 `b|` / `r|` / `k|` 开头的键里取回哈希。
pub fn parse_hash_key(key: &[u8]) -> Result<ContentHash> {
    let head = 2;
    if key.len() != head + HASH_LEN || key[1] != SEP || !matches!(key[0], b'b' | b'r' | b'k') {
        bail!("不是合法的内容元数据 key");
    }
    let mut out = [0u8; HASH_LEN];
    out.copy_from_slice(&key[head..]);
    Ok(ContentHash(out))
}

// ---------------------------------------------------------------------------
// 键构造：stage 与 gc 列族
// ---------------------------------------------------------------------------

/// `s|{upload_id}|{part_no}` → chunk_hash
///
/// part_no 用 4 字节大端：字典序等于序号序，`complete` 时顺序扫描就能按序拼块列表。
pub fn stage_part_key(upload_id: UploadId, part_no: u32) -> Vec<u8> {
    KeyBuf::new(b"s", ID_LEN + 1 + 4)
        .then(&upload_id.as_key_bytes())
        .sep_then(&part_no.to_be_bytes())
        .done()
}

/// 一个上传会话全部分片的扫描前缀。
pub fn stage_part_prefix(upload_id: UploadId) -> Vec<u8> {
    KeyBuf::new(b"s", ID_LEN + 1)
        .then(&upload_id.as_key_bytes())
        .sep_then(b"")
        .done()
}

/// 从分片 key 里取回序号。
pub fn parse_stage_part_key(key: &[u8]) -> Result<u32> {
    let head = 1 + 1 + ID_LEN + 1;
    if key.len() != head + 4 || key[0] != b's' || key[1] != SEP {
        bail!("不是合法的 stage part key");
    }
    let mut n = [0u8; 4];
    n.copy_from_slice(&key[head..]);
    Ok(u32::from_be_bytes(n))
}

/// `si|{upload_id}` → StageInfo
pub fn stage_info_key(upload_id: UploadId) -> Vec<u8> {
    KeyBuf::new(b"si", ID_LEN)
        .then(&upload_id.as_key_bytes())
        .done()
}

/// GC 待删队列里的条目类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    /// 待删 blob，键前缀 `pb`。
    Blob,
    /// 待删 chunk，键前缀 `pk`。
    Chunk,
}

impl GcKind {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Blob => b"pb",
            Self::Chunk => b"pk",
        }
    }

    fn from_tag(tag: &[u8]) -> Result<Self> {
        match tag {
            b"pb" => Ok(Self::Blob),
            b"pk" => Ok(Self::Chunk),
            _ => bail!("未知的 GC 队列前缀"),
        }
    }
}

/// `pb|{delete_after_ts}|{hash}` 或 `pk|{delete_after_ts}|{chunk_hash}` → 空
///
/// 时间戳用 8 字节**大端**：字典序等于时间序，GC 任务从头顺序扫到第一个
/// 未到期的项就能停，不用全表遍历。注意小端会让这个性质完全失效。
pub fn gc_key(kind: GcKind, delete_after_ts: u64, hash: ContentHash) -> Vec<u8> {
    KeyBuf::new(kind.tag(), 8 + 1 + HASH_LEN)
        .then(&delete_after_ts.to_be_bytes())
        .sep_then(&hash.0)
        .done()
}

/// 某类 GC 队列的扫描前缀。
pub fn gc_prefix(kind: GcKind) -> Vec<u8> {
    let tag = kind.tag();
    let mut v = Vec::with_capacity(tag.len() + 1);
    v.extend_from_slice(tag);
    v.push(SEP);
    v
}

/// 从 GC 队列 key 里取回到期时间与哈希。
///
/// 必须按偏移解码：时间戳的高位字节通常是 `0x00`，按分隔符切分会切错。
pub fn parse_gc_key(key: &[u8]) -> Result<(GcKind, u64, ContentHash)> {
    let head = 2 + 1;
    let expect = head + 8 + 1 + HASH_LEN;
    if key.len() != expect {
        bail!("不是合法的 GC key（长度 {} 应为 {expect}）", key.len());
    }
    let kind = GcKind::from_tag(&key[..2])?;
    if key[2] != SEP || key[head + 8] != SEP {
        bail!("GC key 分隔符位置不对");
    }
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&key[head..head + 8]);
    let mut hash = [0u8; HASH_LEN];
    hash.copy_from_slice(&key[head + 8 + 1..]);
    Ok((kind, u64::from_be_bytes(ts), ContentHash(hash)))
}

/// `auth` 列族：`at|{token_hash}` → agent token 记录。
///
/// 存的是 token 的哈希，明文只在签发响应里出现一次。
pub fn agent_token_key(token_hash: ContentHash) -> Vec<u8> {
    KeyBuf::new(b"at", HASH_LEN).then(&token_hash.0).done()
}

/// `auth` 列族：`atp|{project_id}|{token_hash}` → 空
///
/// 按项目列出 agent token（只显示前缀，不回显完整值）。
pub fn agent_token_by_project_key(project_id: ProjectId, token_hash: ContentHash) -> Vec<u8> {
    KeyBuf::new(b"atp", ID_LEN + 1 + HASH_LEN)
        .then(&project_id.as_key_bytes())
        .sep_then(&token_hash.0)
        .done()
}

/// 某个画布全部 agent token 的扫描前缀。
pub fn agent_token_by_project_prefix(project_id: ProjectId) -> Vec<u8> {
    KeyBuf::new(b"atp", ID_LEN + 1)
        .then(&project_id.as_key_bytes())
        .sep_then(b"")
        .done()
}

// Durable jobs use Meta/Idx, deliberately outside the destructive legacy queue.
pub fn job_prefix() -> Vec<u8> {
    b"job2\0".to_vec()
}
pub fn job_key(id: &str) -> Vec<u8> {
    [job_prefix(), id.as_bytes().to_vec()].concat()
}
pub fn job_request_key(uid: Uid, request: &str) -> Vec<u8> {
    format!("jobreq2\0{uid}\0{request}").into_bytes()
}
pub fn job_ready_prefix() -> Vec<u8> {
    b"jobready2\0".to_vec()
}
pub fn job_ready_key(id: &str) -> Vec<u8> {
    [job_ready_prefix(), id.as_bytes().to_vec()].concat()
}
pub fn project_execution_key(id: ProjectId) -> Vec<u8> {
    format!("execution2\0{id}").into_bytes()
}
pub fn execution_token_key(hash: &str) -> Vec<u8> {
    format!("executiontoken2\0{hash}").into_bytes()
}
pub fn resource_version_key(id: CanvasId) -> Vec<u8> {
    format!("resourceversion2\0{id}").into_bytes()
}

pub fn job_canvas_prefix(cid: CanvasId) -> Vec<u8> {
    format!("jobcanvas2\0{cid}\0").into_bytes()
}
pub fn job_owner_prefix(uid: Uid) -> Vec<u8> {
    format!("jobowner2\0{uid}\0").into_bytes()
}
pub fn job_index_key(prefix: Vec<u8>, id: &str) -> Vec<u8> {
    [prefix, id.as_bytes().to_vec()].concat()
}
