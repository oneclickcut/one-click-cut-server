//! 鉴权中间件：从 `Authorization` 头解析出调用者身份。
//!
//! # 三条互不兼容的分支
//!
//! ```text
//! Bearer <jwt>      role=user  → 只能操作自己名下的文件夹与项目
//! Bearer <jwt>      role=admin → 可操作任意用户空间 + 运维接口
//! Bearer at_<...>              → 仅限绑定项目内获授权的操作
//! ```
//!
//! **绝不互相兼容**：不允许拿 agent token 调管理接口，也不允许拿 JWT 放进 URL
//! 当读取凭证。这是接口文档第 1 节的硬要求。
//!
//! # 为什么每次请求都查一次 KV
//!
//! JWT 是自包含的，里面的 `role` 是签发那一刻的快照。如果直接信它，那么
//! 「禁用一个账户」要等到它手上所有 token 过期才生效。所以这里拿到 claims
//! 之后一定会再查一次用户记录，确认账户还在、没被禁用、角色没变。
//! 代价是每个请求一次点查——`meta` 列族开了 point lookup 与 bloom filter，
//! 这个代价可以接受。

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use super::AppState;
use super::error::AppError;
use crate::domain::identity;
use crate::storage::keys::{ContentHash, ProjectId, Uid};
use crate::storage::records::{AgentPerms, Role};

/// agent token 的明文前缀。
pub const AGENT_TOKEN_PREFIX: &str = "at_";

/// 已认证的调用者。
#[derive(Debug, Clone)]
pub enum Caller {
    /// 普通用户或管理员。
    User { uid: Uid, role: Role },
    /// Agent，只能操作绑定项目内获授权的资源。
    Agent {
        uid: Uid,
        project_id: ProjectId,
        perms: AgentPerms,
        token_hash: ContentHash,
    },
}

impl Caller {
    pub async fn ensure_current(&self, state: &AppState) -> Result<(), AppError> {
        if let Self::Agent {
            token_hash,
            project_id,
            uid,
            ..
        } = self
        {
            let raw = state
                .store
                .get(
                    crate::storage::Cf::Auth,
                    crate::storage::keys::agent_token_key(*token_hash),
                )
                .await?
                .ok_or(AppError::Unauthorized)?;
            let record: crate::storage::records::AgentToken =
                crate::storage::records::decode(&raw).map_err(AppError::Internal)?;
            if record.expires_at <= crate::util::now_secs()
                || record.project_id != *project_id
                || record.uid != *uid
            {
                return Err(AppError::Unauthorized);
            }
            let user = identity::find_by_uid(&state.store, *uid)
                .await?
                .ok_or(AppError::Unauthorized)?;
            if user.disabled {
                return Err(AppError::Unauthorized);
            }
        }
        Ok(())
    }
    /// 这个调用者所操作的用户空间。
    pub fn uid(&self) -> Uid {
        match self {
            Self::User { uid, .. } => *uid,
            Self::Agent { uid, .. } => *uid,
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Self::User { role, .. } if role.is_admin())
    }

    /// 是否可以操作 `owner` 的空间。
    ///
    /// 管理员可以操作任意用户空间，但**调用方必须记审计日志**——这是唯一能
    /// 越权的身份。
    pub fn can_access_space(&self, owner: Uid) -> bool {
        self.is_admin() || self.uid() == owner
    }
}

/// 只允许用户或管理员（拒绝 agent token）的提取器。
///
/// 单独做一个类型而不是在 handler 里判断，是为了让「这个接口 agent 能不能调」
/// 在函数签名上就看得出来，漏判会编译不过而不是运行时才发现。
#[derive(Debug, Clone)]
pub struct UserCaller {
    pub uid: Uid,
    pub role: Role,
}

impl UserCaller {
    pub fn is_admin(&self) -> bool {
        self.role.is_admin()
    }

    pub fn can_access_space(&self, owner: Uid) -> bool {
        self.is_admin() || self.uid == owner
    }
}

