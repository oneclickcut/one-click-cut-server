//! 队列持久化、整批失败与请求取消后的并发保护。
#[path = "../common/mod.rs"]
mod common;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use one_click_cut_server::domain::task_queue;
use one_click_cut_server::storage::{
    Cf,
    keys::{self, TaskId, Uid},
};
use serde_json::json;

#[tokio::test]
async fn 重开数据库保留未领取任务且已领取不再出现() {
    let db = common::temp_store();
    let uid = Uid::new();
    let first = task_queue::push(&db.store, uid, json!({"item":1}))
        .await
        .unwrap();
    let second = task_queue::push(&db.store, uid, json!({"item":2}))
        .await
        .unwrap();
    let db = db.reopen();
    let tasks = task_queue::pop(&db.store, 1).await.unwrap();
    assert_eq!(tasks.len(), 1);
    let consumed = &tasks[0].task_id;
    let remaining = if *consumed == first.to_string() {
        second
    } else {
        first
    };
    let db = db.reopen();
    let tasks = task_queue::pop(&db.store, 100).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].task_id, remaining.to_string());
    assert_eq!(tasks[0].user_id, uid.to_string());
    let db = db.reopen();
    assert!(task_queue::pop(&db.store, 1).await.unwrap().is_empty());
}

#[tokio::test]
async fn 批次中存在坏记录或未知版本时不删除任何任务() {
    let db = common::temp_store();
    let good = task_queue::push(&db.store, Uid::new(), json!({"valid":true}))
        .await
        .unwrap();
    // 固定较大的合法 ULID，让正常记录先被解码，验证失败时整批保留。
    let bad = TaskId::parse("7ZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
    let bad_key = keys::task_queue_key(bad);
    db.store
        .put(Cf::TaskQueue, bad_key.clone(), b"not json".to_vec())
        .await
        .unwrap();
    assert!(task_queue::pop(&db.store, 100).await.is_err());
    assert!(task_queue::list(&db.store).await.is_err());
    assert!(
        db.store
            .get(Cf::TaskQueue, keys::task_queue_key(good))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db.store
            .get(Cf::TaskQueue, bad_key.clone())
            .await
            .unwrap()
            .is_some()
    );

    let mut raw: serde_json::Value = serde_json::from_slice(
        &db.store
            .get(Cf::TaskQueue, keys::task_queue_key(good))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    raw["v"] = json!(2);
    db.store
        .put(
            Cf::TaskQueue,
            bad_key.clone(),
            serde_json::to_vec(&raw).unwrap(),
        )
        .await
        .unwrap();
    assert!(task_queue::pop(&db.store, 100).await.is_err());
    assert!(
        db.store
            .get(Cf::TaskQueue, keys::task_queue_key(good))
            .await
            .unwrap()
            .is_some()
    );

    db.store.delete(Cf::TaskQueue, bad_key).await.unwrap();
    assert_eq!(
        task_queue::pop(&db.store, 1).await.unwrap()[0].task_id,
        good.to_string()
    );
}

#[tokio::test]
async fn 非法领取数量不会删除数据() {
    let db = common::temp_store();
    task_queue::push(&db.store, Uid::new(), json!(null))
        .await
        .unwrap();
    for limit in [0, 101, usize::MAX] {
        assert!(task_queue::pop(&db.store, limit).await.is_err());
    }
    assert_eq!(task_queue::pop(&db.store, 1).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 取消领取future不提前释放存储锁() {
    let db = common::temp_store();
    task_queue::push(&db.store, Uid::new(), json!({"item":1}))
        .await
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let store = db.store.clone();
    let first_finished = Arc::clone(&finished);
    let first = tokio::spawn(async move {
        store
            .pop_queue(1, move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                first_finished.store(true, Ordering::Release);
                Ok(())
            })
            .await
    });
    // 确认第一个领取已经在阻塞线程里持锁，才取消其异步调用方。
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let store = db.store.clone();
    let second = tokio::spawn(async move {
        let tasks = task_queue::pop(&store, 1).await.unwrap();
        assert!(
            finished.load(Ordering::Acquire),
            "第二个领取不能越过仍在执行的第一个领取"
        );
        assert!(tasks.is_empty(), "已领取的任务不能被重复返回");
    });
    // 第一个领取取消后，写入仍可进行，不被领取锁阻塞。
    let new = task_queue::push(&db.store, Uid::new(), json!({"item":2}))
        .await
        .unwrap();
    // 移除刚写的任务，以便第二个领取只能观察第一个领取是否重复。
    db.store
        .delete(Cf::TaskQueue, keys::task_queue_key(new))
        .await
        .unwrap();
    release_tx.send(()).unwrap();
    second.await.unwrap();
}
