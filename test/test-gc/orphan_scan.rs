//! S2 对账测试：孤儿 chunk 与悬挂 blobmeta。
//!
//! 两类问题的严重程度完全不同，测试也要分开钉：
//!
//! - **孤儿 chunk 是正常的**——崩在 WriteBatch 提交之前的预期残留，删掉即可；
//! - **悬挂 blobmeta 是数据损坏**——元数据在、内容没了，只能靠备份恢复，必须告警。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::{Batch, Cf, blob, gc, keys};

#[tokio::test]
async fn 正常数据对账干净() {
    let ts = common::temp_store();

    // 先放一份小文件与一份分块文件，都走正常路径。
    let small = blob::put_bytes(&ts.store, "image/jpeg", &common::pseudo_content(1024, 1))
        .await
        .expect("写入");
    let big = blob::put_bytes(
        &ts.store,
        "video/mp4",
        &common::pseudo_content(12 * 1024 * 1024, 2),
    )
    .await
    .expect("写入");
    for h in [small.hash, big.hash] {
        let mut batch = Batch::new();
        blob::stage_reference(&ts.store, &mut batch, h)
            .await
            .expect("加引用");
        ts.store.write(batch).await.expect("提交");
    }

    let report = gc::audit(&ts.store, usize::MAX).await.expect("对账");
    assert!(
        report.is_clean(),
        "正常数据不该有任何问题：孤儿 {:?}，悬挂 {:?}",
        report.orphan_chunks.len(),
        report.dangling_blobs.len()
    );
}

#[tokio::test]
async fn 识别并清理孤儿块() {
    let ts = common::temp_store();

    // 模拟「chunk 已写、WriteBatch 未提交」的崩溃：chunk 列族有值，
    // chunkmeta 里没有对应计数。这是崩溃恢复的预期残留。
    let orphan_data = common::pseudo_content(4 * 1024 * 1024, 7);
    let orphan_hash = keys::ContentHash::from_bytes(*blake3::hash(&orphan_data).as_bytes());
    ts.store
        .put(Cf::Chunk, keys::content_key(orphan_hash), orphan_data)
        .await
        .expect("直接写块，不写计数");

    let report = gc::audit(&ts.store, usize::MAX).await.expect("对账");
    assert_eq!(
        report.orphan_chunks,
        vec![orphan_hash],
        "应识别出这个孤儿块"
    );
    assert!(report.dangling_blobs.is_empty(), "不该误报成数据损坏");

    let n = gc::purge_orphans(&ts.store, &report.orphan_chunks)
        .await
        .expect("清理");
    assert_eq!(n, 1);
    assert!(
        ts.store
            .get(Cf::Chunk, keys::content_key(orphan_hash))
            .await
            .expect("get")
            .is_none(),
        "孤儿块应被删除"
    );

    // 清理后再对账应该干净。
    assert!(
        gc::audit(&ts.store, usize::MAX)
            .await
            .expect("对账")
            .is_clean()
    );
}

#[tokio::test]
async fn 孤儿扫描不会误伤正在使用的块() {
    let ts = common::temp_store();

    // 有计数的块绝不能被当成孤儿——误删会让一个好文件变成损坏状态。
    let out = blob::put_bytes(
        &ts.store,
        "video/mp4",
        &common::pseudo_content(12 * 1024 * 1024, 8),
    )
    .await
    .expect("写入");
    let mut batch = Batch::new();
    blob::stage_reference(&ts.store, &mut batch, out.hash)
        .await
        .expect("加引用");
    ts.store.write(batch).await.expect("提交");

    // 再掺一个真孤儿进去，确认扫描能区分。
    let orphan_data = common::pseudo_content(1024, 9);
    let orphan_hash = keys::ContentHash::from_bytes(*blake3::hash(&orphan_data).as_bytes());
    ts.store
        .put(Cf::Chunk, keys::content_key(orphan_hash), orphan_data)
        .await
        .expect("put");

    let report = gc::audit(&ts.store, usize::MAX).await.expect("对账");
    assert_eq!(report.orphan_chunks, vec![orphan_hash], "只有那个真孤儿");

    gc::purge_orphans(&ts.store, &report.orphan_chunks)
        .await
        .expect("清理");

    // 正常文件必须完好。
    let back = blob::read_all(&ts.store, out.hash)
        .await
        .expect("读")
        .expect("应存在");
    assert_eq!(back.len(), 12 * 1024 * 1024);
}

#[tokio::test]
async fn 识别悬挂的单块元数据() {
    let ts = common::temp_store();

    let data = common::pseudo_content(2048, 11);
    let out = blob::put_bytes(&ts.store, "image/jpeg", &data)
        .await
        .expect("写入");

    // 人为删掉内容值、留下元数据，模拟数据损坏。
    ts.store
        .delete(Cf::BlobBin, keys::content_key(out.hash))
        .await
        .expect("删内容");

    let report = gc::audit(&ts.store, usize::MAX).await.expect("对账");
    assert_eq!(
        report.dangling_blobs,
        vec![out.hash],
        "元数据指向不存在的内容应被识别为损坏"
    );
    // 这类问题不能自动「清理」——清理等于承认数据丢了。只能告警 + 从备份恢复。
    assert!(!report.is_clean());
}

#[tokio::test]
async fn 识别悬挂的分块元数据() {
    let ts = common::temp_store();

    let out = blob::put_bytes(
        &ts.store,
        "video/mp4",
        &common::pseudo_content(12 * 1024 * 1024, 12),
    )
    .await
    .expect("写入");
    let meta = blob::meta(&ts.store, out.hash)
        .await
        .expect("元数据")
        .expect("应存在");
    let chunks = match &meta.layout {
        one_click_cut_server::storage::records::BlobLayout::Chunked { chunks } => chunks.clone(),
        other => panic!("应为 Chunked，实际 {other:?}"),
    };

    // 只删中间一块：缺任何一块，整个文件就读不出来了，必须算损坏。
    ts.store
        .delete(Cf::Chunk, keys::content_key(chunks[1]))
        .await
        .expect("删块");

    let report = gc::audit(&ts.store, usize::MAX).await.expect("对账");
    assert_eq!(report.dangling_blobs, vec![out.hash], "缺一块也应判为损坏");

    // 确认读取确实会失败而不是静默返回短内容。
    assert!(
        blob::read_all(&ts.store, out.hash).await.is_err(),
        "缺块时读取应报错，不能静默返回不完整内容"
    );
}

#[tokio::test]
async fn 对账可限制扫描量() {
    let ts = common::temp_store();

    for i in 0..10u64 {
        let data = common::pseudo_content(1024, i + 1);
        let h = keys::ContentHash::from_bytes(*blake3::hash(&data).as_bytes());
        ts.store
            .put(Cf::Chunk, keys::content_key(h), data)
            .await
            .expect("put");
    }

    let report = gc::audit(&ts.store, 4).await.expect("对账");
    assert_eq!(report.orphan_chunks.len(), 4, "扫描量上限应生效");
}
