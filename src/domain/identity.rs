//! 用户、角色、密码校验、JWT 签发。
//!
//! # 鉴权模型
//!
//! 读取一律公开，所以这里守的只有写入面。三类主体的凭证互不兼容：
//!
//! | 主体 | 凭证 | 有效期 |
//! |------|------|--------|
//! | 用户 | JWT `role=user` | JWT 90 天；队列读取名单内不按时间过期 |
//! | 管理账户 | JWT `role=admin` | 同上 |
//! | AI Agent | Agent Token（S7） | 任务期，默认 24 小时 |
//!
//! # 一条硬规则
//!
//! **JWT claim 里只放 `uid` / `role` / `exp` / `jti`，不放权限细节。**
//! 具体权限一律实时查 KV——否则改权限或禁用账户要等 token 过期才生效，
//! 而「立即吊销」在 agent 出问题时是刚需。

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::domain::plans::{self, PlanState};
use crate::storage::keys::{self, Uid};
use crate::storage::records::{Role, User};
use crate::storage::{Batch, Cf, Store, records};
use crate::util;

/// 普通用户 JWT 有效期：90 天；到期后重新登录。
pub const ACCESS_TTL_SECS: u64 = 90 * 24 * 3600;

/// 密码长度下限。
pub const MIN_PASSWORD_LEN: usize = 8;

/// 密码长度上限。
///
/// argon2 本身不限长度，但不设上限会给出一条免费的 DoS：超长密码会让每次登录
/// 都做一次大内存哈希。
pub const MAX_PASSWORD_LEN: usize = 256;

/// JWT 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// subject：用户 id。
    pub sub: String,
    pub role: Role,
    /// 过期时间，Unix 秒；缺省时必须额外验证当前队列读取名单资格。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,
    /// 签发时间。
    pub iat: u64,
    /// token 唯一 id，用于审计串联。
    pub jti: String,
}

/// 登录成功后返回的唯一凭证。
#[derive(Debug, Clone)]
pub struct LoginToken {
    pub access_token: String,
    pub expires_in: Option<u64>,
    pub role: Role,
}

/// 身份服务。持有签名密钥。
#[derive(Clone)]
pub struct Identity {
    encoding: jsonwebtoken::EncodingKey,
    decoding: jsonwebtoken::DecodingKey,
}

impl Identity {
    /// 用 HS256 密钥构造。
    ///
    /// 密钥太短会让签名可被暴力破解，所以这里直接拒绝——**宁可起不来，
    /// 也不要带着一个弱密钥上线**。
    pub fn new(secret: &str) -> Result<Self> {
        if secret.len() < 32 {
            bail!(
                "JWT 密钥至少需要 32 字节，当前 {} 字节。生产用 OCC_JWT_SECRET 注入",
                secret.len()
            );
        }
        Ok(Self {
            encoding: jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
            decoding: jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        })
    }

    /// 签发 access token。
    pub fn issue_access(&self, uid: Uid, role: Role, now: u64, ttl: Option<u64>) -> Result<String> {
        let claims = Claims {
            sub: uid.to_text(),
            role,
            exp: ttl.map(|seconds| now + seconds),
            iat: now,
            jti: util::random_base62(16),
        };
        jsonwebtoken::encode(&jsonwebtoken::Header::default(), &claims, &self.encoding)
            .context("签发 JWT 失败")
    }

    /// 校验 access token 并取出载荷。
    ///
    /// 验证签名及存在的过期时间。无期限资格、禁用状态与当前角色由调用方
    /// 实时查询，成功解码本身不代表已获授权。
    pub fn verify_access(&self, token: &str) -> Result<Claims> {
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.required_spec_claims.remove("exp");
        validation.validate_exp = true;
        // 不用 aud / iss，显式关掉避免库的默认行为变化。
        validation.validate_aud = false;

        let data = jsonwebtoken::decode::<Claims>(token, &self.decoding, &validation)
            .context("JWT 校验失败")?;
        Ok(data.claims)
    }
}

// ---------------------------------------------------------------------------
// 密码
// ---------------------------------------------------------------------------

/// 校验密码强度。
///
/// 只做长度与「不能全是空白」两条。不强制大小写数字符号混合：那类规则会把用户
/// 逼向 `Password1!` 这种可预测的形状，长度才是真正有效的维度。
pub fn validate_password(password: &str) -> Result<()> {
    if password.len() < MIN_PASSWORD_LEN {
        bail!("密码至少需要 {MIN_PASSWORD_LEN} 个字符");
    }
    if password.len() > MAX_PASSWORD_LEN {
        bail!("密码不能超过 {MAX_PASSWORD_LEN} 个字符");
    }
    if password.trim().is_empty() {
        bail!("密码不能全是空白字符");
    }
    Ok(())
}

