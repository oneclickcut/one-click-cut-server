//! 落盘记录的结构定义与编解码。
//!
//! 这个模块不在设计文档的模块清单里，是实现时加的：value 的二进制格式跟 key 一样
//! 属于「存储格式」，散落到各层会让格式演进无从下手。放一起还能把版本字段的纪律
//! 集中在一处。
//!
//! # 版本字段纪律
//!
//! **每个结构体第一个字段都是 `v: u16`。** 现在全是 1，作用是给以后留口：
//! 加字段用 `#[serde(default)]` 就能兼容读旧值，真要改字段语义时可以按 `v` 分支。
//! 没有这个字段的话，第一次格式变更就只能写迁移脚本全库重写。
//!
//! # 为什么用 bincode 而不是 JSON
//!
//! 元数据条目数量级是千万（两万画布 × 350 文件），每条省下的几十字节会直接
//! 决定「纯元数据能否全量吃进 block cache」。接口层仍然用 JSON，两者互不影响。

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::keys::{CanvasId, ContentHash, FolderId, ProjectId, Uid};

/// 当前记录版本。
const V: u16 = 1;

/// 公共任务队列记录。payload 是任意 JSON，整条记录用 JSON 编解码，
/// 避免 bincode 不支持 serde_json::Value 的 deserialize_any。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTask {
    pub v: u16,
    pub task_id: String,
    pub user_id: String,
    pub created_at: u64,
    pub payload: serde_json::Value,
}

/// bincode 配置。定长整数编码，避免 varint 在小结构上的解码分支开销。
fn codec() -> impl bincode::config::Config {
    bincode::config::standard()
}

/// 把记录编码成 value 字节。
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(value, codec()).context("记录编码失败")
}

/// 从 value 字节解码记录。
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (value, _) = bincode::serde::decode_from_slice(bytes, codec()).context("记录解码失败")?;
    Ok(value)
}

// ---------------------------------------------------------------------------
// 身份
// ---------------------------------------------------------------------------

/// 用户角色。管理账户就是 `role = Admin` 的普通用户记录，不单开一张表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Admin,
}

