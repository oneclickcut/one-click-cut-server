//! 容量统计与限额。
//!
//! # 配额按逻辑大小算，不按物理占用算
//!
//! 三个 FileEntry 引用同一份内容时，配额算三份。理由是**用户必须能预测自己
//! 还能存多少**——按物理占用算的话，去重命中率会让「我还剩多少空间」变成
//! 一个不可解释的数字。去重省下的空间是服务端的收益，不是用户的额度。

use anyhow::Result;

use crate::storage::keys::Uid;
use crate::storage::records::User;
use crate::storage::{Cf, Store, keys, records};

/// 一次配额检查的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaCheck {
    /// 有空间。
    Ok,
    /// 会超出配额。附上「还剩多少」，让调用方给出有用的错误信息。
    Exceeded { used: u64, limit: u64, need: u64 },
}

impl QuotaCheck {
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// 写入前检查配额。
///
/// `delta` 是这次写入会**增加**的字节数（覆盖写时是差值，可能为 0）。
/// `quota_bytes == 0` 表示不限。
pub async fn check(store: &Store, uid: Uid, delta: u64) -> Result<QuotaCheck> {
    let Some(user) = crate::domain::identity::find_by_uid(store, uid).await? else {
        // 用户不存在时不放行——上层会因为别的原因先失败，但这里不该是那个缺口。
        return Ok(QuotaCheck::Exceeded {
            used: 0,
            limit: 0,
            need: delta,
        });
    };
    Ok(check_user(&user, delta))
}

/// 纯函数版本，方便测试与批量场景复用。
pub fn check_user(user: &User, delta: u64) -> QuotaCheck {
    if user.quota_bytes == 0 {
        return QuotaCheck::Ok;
    }
    if user.bytes_used.saturating_add(delta) > user.quota_bytes {
        return QuotaCheck::Exceeded {
            used: user.bytes_used,
            limit: user.quota_bytes,
            need: delta,
        };
    }
    QuotaCheck::Ok
}

/// 调整用户已用字节数。
///
/// `delta` 可以为负（删除时）。用 `saturating` 是因为统计漂移不该让服务崩——
/// 真正的对账由 [`recount`] 做。
pub async fn adjust(store: &Store, uid: Uid, delta: i64) -> Result<()> {
    let Some(mut user) = crate::domain::identity::find_by_uid(store, uid).await? else {
        return Ok(());
    };
    user.bytes_used = if delta >= 0 {
        user.bytes_used.saturating_add(delta as u64)
    } else {
        user.bytes_used.saturating_sub(delta.unsigned_abs())
    };
    crate::domain::identity::save(store, uid, &user).await
}

/// 设置配额上限。0 表示不限。
pub async fn set_limit(store: &Store, uid: Uid, limit: u64) -> Result<Option<User>> {
    let Some(mut user) = crate::domain::identity::find_by_uid(store, uid).await? else {
        return Ok(None);
    };
    user.quota_bytes = limit;
    crate::domain::identity::save(store, uid, &user).await?;
    Ok(Some(user))
}

/// 重新统计一个用户的实际占用。
///
/// 扫他的全部画布，把 `bytes` 加起来。用于对账：增量维护的统计可能因为
/// 某次崩溃或 bug 而漂移，而配额是靠它执行的。
///
/// 返回 `(旧值, 新值)`。
pub async fn recount(store: &Store, uid: Uid) -> Result<Option<(u64, u64)>> {
    let Some(mut user) = crate::domain::identity::find_by_uid(store, uid).await? else {
        return Ok(None);
    };

    // 前缀由 keys 模块给：这是**唯一允许拼 key 的地方**，
    // 在这里自己拼会让键格式的改动漏掉一处。
    let rows = store
        .scan_prefix_limit(Cf::Meta, keys::user_canvas_prefix(uid), usize::MAX)
        .await?;

    let mut actual = 0u64;
    for (_, value) in rows {
        let canvas: records::Canvas = records::decode(&value)?;
        actual = actual.saturating_add(canvas.bytes);
    }

    let old = user.bytes_used;
    if old != actual {
        tracing::warn!(
            %uid,
            recorded = old,
            actual,
            "用户容量统计与实际不一致，已修正"
        );
        user.bytes_used = actual;
        crate::domain::identity::save(store, uid, &user).await?;
    }
    Ok(Some((old, actual)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::records::Role;

    fn user(used: u64, limit: u64) -> User {
        User {
            v: 1,
            username: "alice".into(),
            pwd_hash: "$argon2id$x".into(),
            role: Role::User,
            created_at: 0,
            bytes_used: used,
            quota_bytes: limit,
            disabled: false,
        }
    }

    #[test]
    fn 零表示不限() {
        assert!(check_user(&user(u64::MAX / 2, 0), u64::MAX / 4).is_ok());
    }

    #[test]
    fn 恰好等于限额放行() {
        // 边界必须是「超过才拒」，而不是「达到就拒」——否则用户永远填不满配额。
        assert!(check_user(&user(900, 1000), 100).is_ok());
        assert!(!check_user(&user(900, 1000), 101).is_ok());
    }

    #[test]
    fn 超限返回可用来给提示的数字() {
        match check_user(&user(900, 1000), 500) {
            QuotaCheck::Exceeded { used, limit, need } => {
                assert_eq!((used, limit, need), (900, 1000, 500));
            }
            QuotaCheck::Ok => panic!("应超限"),
        }
    }

    #[test]
    fn 零字节写入总是放行() {
        // 覆盖写同大小的内容时 delta 是 0，即使已经满了也该放行。
        assert!(check_user(&user(1000, 1000), 0).is_ok());
    }

    #[test]
    fn 加法溢出不会绕过检查() {
        // 用 saturating_add：溢出回绕会让一个巨大的 delta 变成小数字而通过检查。
        assert!(!check_user(&user(u64::MAX - 10, u64::MAX - 1), 100).is_ok());
    }
}