/// 哈希密码，返回 PHC 字符串。
///
/// PHC 字符串自带算法、参数与 salt，所以**不需要单独存 salt 字段**，
/// 而且以后调 argon2 参数时旧哈希仍然能验证。
pub fn hash_password(password: &str) -> Result<String> {
    use argon2::Argon2;
    use argon2::password_hash::PasswordHasher;

    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("密码哈希失败: {e}"))?;
    Ok(hash.to_string())
}

/// 校验密码。
///
/// 返回 `Ok(false)` 表示密码不对，`Err` 表示存储里的哈希本身有问题。
/// 两者要分开：后者是数据损坏，不该当成「密码错误」糊过去。
pub fn verify_password(password: &str, phc: &str) -> Result<bool> {
    use argon2::Argon2;
    use argon2::password_hash::PasswordVerifier;
    use argon2::password_hash::phc::PasswordHash;

    let parsed = PasswordHash::new(phc).map_err(|e| anyhow::anyhow!("存储的密码哈希非法: {e}"))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::PasswordInvalid) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("密码校验出错: {e}")),
    }
}

// ---------------------------------------------------------------------------
// 用户
// ---------------------------------------------------------------------------

/// 注册一个新用户。
///
/// 用户名重复返回 `Ok(None)`，让调用方映射成 409。
pub async fn register(
    store: &Store,
    username: &str,
    password: &str,
    role: Role,
    now: u64,
) -> Result<Option<(Uid, User)>> {
    match register_inner(store, username, password, role, now, None).await? {
        RegistrationOutcome::Created(uid, user) => Ok(Some((uid, user))),
        RegistrationOutcome::UsernameTaken => Ok(None),
        RegistrationOutcome::DailyLimitReached => unreachable!("内部注册不受每日上限约束"),
    }
}

pub enum RegistrationOutcome {
    Created(Uid, User),
    UsernameTaken,
    DailyLimitReached,
}

/// 公开注册；调用方必须在 operation_lock 下执行，确保检查与写入之间无竞争。
pub async fn register_public(
    store: &Store,
    username: &str,
    password: &str,
    now: u64,
    basic_storage_bytes: u64,
    daily_limit: u64,
) -> Result<RegistrationOutcome> {
    register_inner(
        store,
        username,
        password,
        Role::User,
        now,
        Some((basic_storage_bytes, daily_limit)),
    )
    .await
}

async fn register_inner(
    store: &Store,
    username: &str,
    password: &str,
    role: Role,
    now: u64,
    policy: Option<(u64, u64)>,
) -> Result<RegistrationOutcome> {
    keys::validate_username(username)?;
    validate_password(password)?;

    if store
        .get(Cf::Idx, keys::username_key(username)?)
        .await?
        .is_some()
    {
        return Ok(RegistrationOutcome::UsernameTaken);
    }

    let (quota_bytes, day_key, next_count) = if let Some((storage, daily_limit)) = policy {
        let count = plans::registered_today(store, now).await?;
        if count >= daily_limit {
            return Ok(RegistrationOutcome::DailyLimitReached);
        }
        let day = plans::registration_day(now)?;
        (
            storage,
            Some(keys::registration_day_key(&day)),
            Some(count.checked_add(1).context("每日注册计数溢出")?),
        )
    } else {
        (0, None, None)
    };

    let uid = Uid::new();
    let user = User {
        v: 1,
        username: username.to_owned(),
        pwd_hash: hash_password(password)?,
        role,
        created_at: now,
        bytes_used: 0,
        quota_bytes,
        disabled: false,
    };

    // 用户记录与用户名索引必须同批提交：只有索引没有记录会让登录查到一个
    // 不存在的 uid，只有记录没有索引会让用户名可被再次注册。
    let mut batch = Batch::new();
    batch
        .put(Cf::Meta, keys::user_key(uid), records::encode(&user)?)
        .put(Cf::Idx, keys::username_key(username)?, uid.to_text());
    if role == Role::User {
        batch.put(
            Cf::Meta,
            keys::user_plan_key(uid),
            plans::encode(&PlanState::basic(now))?,
        );
    }
    if let (Some(key), Some(count)) = (day_key, next_count) {
        batch.put(Cf::Meta, key, count.to_be_bytes());
    }
    store.write(batch).await?;

    Ok(RegistrationOutcome::Created(uid, user))
}

/// 按 uid 读用户。
pub async fn find_by_uid(store: &Store, uid: Uid) -> Result<Option<User>> {
    let Some(raw) = store.get(Cf::Meta, keys::user_key(uid)).await? else {
        return Ok(None);
    };
    Ok(Some(records::decode(&raw)?))
}