impl Role {
    pub fn is_admin(self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// `meta` 列族 `u|{uid}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    #[serde(default = "default_version")]
    pub v: u16,
    pub username: String,
    /// argon2id 的 PHC 字符串，自带每用户独立 salt 与参数，所以不需要单独存 salt。
    pub pwd_hash: String,
    pub role: Role,
    pub created_at: u64,
    /// 已用字节数。写入时增量维护，不靠扫描统计。
    pub bytes_used: u64,
    /// 配额上限，0 表示不限。
    pub quota_bytes: u64,
    /// 禁用的账户仍可读取自己的公开内容，但一切写操作与登录都拒绝。
    #[serde(default)]
    pub disabled: bool,
}

// ---------------------------------------------------------------------------
// 文件空间
// ---------------------------------------------------------------------------

/// 可见性。
///
/// 读取已经一律公开，所以它**只影响可发现性**（是否出现在公开列表与分享入口），
/// 不阻止拿到 URL 的人直接访问。详见接口文档第 2 节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

/// `meta` 列族 `d|{uid}|{folder_id}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    #[serde(default = "default_version")]
    pub v: u16,
    /// `None` 表示在根层。
    pub parent_id: Option<FolderId>,
    pub name: String,
    /// 文件夹的可见性只作为新建子项的默认值，**不参与画布的可见性判定**。
    pub visibility: Visibility,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Deleting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub v: u16,
    pub name: String,
    pub parent_id: Option<FolderId>,
    pub status: ProjectStatus,
    pub resources_canvas_id: CanvasId,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareCover {
    pub canvas_id: CanvasId,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareConfig {
    pub title: String,
    pub description: String,
    pub author_name: String,
    pub canvas_ids: Vec<CanvasId>,
    pub cover: Option<ShareCover>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectShare {
    pub v: u16,
    pub project_id: ProjectId,
    pub owner_uid: Uid,
    pub share_id: super::keys::ShareId,
    pub config: ShareConfig,
    pub shared_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryTarget {
    pub project_id: ProjectId,
    pub owner_uid: Uid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanvasRole {
    Production,
    Resources,
}

/// `meta` 列族 `c|{uid}|{canvas_id}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Canvas {
    #[serde(default = "default_version")]
    pub v: u16,
    pub project_id: ProjectId,
    pub role: CanvasRole,
    pub sort_order: i64,
    pub name: String,
    /// 12 字符随机 base62，创建时生成，**永不变**——改名不影响 URL 与 CDN 缓存。
    pub slug: String,
    pub visibility: Visibility,
    pub file_count: u64,
    pub bytes: u64,
    pub created_at: u64,
    pub updated_at: u64,
    /// manifest 里的 `entry`，指向描述时间轴的 json。本服务不解析它的内容。
    #[serde(default)]
    pub entry: Option<String>,
}

/// `idx` 列族 `cs|{canvas_slug}`。
///
/// slug 全局唯一，所以键里没有所有者信息——`/f/{slug}/{path}` 因此不必暴露
/// 用户名或 uid。但画布记录的键是 `c|{uid}|{canvas_id}`，少了 uid 读不到画布，
/// 所以两个 id 都得存在 value 里。
///
/// 另外两条索引（`un|`、`ch|`）的 value 是裸文本或空，这条走 bincode 是因为它
/// 是复合值：将来要往里加字段（比如缓存 visibility 省掉一次回查）时，
/// 有 `v` 就不用全库重写。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlugTarget {
    #[serde(default = "default_version")]
    pub v: u16,
    pub uid: Uid,
    pub canvas_id: CanvasId,
}

impl SlugTarget {
    pub fn new(uid: Uid, canvas_id: CanvasId) -> Self {
        Self {
            v: V,
            uid,
            canvas_id,
        }
    }
}

/// `meta` 列族 `f|{canvas_id}|{path}`。
///
/// 这是数量最大的记录（约 735 万条），所以字段要尽量省：path 在 key 里不重复存，
/// mime 存枚举化的类别而不是完整字符串（完整 `Content-Type` 按扩展名实时推导）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    #[serde(default = "default_version")]
    pub v: u16,
    pub hash: ContentHash,
    pub size: u64,
    pub updated_at: u64,
}

/// `meta` 列族 `fd|{canvas_id}|{dir_path}`。
///
/// 画布内的目录记录。**目录名就在 key 里，所以这条记录除了时间戳什么都不存。**
///
/// 它存在的唯一理由是让**空目录**能够持久化：非空目录不需要它，从文件 path 的
/// 前缀就能派生出来。也正因为如此，缺了某条记录不会导致任何功能不正常——
/// 目录树的真相永远是文件的 path，这条记录只是附加信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasDir {
    #[serde(default = "default_version")]
    pub v: u16,
    pub created_at: u64,
    pub updated_at: u64,
}

impl CanvasDir {
    pub fn new(now: u64) -> Self {
        Self {
            v: V,
            created_at: now,
            updated_at: now,
        }
    }
}

// ---------------------------------------------------------------------------
// 内容
// ---------------------------------------------------------------------------

/// 内容按可压缩性分到哪个列族。
///
/// 分流的目的是差异化压缩：文本用 zstd 收益很高，已压缩的 jpg/png/mp4 再压
/// 是纯 CPU 浪费。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentClass {
    /// json / txt / srt / md / html → `blob_txt`（zstd）
    Text,
    /// jpg / png 等 → `blob_bin`（不压缩）
    Binary,
}

/// 内容的存放方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobLayout {
    /// 不超过一块的内容，整个存成单个 value。
    Single { class: ContentClass },
    /// 超过一块的内容，按 4MB 切成定长块，这里记块哈希的**有序**列表。
    ///
    /// 顺序就是文件字节顺序，Range 请求按 `offset / chunk_size` 直接索引到块。
    Chunked { chunks: Vec<ContentHash> },
}

/// `blobmeta` 列族 `b|{hash}`。
///
/// **不可变**：字段全部由内容唯一决定，同一个哈希写多少次都是同一份值，
/// 所以写一次之后再也不需要更新。引用计数另存于 `r|{hash}`，见
/// [`super::keys::blob_ref_key`] 的说明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobMeta {
    #[serde(default = "default_version")]
    pub v: u16,
    pub size: u64,
    pub layout: BlobLayout,
}

impl BlobMeta {
    pub fn single(size: u64, class: ContentClass) -> Self {
        Self {
            v: V,
            size,
            layout: BlobLayout::Single { class },
        }
    }

    pub fn chunked(size: u64, chunks: Vec<ContentHash>) -> Self {
        Self {
            v: V,
            size,
            layout: BlobLayout::Chunked { chunks },
        }
    }

    /// 内容是否分块存放。
    pub fn is_chunked(&self) -> bool {
        matches!(self.layout, BlobLayout::Chunked { .. })
    }
}

