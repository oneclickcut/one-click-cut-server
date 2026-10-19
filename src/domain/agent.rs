//! Agent token 的签发、校验与吊销。
//!
//! # 设计要点
//!
//! | 项 | 选择 | 理由 |
//! |----|------|------|
//! | 形态 | `at_` + 32 字符 base62 | 前缀让鉴权层一眼分辨出它不是 JWT |
//! | 存储 | 只存 SHA-256 | 库被拖走也不能直接拿去用 |
//! | 明文 | 只在签发响应里出现一次 | 列表接口只显示前缀 |
//! | 作用域 | 绑定**一个项目** | 覆盖项目内现有和未来的制作画布 |
//! | 权限位 | 默认只给制作文件写入 | 公共草稿和删除需单独授权 |
//! | 有效期 | 默认 24 小时 | 任务期；可提前吊销 |
//!
//! 没有 `read` 权限位——读取一律公开，不需要授权。

use anyhow::{Context, Result, bail};

use crate::storage::keys::{self, ContentHash, ProjectId, Uid};
use crate::storage::records::{self, AgentPerms, AgentToken};
use crate::storage::{Batch, Cf, Store};
use crate::util;

/// token 明文的前缀。
pub const TOKEN_PREFIX: &str = "at_";

/// 随机部分的长度。32 字符 base62 约 190 bit 熵。
const TOKEN_RANDOM_LEN: usize = 32;

/// 展示给用户的前缀长度（`at_` + 前 6 个字符）。
///
/// 够用来在列表里区分几个 token，又不足以缩小暴力破解的范围。
const DISPLAY_PREFIX_LEN: usize = TOKEN_PREFIX.len() + 6;

/// 默认有效期：24 小时。
pub const DEFAULT_TTL_SECS: u64 = 24 * 3600;

/// 有效期上限：30 天。
///
/// 设上限是因为「永不过期的 agent token」等于一个永久后门：任务早就结束了，
/// 凭证还能写。
pub const MAX_TTL_SECS: u64 = 30 * 24 * 3600;

/// 签发结果。明文只在这里出现一次。
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// **完整明文，只返回这一次。** KV 里只有它的哈希。
    pub plaintext: String,
    pub token_id: String,
    pub prefix: String,
    pub expires_at: u64,
    pub perms: AgentPerms,
}

/// 列表里的一条 token（不含明文）。
#[derive(Debug, Clone)]
pub struct TokenSummary {
    pub token_id: String,
    pub prefix: String,
    pub project_id: ProjectId,
    pub perms: AgentPerms,
    pub created_at: u64,
    pub expires_at: u64,
}

/// token 明文 → 存储用的哈希。
fn hash_token(plaintext: &str) -> ContentHash {
    ContentHash::from_bytes(util::sha256(plaintext.as_bytes()))
}

/// 签发一个绑定到指定项目的 token。
pub async fn issue(
    store: &Store,
    uid: Uid,
    project_id: ProjectId,
    perms: AgentPerms,
    ttl_secs: Option<u64>,
    now: u64,
) -> Result<IssuedToken> {
    let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    if ttl == 0 {
        bail!("有效期必须大于 0");
    }
    if ttl > MAX_TTL_SECS {
        bail!(
            "有效期不能超过 {} 天，永不过期的 token 等于永久后门",
            MAX_TTL_SECS / 86400
        );
    }

    let plaintext = format!("{TOKEN_PREFIX}{}", util::random_base62(TOKEN_RANDOM_LEN));
    let hash = hash_token(&plaintext);
    let prefix = plaintext[..DISPLAY_PREFIX_LEN].to_owned();
    let expires_at = now + ttl;

    let record = AgentToken {
        v: 1,
        project_id,
        uid,
        perms,
        created_at: now,
        expires_at,
        prefix: prefix.clone(),
    };

    // 主记录与「按画布列出」的索引同批提交：只有索引没有记录会让列表里
    // 出现一个查不到的 token，反过来则让吊销时漏掉索引。
    let mut batch = Batch::new();
    batch
        .put(
            Cf::Auth,
            keys::agent_token_key(hash),
            records::encode(&record)?,
        )
        .put(
            Cf::Auth,
            keys::agent_token_by_project_key(project_id, hash),
            Vec::new(),
        );
    store.write(batch).await?;

    tracing::info!(
        %uid,
        project = %project_id,
        prefix = %prefix,
        write = perms.canvas_files_write,
        delete = perms.canvas_files_delete,
        ttl_secs = ttl,
        "已签发 agent token"
    );

    Ok(IssuedToken {
        // token_id 就是哈希的 hex，用于吊销接口——这样不需要把明文传回来。
        token_id: hash.to_hex(),
        plaintext,
        prefix,
        expires_at,
        perms,
    })
}