/// 按用户名读用户。
pub async fn find_by_username(store: &Store, username: &str) -> Result<Option<(Uid, User)>> {
    if keys::validate_username(username).is_err() {
        // 非法用户名不可能存在，直接当查不到——不要因为格式问题返回不同的错误，
        // 那会变成一个用户名探测接口。
        return Ok(None);
    }
    let Some(raw) = store.get(Cf::Idx, keys::username_key(username)?).await? else {
        return Ok(None);
    };
    let uid = Uid::parse(std::str::from_utf8(&raw).context("用户名索引不是 UTF-8")?)?;
    Ok(find_by_uid(store, uid).await?.map(|u| (uid, u)))
}

/// 写回用户记录。
pub async fn save(store: &Store, uid: Uid, user: &User) -> Result<()> {
    store
        .put(Cf::Meta, keys::user_key(uid), records::encode(user)?)
        .await
}

// ---------------------------------------------------------------------------
// 登录
// ---------------------------------------------------------------------------

/// 登录失败的原因。
///
/// **调用方必须把所有变体映射成同一个 401 响应**，见 [`LoginError::is_credential_failure`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    /// 用户不存在。
    NoSuchUser,
    /// 密码不对。
    WrongPassword,
    /// 账户被禁用。
    Disabled,
}

impl LoginError {
    /// 是否属于「凭证问题」——这类失败对外必须长得一模一样。
    ///
    /// 区分「用户不存在」与「密码错误」会把登录接口变成用户名枚举工具：
    /// 攻击者只要看响应就能确认哪些用户名已注册。
    pub fn is_credential_failure(self) -> bool {
        matches!(
            self,
            Self::NoSuchUser | Self::WrongPassword | Self::Disabled
        )
    }
}

/// 登录：校验密码并按当前队列读取名单签发单个 JWT。
pub async fn login(
    store: &Store,
    identity: &Identity,
    username: &str,
    password: &str,
    now: u64,
    reader_usernames: &[String],
) -> Result<std::result::Result<LoginToken, LoginError>> {
    let Some((uid, user)) = find_by_username(store, username).await? else {
        // 用户不存在时也要走一遍密码哈希，否则响应时间会泄露用户是否存在。
        // 这是时序侧信道最容易被忽略的一处。
        let _ = verify_password(password, DUMMY_PHC);
        return Ok(Err(LoginError::NoSuchUser));
    };

    if !verify_password(password, &user.pwd_hash)? {
        return Ok(Err(LoginError::WrongPassword));
    }
    if user.disabled {
        return Ok(Err(LoginError::Disabled));
    }

    let expires_in = if reader_usernames.contains(&user.username) {
        None
    } else {
        Some(ACCESS_TTL_SECS)
    };
    Ok(Ok(LoginToken {
        access_token: identity.issue_access(uid, user.role, now, expires_in)?,
        expires_in,
        role: user.role,
    }))
}

/// 一个固定的合法 PHC 串，用于「用户不存在」时消耗与真实校验相当的时间。
///
/// 值本身无意义（它是 argon2id 对某个随机密码的哈希），只要参数与
/// `Argon2::default()` 一致就行。
const DUMMY_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2E$\
    YXNkZmFzZGZhc2RmYXNkZmFzZGZhc2RmYXNkZmFzZGY";

// ---------------------------------------------------------------------------
// 管理账户初始化
// ---------------------------------------------------------------------------