/// 只允许管理员的提取器。
#[derive(Debug, Clone)]
pub struct AdminCaller {
    pub uid: Uid,
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts).ok_or(AppError::Unauthorized)?;

        // agent token 走完全独立的一条分支：不解 JWT、不查用户角色，
        // 查绑定项目和操作权限。
        if token.starts_with(AGENT_TOKEN_PREFIX) {
            let record =
                crate::domain::agent::verify(&state.store, &token, crate::util::now_secs())
                    .await?
                    .ok_or(AppError::Unauthorized)?;

            // 画布所属用户如果被禁用，它的 agent token 也该失效——
            // 否则禁用一个账户之后它的 agent 还能继续写。
            let owner = identity::find_by_uid(&state.store, record.uid)
                .await?
                .ok_or(AppError::Unauthorized)?;
            if owner.disabled {
                return Err(AppError::Unauthorized);
            }
            if crate::domain::space::get_project(&state.store, record.uid, record.project_id)
                .await?
                .is_none()
            {
                return Err(AppError::Unauthorized);
            }

            return Ok(Self::Agent {
                uid: record.uid,
                project_id: record.project_id,
                perms: record.perms,
                token_hash: ContentHash::from_bytes(crate::util::sha256(token.as_bytes())),
            });
        }

        let claims = state
            .identity
            .verify_access(&token)
            .map_err(|_| AppError::Unauthorized)?;
        let uid = Uid::parse(&claims.sub).map_err(|_| AppError::Unauthorized)?;

        // 实时查 KV：token 里的 role 只是签发时的快照，不能信。
        let user = identity::find_by_uid(&state.store, uid)
            .await?
            .ok_or(AppError::Unauthorized)?;
        if user.disabled {
            return Err(AppError::Unauthorized);
        }

        // 无到期点只属于当前读取名单。移出名单后，所有用户接口都拒绝旧永久 JWT。
        if claims.exp.is_none()
            && !state
                .config
                .task_queue
                .reader_usernames
                .contains(&user.username)
        {
            return Err(AppError::Unauthorized);
        }

        Ok(Self::User {
            uid,
            role: user.role,
        })
    }
}

impl FromRequestParts<AppState> for UserCaller {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match Caller::from_request_parts(parts, state).await? {
            Caller::User { uid, role } => Ok(Self { uid, role }),
            // agent token 合法，但这个接口不接受它。403 而不是 401：
            // 凭证本身没问题，是权限不够。
            Caller::Agent { .. } => Err(AppError::Forbidden),
        }
    }
}

impl FromRequestParts<AppState> for AdminCaller {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let caller = UserCaller::from_request_parts(parts, state).await?;
        if !caller.is_admin() {
            return Err(AppError::Forbidden);
        }
        Ok(Self { uid: caller.uid })
    }
}

/// 从 `Authorization: Bearer <token>` 里取出 token。
fn bearer_token(parts: &Parts) -> Option<String> {
    let raw = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let rest = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?;
    let t = rest.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

/// 审计日志。管理员越权操作别人的空间时必须调用。
///
/// 这是唯一能跨用户操作的身份，所以「谁、什么时候、动了谁的哪个资源」必须留痕。
pub fn audit_admin_action(admin: Uid, action: &str, target_owner: Uid, target: &str) {
    tracing::warn!(
        audit = true,
        admin = %admin,
        action,
        target_owner = %target_owner,
        target,
        "管理员跨用户操作"
    );
}

#[cfg(test)]
mod tests {
    use axum::http::Request;

    use super::*;

    fn parts_with_auth(value: Option<&str>) -> Parts {
        let mut b = Request::builder().uri("/");
        if let Some(v) = value {
            b = b.header(axum::http::header::AUTHORIZATION, v);
        }
        b.body(()).expect("构造请求").into_parts().0
    }

    #[test]
    fn 解析bearer头() {
        assert_eq!(
            bearer_token(&parts_with_auth(Some("Bearer abc123"))).as_deref(),
            Some("abc123")
        );
        // 大小写宽容：有些客户端会发小写。
        assert_eq!(
            bearer_token(&parts_with_auth(Some("bearer abc123"))).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            bearer_token(&parts_with_auth(Some("Bearer  padded  "))).as_deref(),
            Some("padded")
        );
    }

    #[test]
    fn 非法的authorization头一律视为缺失() {
        for v in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer "),
            Some("Basic dXNlcjpwYXNz"),
            Some("abc123"),
        ] {
            assert!(
                bearer_token(&parts_with_auth(v)).is_none(),
                "应视为缺失: {v:?}"
            );
        }
    }

    #[test]
    fn 管理员可跨空间普通用户不可() {
        let a = Uid::new();
        let b = Uid::new();

        let user = Caller::User {
            uid: a,
            role: Role::User,
        };
        assert!(user.can_access_space(a), "自己的空间可以");
        assert!(!user.can_access_space(b), "别人的空间不行");
        assert!(!user.is_admin());

        let admin = Caller::User {
            uid: a,
            role: Role::Admin,
        };
        assert!(admin.can_access_space(b), "管理员可跨用户");
        assert!(admin.is_admin());
    }

    #[test]
    fn agent不是管理员() {
        // 防回归：agent token 永远不该被当成管理身份。
        let agent = Caller::Agent {
            uid: Uid::new(),
            project_id: ProjectId::new(),
            perms: AgentPerms::default(),
            token_hash: ContentHash::from_bytes([0; 32]),
        };
        assert!(!agent.is_admin());
        assert!(
            !agent.can_access_space(Uid::new()),
            "agent 不能操作别人的空间"
        );
    }
}
