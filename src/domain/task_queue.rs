//! 公共任务队列：每次提交新建任务，领取后立即删除，不确认、不自动重试。

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::PlansConfig;
use crate::domain::{identity, plans};
use crate::storage::Batch;
use crate::storage::keys::{self, TaskId, Uid};
use crate::storage::records::QueueTask;
use crate::storage::{Cf, Store};

pub enum PushOutcome {
    Created(TaskId),
    LimitReached {
        used: u64,
        limit: u64,
        next_reset_at: u64,
    },
}

/// 调用方在 operation_lock 下执行，任务与计数同批持久化。
pub async fn push_limited(
    store: &Store,
    uid: Uid,
    payload: Value,
    config: &PlansConfig,
    now: u64,
) -> Result<PushOutcome> {
    let mut batch = Batch::new();
    if let Some(limit) = stage_submission(store, &mut batch, uid, config, now).await? {
        return Ok(limit);
    }
    let id = TaskId::new();
    let task = QueueTask {
        v: 1,
        task_id: id.to_string(),
        user_id: uid.to_string(),
        created_at: now,
        payload,
    };
    batch.put(
        Cf::TaskQueue,
        keys::task_queue_key(id),
        serde_json::to_vec(&task)?,
    );
    store.write(batch).await?;
    Ok(PushOutcome::Created(id))
}

pub async fn push(store: &Store, uid: Uid, payload: Value) -> Result<TaskId> {
    let id = TaskId::new();
    let task = QueueTask {
        v: 1,
        task_id: id.to_string(),
        user_id: uid.to_string(),
        created_at: crate::util::now_secs(),
        payload,
    };
    store
        .put(
            Cf::TaskQueue,
            keys::task_queue_key(id),
            serde_json::to_vec(&task)?,
        )
        .await?;
    Ok(id)
}

pub async fn pop(store: &Store, limit: usize) -> Result<Vec<QueueTask>> {
    store.pop_queue(limit, decode_task).await
}

pub async fn list(store: &Store) -> Result<Vec<QueueTask>> {
    store.list_queue(decode_task).await
}

fn decode_task(raw: &[u8]) -> Result<QueueTask> {
    let task: QueueTask = serde_json::from_slice(raw).context("任务记录损坏")?;
    anyhow::ensure!(task.v == 1, "不支持的任务记录版本: {}", task.v);
    Ok(task)
}

/// Shared quota operation; no queue side effect. Caller commits this batch with the new task.
pub async fn stage_submission(
    store: &Store,
    batch: &mut Batch,
    uid: Uid,
    config: &PlansConfig,
    now: u64,
) -> Result<Option<PushOutcome>> {
    let user = identity::find_by_uid(store, uid)
        .await?
        .context("提交用户不存在")?;
    if user.role.is_admin() {
        return Ok(None);
    }
    let state = plans::get(store, uid, user.created_at).await?;
    let (mut current, next) = state.current(now)?;
    let limit = current.plan.limits(config).monthly_push_limit;
    if current.push_used >= limit {
        return Ok(Some(PushOutcome::LimitReached {
            used: current.push_used,
            limit,
            next_reset_at: next,
        }));
    }
    current.push_used += 1;
    batch.put(Cf::Meta, keys::user_plan_key(uid), plans::encode(&current)?);
    Ok(None)
}