/// 首次启动时创建 admin 账户，返回一次性初始密码。
///
/// 已存在则返回 `None` 且不做任何修改——**重启不该重置管理员密码**，
/// 否则每次重启都会在日志里留一个可用的管理员凭证。
pub async fn ensure_admin(store: &Store, username: &str, now: u64) -> Result<Option<String>> {
    if find_by_username(store, username).await?.is_some() {
        return Ok(None);
    }
    let password = util::random_base62(24);
    match register(store, username, &password, Role::Admin, now).await? {
        Some(_) => Ok(Some(password)),
        // 并发启动两个进程时可能被对方抢先建好，这不是错误。
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 密码强度规则() {
        assert!(validate_password("12345678").is_ok());
        assert!(validate_password(&"a".repeat(MAX_PASSWORD_LEN)).is_ok());

        assert!(validate_password("1234567").is_err(), "太短");
        assert!(validate_password("").is_err(), "空");
        assert!(
            validate_password(&"a".repeat(MAX_PASSWORD_LEN + 1)).is_err(),
            "超长会给出一条免费的 DoS"
        );
        assert!(validate_password("         ").is_err(), "全是空白");
    }

    #[test]
    fn 密码哈希可验证且每次不同() {
        let a = hash_password("correct horse battery").expect("哈希");
        let b = hash_password("correct horse battery").expect("哈希");
        assert_ne!(a, b, "每个用户必须有独立 salt，同密码不该产生相同哈希");

        assert!(verify_password("correct horse battery", &a).expect("校验"));
        assert!(verify_password("correct horse battery", &b).expect("校验"));
        assert!(!verify_password("wrong password", &a).expect("校验"));
    }

    #[test]
    fn phc串自带算法与参数() {
        let phc = hash_password("some password").expect("哈希");
        assert!(phc.starts_with("$argon2id$"), "应是 argon2id: {phc}");
        assert!(phc.contains("v=19"));
        // 参数在串里，所以以后调参数不会让旧哈希失效。
        assert!(phc.contains("m=") && phc.contains("t=") && phc.contains("p="));
    }

    #[test]
    fn 损坏的哈希返回错误而不是密码错误() {
        // 这两者必须分开：把数据损坏当成「密码错误」会让用户以为自己记错了密码，
        // 而真正的问题（存储损坏）被完全掩盖。
        let r = verify_password("whatever", "not a phc string");
        assert!(r.is_err(), "非法 PHC 串应返回 Err 而不是 Ok(false)");
    }

    #[test]
    fn 用于时序补偿的哈希串是合法的() {
        // 如果这个常量不合法，「用户不存在」分支就不会真的做一次哈希，
        // 时序侧信道会悄悄回来。
        let r = verify_password("anything", DUMMY_PHC);
        assert!(r.is_ok(), "DUMMY_PHC 必须能被解析: {r:?}");
        assert!(!r.expect("已确认是 Ok"), "它不该匹配任何真实密码");
    }

    #[test]
    fn jwt签发与校验() {
        let id = Identity::new(&"k".repeat(32)).expect("构造");
        let uid = Uid::new();
        // 必须用真实时间：`verify_access` 的过期判断比的是真实时钟，
        // 写死的时间戳会随着墙上时间推移而突然开始失败。
        let now = util::now_secs();

        let token = id
            .issue_access(uid, Role::Admin, now, Some(ACCESS_TTL_SECS))
            .expect("签发");
        let claims = id.verify_access(&token).expect("校验");
        assert_eq!(claims.sub, uid.to_text());
        assert_eq!(claims.role, Role::Admin);
        assert_eq!(claims.exp, Some(now + ACCESS_TTL_SECS));
        assert!(!claims.jti.is_empty(), "jti 用于审计串联");
    }

    #[test]
    fn 过期的token被拒() {
        let id = Identity::new(&"k".repeat(32)).expect("构造");
        // 签一个一小时前就该过期的 token（库默认有几十秒的时钟容差，
        // 所以要明显越过它）。
        let long_ago = util::now_secs() - 3600 - ACCESS_TTL_SECS;
        let token = id
            .issue_access(Uid::new(), Role::User, long_ago, Some(ACCESS_TTL_SECS))
            .expect("签发");
        assert!(
            id.verify_access(&token).is_err(),
            "过期的 access token 必须被拒"
        );
    }

    #[test]
    fn 弱密钥被拒() {
        assert!(Identity::new("").is_err());
        assert!(Identity::new("short").is_err());
        assert!(Identity::new(&"k".repeat(31)).is_err());
        assert!(Identity::new(&"k".repeat(32)).is_ok());
    }

    #[test]
    fn 换密钥后旧token失效() {
        let uid = Uid::new();
        let now = util::now_secs();
        let a = Identity::new(&"a".repeat(32)).expect("构造");
        let b = Identity::new(&"b".repeat(32)).expect("构造");

        let token = a
            .issue_access(uid, Role::User, now, Some(ACCESS_TTL_SECS))
            .expect("签发");
        assert!(a.verify_access(&token).is_ok());
        assert!(
            b.verify_access(&token).is_err(),
            "换了密钥旧 token 必须失效"
        );
    }

    #[test]
    fn 篡改的token被拒() {
        let id = Identity::new(&"k".repeat(32)).expect("构造");
        let token = id
            .issue_access(
                Uid::new(),
                Role::User,
                util::now_secs(),
                Some(ACCESS_TTL_SECS),
            )
            .expect("签发");

        // 改 payload 而不改签名。
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let tampered = format!("{}.{}x.{}", parts[0], parts[1], parts[2]);
        assert!(id.verify_access(&tampered).is_err());

        // 只留 header + payload，去掉签名。
        let unsigned = format!("{}.{}.", parts[0], parts[1]);
        assert!(
            id.verify_access(&unsigned).is_err(),
            "无签名的 token 必须被拒"
        );
    }

    #[test]
    fn 登录失败原因对外一律相同() {
        // 这条测试锁住一个安全属性：三种失败都是「凭证问题」，
        // HTTP 层必须把它们映射成同一个 401，不能区分。
        for e in [
            LoginError::NoSuchUser,
            LoginError::WrongPassword,
            LoginError::Disabled,
        ] {
            assert!(e.is_credential_failure(), "{e:?} 应归为凭证失败");
        }
    }
}
