//! 套餐、月度任务周期和每日注册计数。时间均以 Unix 秒存储。

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, Months, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{PlanLimits, PlansConfig};
use crate::storage::keys::{self, Uid};
use crate::storage::{Batch, Cf, Store, records};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Basic,
    Pro,
    Max,
}

impl Plan {
    pub fn limits(self, config: &PlansConfig) -> &PlanLimits {
        match self {
            Self::Basic => &config.basic,
            Self::Pro => &config.pro,
            Self::Max => &config.max,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Basic => "基础版",
            Self::Pro => "专业版",
            Self::Max => "旗舰版",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanState {
    pub plan: Plan,
    pub plan_started_at: u64,
    pub push_period_start: u64,
    pub push_used: u64,
}

impl PlanState {
    pub fn basic(created_at: u64) -> Self {
        Self {
            plan: Plan::Basic,
            plan_started_at: created_at,
            push_period_start: created_at,
            push_used: 0,
        }
    }
    pub fn current(&self, now: u64) -> Result<(Self, u64)> {
        let (start, next) = period(self.plan_started_at, now)?;
        let mut current = self.clone();
        if current.push_period_start != start {
            current.push_period_start = start;
            current.push_used = 0;
        }
        Ok((current, next))
    }
}

pub async fn get(store: &Store, uid: Uid, _created_at: u64) -> Result<PlanState> {
    match store.get(Cf::Meta, keys::user_plan_key(uid)).await? {
        Some(bytes) => serde_json::from_slice(&bytes).context("用户套餐记录损坏"),
        None => anyhow::bail!("用户缺少套餐记录，请重置旧开发数据"),
    }
}

/// 启动时同步现有套餐用户的容量上限，支持修改配置后重启生效。
pub async fn sync_plan_limits(store: &Store, config: &PlansConfig) -> Result<()> {
    let rows = store
        .scan_prefix_limit(Cf::Meta, keys::user_prefix(), usize::MAX)
        .await?;
    for (key, value) in rows {
        let uid = keys::parse_user_key(&key)?;
        let mut user: records::User = records::decode(&value)?;
        if user.role.is_admin() {
            continue;
        }
        let plan_key = keys::user_plan_key(uid);
        let existing = store.get(Cf::Meta, plan_key.clone()).await?;
        let plan = match &existing {
            Some(raw) => serde_json::from_slice::<PlanState>(raw).context("用户套餐记录损坏")?,
            None => {
                tracing::warn!(%uid, "用户缺少套餐记录，请重置旧开发数据");
                continue;
            }
        };
        let new_limit = plan.plan.limits(config).storage_bytes;
        if user.quota_bytes != new_limit {
            user.quota_bytes = new_limit;
            let mut batch = Batch::new();
            batch.put(Cf::Meta, keys::user_key(uid), records::encode(&user)?);
            store.write(batch).await?;
        }
    }
    Ok(())
}

pub fn encode(state: &PlanState) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(state)?)
}

/// 按北京时间自然日计数，不依赖进程内存或定时任务。
pub fn registration_day(now: u64) -> Result<String> {
    let utc = DateTime::<Utc>::from_timestamp(now as i64, 0).context("注册时间无效")?;
    Ok((utc + Duration::hours(8)).format("%Y-%m-%d").to_string())
}

pub async fn registered_today(store: &Store, now: u64) -> Result<u64> {
    let day = registration_day(now)?;
    match store
        .get(Cf::Meta, keys::registration_day_key(&day))
        .await?
    {
        Some(raw) => Ok(u64::from_be_bytes(
            raw.as_slice().try_into().context("每日注册计数损坏")?,
        )),
        None => Ok(0),
    }
}

/// 将原始开通日直接推进 N 个月，避免月末日期逐月漂移。
pub fn period(anchor: u64, now: u64) -> Result<(u64, u64)> {
    let origin = DateTime::<Utc>::from_timestamp(anchor as i64, 0).context("开通时间无效")?;
    let current = DateTime::<Utc>::from_timestamp(now as i64, 0).context("当前时间无效")?;
    anyhow::ensure!(current >= origin, "开通时间不能晚于当前时间");
    let months =
        (current.year() - origin.year()) * 12 + current.month() as i32 - origin.month() as i32;
    let mut index = months.max(0) as u32;
    let mut start = origin
        .checked_add_months(Months::new(index))
        .context("周期时间溢出")?;
    if start > current {
        index -= 1;
        start = origin
            .checked_add_months(Months::new(index))
            .context("周期时间溢出")?;
    }
    let mut next = origin
        .checked_add_months(Months::new(index + 1))
        .context("周期时间溢出")?;
    if next <= current {
        index += 1;
        start = next;
        next = origin
            .checked_add_months(Months::new(index + 1))
            .context("周期时间溢出")?;
    }
    Ok((start.timestamp() as u64, next.timestamp() as u64))
}
