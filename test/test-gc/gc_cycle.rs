//! S2 GC 闭环测试：到期才删、复活则保留、级联删块、空间真的还回去。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::records::BlobLayout;
use one_click_cut_server::storage::{Batch, Cf, blob, gc, keys};

/// 写一份内容并加一个引用，返回哈希。
async fn put_and_reference(
    store: &one_click_cut_server::Store,
    mime: &str,
    data: &[u8],
) -> keys::ContentHash {
    let out = blob::put_bytes(store, mime, data).await.expect("写入");
    let mut batch = Batch::new();
    blob::stage_reference(store, &mut batch, out.hash)
        .await
        .expect("加引用");
    store.write(batch).await.expect("提交");
    out.hash
}

/// 减掉唯一的引用，让内容排进待删队列。
async fn dereference(store: &one_click_cut_server::Store, hash: keys::ContentHash, now: u64) {
    let mut batch = Batch::new();
    blob::stage_dereference(store, &mut batch, hash, now)
        .await
        .expect("减引用");
    store.write(batch).await.expect("提交");
}

#[tokio::test]
async fn 未到期不删到期才删() {
    let ts = common::temp_store();
    let now = 1_000_000u64;
    let hash = put_and_reference(&ts.store, "image/jpeg", &common::pseudo_content(1024, 1)).await;
    dereference(&ts.store, hash, now).await;

    // 延迟一小时是为了避开正在传输中的响应：URL 已发出、字节还没读完时
    // 内容被删会导致播放中断。所以「没到期不能删」是硬要求。
    let report = gc::run_once(&ts.store, now + 10, 100).await.expect("GC");
    assert_eq!(report.blobs_deleted, 0, "还没到期就删了");
    assert!(
        blob::meta(&ts.store, hash).await.expect("查询").is_some(),
        "未到期的内容应还在"
    );

    let report = gc::run_once(&ts.store, now + blob::GC_DELAY_SECS, 100)
        .await
        .expect("GC");
    assert_eq!(report.blobs_deleted, 1);
    assert!(
        blob::meta(&ts.store, hash).await.expect("查询").is_none(),
        "到期后元数据应删除"
    );
    assert!(
        ts.store
            .get(Cf::BlobBin, keys::content_key(hash))
            .await
            .expect("get")
            .is_none(),
        "内容值也应删除"
    );
}

#[tokio::test]
async fn 队列项被消费后不再残留() {
    let ts = common::temp_store();
    let now = 1_000_000u64;
    let hash = put_and_reference(&ts.store, "image/jpeg", &common::pseudo_content(1024, 2)).await;
    dereference(&ts.store, hash, now).await;

    gc::run_once(&ts.store, now + blob::GC_DELAY_SECS, 100)
        .await
        .expect("GC");

    let left = ts
        .store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Blob), usize::MAX)
        .await
        .expect("扫描");
    assert!(left.is_empty(), "处理完的队列项应删除，否则会反复处理");

    // 计数记录也该清掉，不然对账会把它当成孤儿计数。
    assert!(
        ts.store
            .get(Cf::BlobMeta, keys::blob_ref_key(hash))
            .await
            .expect("get")
            .is_none()
    );
}

#[tokio::test]
async fn 延迟窗口内复活则不删() {
    let ts = common::temp_store();
    let now = 1_000_000u64;
    let data = common::pseudo_content(2048, 3);
    let hash = put_and_reference(&ts.store, "image/jpeg", &data).await;
    dereference(&ts.store, hash, now).await;

    // ==== 二次确认的意义 ====
    // 排队与执行之间有一小时窗口，这期间 agent 完全可能重传相同内容。
    // 不确认就删会删掉正在被引用的内容，而且因为是内容寻址，
    // 症状会出现在一个看起来毫不相关的画布上。
    let again = blob::put_bytes(&ts.store, "image/jpeg", &data)
        .await
        .expect("重传");
    assert!(again.deduplicated);
    let mut batch = Batch::new();
    blob::stage_reference(&ts.store, &mut batch, hash)
        .await
        .expect("重新引用");
    ts.store.write(batch).await.expect("提交");

    let report = gc::run_once(&ts.store, now + blob::GC_DELAY_SECS, 100)
        .await
        .expect("GC");
    assert_eq!(report.blobs_deleted, 0, "复活的内容不该被删");
    assert_eq!(report.revived, 1);
    assert!(
        blob::meta(&ts.store, hash).await.expect("查询").is_some(),
        "复活的内容必须还在"
    );
    // 队列项要清掉，否则下一轮会再判一次。
    let left = ts
        .store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Blob), usize::MAX)
        .await
        .expect("扫描");
    assert!(left.is_empty());
}