/// 校验一个 token 明文，返回它的记录。
///
/// 过期的 token 返回 `None` 而不是错误：调用方只需要知道「能不能用」。
pub async fn verify(store: &Store, plaintext: &str, now: u64) -> Result<Option<AgentToken>> {
    if !plaintext.starts_with(TOKEN_PREFIX) {
        return Ok(None);
    }
    let hash = hash_token(plaintext);
    let Some(raw) = store.get(Cf::Auth, keys::agent_token_key(hash)).await? else {
        return Ok(None);
    };
    let record: AgentToken = records::decode(&raw)?;

    if record.expires_at <= now {
        // 过期记录留给后台任务清理，这里只是不让它通过。
        return Ok(None);
    }
    Ok(Some(record))
}

/// 列出一个画布上未过期的 token。
pub async fn list_for_project(
    store: &Store,
    project_id: ProjectId,
    now: u64,
) -> Result<Vec<TokenSummary>> {
    let index_keys = store
        .scan_prefix_keys(
            Cf::Auth,
            keys::agent_token_by_project_prefix(project_id),
            usize::MAX,
        )
        .await?;

    let mut out = Vec::new();
    for key in index_keys {
        let Ok(hash) = keys::parse_agent_token_index_key(&key) else {
            tracing::warn!(len = key.len(), "agent token 索引键异常，跳过");
            continue;
        };

        let Some(raw) = store.get(Cf::Auth, keys::agent_token_key(hash)).await? else {
            continue;
        };
        let record: AgentToken = records::decode(&raw)?;
        if record.expires_at <= now {
            continue;
        }
        out.push(TokenSummary {
            token_id: hash.to_hex(),
            prefix: record.prefix,
            project_id: record.project_id,
            perms: record.perms,
            created_at: record.created_at,
            expires_at: record.expires_at,
        });
    }
    Ok(out)
}

/// 按 `token_id`（哈希的 hex）吊销。
///
/// 返回是否真的删掉了一条。吊销**立即生效**：鉴权每次都查 KV，不看缓存。
pub async fn revoke(store: &Store, token_id: &str) -> Result<Option<AgentToken>> {
    let hash = ContentHash::parse_hex(token_id).context("token_id 不是合法的 hex")?;
    let Some(raw) = store.get(Cf::Auth, keys::agent_token_key(hash)).await? else {
        return Ok(None);
    };
    let record: AgentToken = records::decode(&raw)?;

    let mut batch = Batch::new();
    batch.delete(Cf::Auth, keys::agent_token_key(hash)).delete(
        Cf::Auth,
        keys::agent_token_by_project_key(record.project_id, hash),
    );
    store.write(batch).await?;

    tracing::info!(
        project = %record.project_id,
        prefix = %record.prefix,
        "已吊销 agent token"
    );
    Ok(Some(record))
}

/// 清理过期的 token 记录。S8 的后台任务会定期调用。
pub async fn purge_expired(store: &Store, now: u64, max_items: usize) -> Result<u64> {
    let rows = store
        .scan_prefix_limit(Cf::Auth, keys::agent_token_prefix(), max_items)
        .await?;

    let mut batch = Batch::new();
    let mut n = 0u64;
    for (key, value) in rows {
        let record: AgentToken = records::decode(&value)?;
        if record.expires_at > now {
            continue;
        }
        // 注意用的是 `parse_agent_token_key` 而不是 `parse_hash_key`：
        // 后者只认单字符前缀，两字符的 `at|` 会被它拒掉。
        let Ok(hash) = keys::parse_agent_token_key(&key) else {
            tracing::warn!(len = key.len(), "agent token 主键异常，跳过");
            continue;
        };
        batch.delete(Cf::Auth, key).delete(
            Cf::Auth,
            keys::agent_token_by_project_key(record.project_id, hash),
        );
        n += 1;
    }
    store.write(batch).await?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token形态() {
        // `at_` 前缀让鉴权层一眼分辨出它不是 JWT，三条鉴权分支因此不会串。
        let plaintext = format!("{TOKEN_PREFIX}{}", util::random_base62(TOKEN_RANDOM_LEN));
        assert!(plaintext.starts_with("at_"));
        assert_eq!(plaintext.len(), 3 + 32);
        assert_eq!(&plaintext[..DISPLAY_PREFIX_LEN].len(), &9);
    }

    #[test]
    fn 相同明文哈希相同不同明文哈希不同() {
        let a = "at_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "at_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert_eq!(hash_token(a), hash_token(a));
        assert_ne!(hash_token(a), hash_token(b));
    }

    #[test]
    fn 默认权限只给写入() {
        // agent 出 bug 时最坏情况是写脏文件，而不是清空画布。
        let p = AgentPerms::default();
        assert!(p.canvas_files_write);
        assert!(!p.canvas_files_delete);
    }
}