// ---------------------------------------------------------------------------
// 分块上传
// ---------------------------------------------------------------------------

/// `stage` 列族 `si|{upload_id}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageInfo {
    #[serde(default = "default_version")]
    pub v: u16,
    pub canvas_id: CanvasId,
    pub path: String,
    pub created_at: u64,
    /// 客户端声明的分片大小。与存储层的 4MB chunk 不是一回事：
    /// 一个 16MB 的 part 会被切成 4 个 chunk。
    pub part_size: u32,
}

// ---------------------------------------------------------------------------
// 凭证
// ---------------------------------------------------------------------------

/// agent token 的权限位。
///
/// 没有 `read` 位——读取不需要授权了。默认只给 `write`：agent 出 bug 时
/// 最坏情况是写脏文件，而不是清空画布。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPerms {
    pub canvas_files_write: bool,
    pub canvas_files_delete: bool,
    pub canvas_create: bool,
    pub constraints_write: bool,
    pub constraints_delete: bool,
}

impl Default for AgentPerms {
    fn default() -> Self {
        Self {
            canvas_files_write: true,
            canvas_files_delete: false,
            canvas_create: false,
            constraints_write: false,
            constraints_delete: false,
        }
    }
}

/// `auth` 列族 `at|{token_hash}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToken {
    #[serde(default = "default_version")]
    pub v: u16,
    /// 绑定项目；同一所有者的其他项目也不能访问。
    pub project_id: ProjectId,
    /// 画布所属用户，鉴权时不用回查画布就能定位空间。
    pub uid: Uid,
    pub perms: AgentPerms,
    pub created_at: u64,
    pub expires_at: u64,
    /// 明文前缀，列表接口只显示这一段，完整值不回显。
    pub prefix: String,
}

fn default_version() -> u16 {
    V
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 编解码往返() {
        let entry = FileEntry {
            v: V,
            hash: ContentHash::from_bytes([7u8; 32]),
            size: 204800,
            updated_at: 1789727178,
        };
        let bytes = encode(&entry).expect("编码");
        let back: FileEntry = decode(&bytes).expect("解码");
        assert_eq!(back.hash, entry.hash);
        assert_eq!(back.size, entry.size);
        assert_eq!(back.updated_at, entry.updated_at);
        assert_eq!(back.v, V);
    }

    #[test]
    fn 文件条目编码足够紧凑() {
        // 735 万条元数据要能全量吃进 block cache，所以单条大小是硬指标。
        // 32 字节哈希 + size + 时间戳 + 版本 ≈ 45 字节。
        let entry = FileEntry {
            v: V,
            hash: ContentHash::from_bytes([0xab; 32]),
            size: 31457280,
            updated_at: 1789727178,
        };
        let n = encode(&entry).expect("编码").len();
        assert!(n <= 64, "FileEntry 编码 {n} 字节，超过 64 字节预算");
    }

    #[test]
    fn 分块布局保留块顺序() {
        let chunks: Vec<ContentHash> = (0u8..8).map(|i| ContentHash::from_bytes([i; 32])).collect();
        let meta = BlobMeta::chunked(31457280, chunks.clone());
        let back: BlobMeta = decode(&encode(&meta).expect("编码")).expect("解码");
        assert_eq!(back.size, 31457280);
        assert!(back.is_chunked());
        match &back.layout {
            BlobLayout::Chunked { chunks: got } => {
                // 顺序错了 Range 请求会读到错误的字节，这是最难排查的一类 bug。
                assert_eq!(got, &chunks, "块列表顺序必须原样保留");
            }
            other => panic!("布局应为 Chunked，实际 {other:?}"),
        }
    }

    #[test]
    fn 单块布局记录列族归属() {
        let meta = BlobMeta::single(8412, ContentClass::Text);
        let back: BlobMeta = decode(&encode(&meta).expect("编码")).expect("解码");
        assert!(!back.is_chunked());
        assert_eq!(
            back.layout,
            BlobLayout::Single {
                class: ContentClass::Text
            }
        );
    }

    #[test]
    fn agent权限默认不含删除() {
        let p = AgentPerms::default();
        assert!(p.canvas_files_write);
        assert!(
            !p.canvas_files_delete,
            "默认给删除权限会让 agent 的 bug 变成数据丢失"
        );
    }

    #[test]
    fn 解码垃圾字节应报错而不是panic() {
        let r: Result<FileEntry> = decode(&[0xff, 0x00, 0x01]);
        assert!(r.is_err(), "损坏的 value 应返回错误");
    }
}