#[tokio::test]
async fn 删分块blob会级联排队它的块() {
    let ts = common::temp_store();
    let now = 1_000_000u64;
    let data = common::pseudo_content(20 * 1024 * 1024, 4);
    let hash = put_and_reference(&ts.store, "video/mp4", &data).await;

    let meta = blob::meta(&ts.store, hash)
        .await
        .expect("元数据")
        .expect("应存在");
    let chunks = match &meta.layout {
        BlobLayout::Chunked { chunks } => chunks.clone(),
        other => panic!("应为 Chunked，实际 {other:?}"),
    };
    assert_eq!(chunks.len(), 5);

    dereference(&ts.store, hash, now).await;

    // 第一轮：删 blob，并把归零的块排进 chunk 队列（块有自己的延迟）。
    let report = gc::run_once(&ts.store, now + blob::GC_DELAY_SECS, 100)
        .await
        .expect("GC");
    assert_eq!(report.blobs_deleted, 1);
    assert_eq!(report.chunks_enqueued, 5, "5 个块都应排队");
    assert_eq!(report.chunks_deleted, 0, "块还没到期");
    // 块内容此刻仍在——这是有意的，正在传输的响应可能还在读它们。
    for ch in &chunks {
        assert!(
            ts.store
                .get(Cf::Chunk, keys::content_key(*ch))
                .await
                .expect("get")
                .is_some(),
            "块应等到自己的延迟到期才删"
        );
    }

    // 第二轮：块到期，真正删除。
    let report = gc::run_once(&ts.store, now + 2 * blob::GC_DELAY_SECS, 100)
        .await
        .expect("GC");
    assert_eq!(report.chunks_deleted, 5);
    for ch in &chunks {
        assert!(
            ts.store
                .get(Cf::Chunk, keys::content_key(*ch))
                .await
                .expect("get")
                .is_none(),
            "到期后块应删除"
        );
        assert!(
            ts.store
                .get(Cf::ChunkMeta, keys::chunk_ref_key(*ch))
                .await
                .expect("get")
                .is_none(),
            "块的计数记录也应删除"
        );
    }
}

#[tokio::test]
async fn 共享块不会因为删一个文件而消失() {
    let ts = common::temp_store();
    let now = 1_000_000u64;

    // 两个文件共享前 2 块。删掉 A 之后，共享块的计数应该还是 1，不能被回收——
    // 否则 B 会变成「元数据在、内容没了」的损坏状态。
    let shared = common::pseudo_content(8 * 1024 * 1024, 5);
    let mut a = shared.clone();
    a.extend_from_slice(&common::pseudo_content(4 * 1024 * 1024, 20));
    let mut b = shared.clone();
    b.extend_from_slice(&common::pseudo_content(4 * 1024 * 1024, 30));

    let ha = put_and_reference(&ts.store, "video/mp4", &a).await;
    let hb = put_and_reference(&ts.store, "video/mp4", &b).await;

    let meta_b = blob::meta(&ts.store, hb)
        .await
        .expect("元数据")
        .expect("应存在");
    let chunks_b = match &meta_b.layout {
        BlobLayout::Chunked { chunks } => chunks.clone(),
        other => panic!("应为 Chunked，实际 {other:?}"),
    };

    dereference(&ts.store, ha, now).await;
    gc::run_once(&ts.store, now + blob::GC_DELAY_SECS, 100)
        .await
        .expect("第一轮");
    gc::run_once(&ts.store, now + 2 * blob::GC_DELAY_SECS, 100)
        .await
        .expect("第二轮");

    // B 必须完好：全部块都还在，内容能完整读回。
    for ch in &chunks_b {
        assert!(
            ts.store
                .get(Cf::Chunk, keys::content_key(*ch))
                .await
                .expect("get")
                .is_some(),
            "B 引用的块不该被删"
        );
    }
    let back = blob::read_all(&ts.store, hb)
        .await
        .expect("读")
        .expect("应存在");
    assert_eq!(
        blake3::hash(&back).as_bytes(),
        blake3::hash(&b).as_bytes(),
        "删掉 A 之后 B 的内容必须完好"
    );
}

#[tokio::test]
async fn 空队列跑gc不报错() {
    let ts = common::temp_store();
    let report = gc::run_once(&ts.store, 1_000_000, 100).await.expect("GC");
    assert!(report.is_empty());
}

#[tokio::test]
async fn 单轮处理量可限制() {
    let ts = common::temp_store();
    let now = 1_000_000u64;

    // 一次删几十万项会把后台任务卡住太久，所以要能分批。
    for i in 0..10u64 {
        let hash =
            put_and_reference(&ts.store, "image/jpeg", &common::pseudo_content(512, i + 1)).await;
        dereference(&ts.store, hash, now).await;
    }

    let report = gc::run_once(&ts.store, now + blob::GC_DELAY_SECS, 3)
        .await
        .expect("GC");
    assert_eq!(report.blobs_deleted, 3, "单轮上限应生效");

    let left = ts
        .store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Blob), usize::MAX)
        .await
        .expect("扫描");
    assert_eq!(left.len(), 7, "剩下的项留给下一轮");
}

#[tokio::test]
async fn 删除后compaction把空间还回去() {
    let ts = common::temp_store();
    let now = 1_000_000u64;

    let data = common::pseudo_content(20 * 1024 * 1024, 66);
    let hash = put_and_reference(&ts.store, "video/mp4", &data).await;
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");
    let before = ts.disk_usage();
    assert!(before > 15 << 20, "写入后应占用 20MB 左右，实际 {before}");

    dereference(&ts.store, hash, now).await;
    gc::run_once(&ts.store, now + blob::GC_DELAY_SECS, 100)
        .await
        .expect("第一轮");
    gc::run_once(&ts.store, now + 2 * blob::GC_DELAY_SECS, 100)
        .await
        .expect("第二轮");

    // RocksDB 的删除只是写 tombstone，空间回收是异步的。
    // 所以删完大量数据后必须手动触发一次 compaction，否则磁盘不会真的空出来。
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");
    let after = ts.disk_usage();

    assert!(
        after < before / 2,
        "删除并 compact 后占用应明显下降：{before} → {after}"
    );
}
