//! S2 内容层测试：inline 边界、分块、去重、Range 精确性。
//!
//! Range 的正确性是这里最重要的一条：算错一个字节，播放器拖进度条会花屏或
//! 直接报错，而症状离原因很远。所以全部区间读取都与内存原始切片**逐字节比对**。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::blob;
use one_click_cut_server::storage::chunk::{ByteRange, CHUNK_SIZE};
use one_click_cut_server::storage::records::BlobLayout;
use one_click_cut_server::storage::{Cf, keys};

const MB4: u64 = CHUNK_SIZE;

// ---------------------------------------------------------------------------
// inline 与分块的边界
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inline边界三个尺寸() {
    let ts = common::temp_store();

    // 4MB-1 与 4MB 走 inline，4MB+1 走分块。这三个尺寸是最容易写成 >= 或 <= 的地方。
    for (size, want_chunked) in [(MB4 - 1, false), (MB4, false), (MB4 + 1, true)] {
        let data = common::pseudo_content(size as usize, size);
        let out = blob::put_bytes(&ts.store, "video/mp4", &data)
            .await
            .expect("写入");
        assert_eq!(out.size, size);

        let meta = blob::meta(&ts.store, out.hash)
            .await
            .expect("读元数据")
            .expect("应存在");
        assert_eq!(meta.is_chunked(), want_chunked, "{size} 字节的分块判断错了");
        assert_eq!(meta.size, size);
    }
}

#[tokio::test]
async fn 小文件按mime分流到不同列族() {
    let ts = common::temp_store();

    let text = br#"{"scene":1,"frames":[]}"#.to_vec();
    let out = blob::put_bytes(&ts.store, "application/json", &text)
        .await
        .expect("写入");
    assert!(
        ts.store
            .get(Cf::BlobTxt, keys::content_key(out.hash))
            .await
            .expect("get")
            .is_some(),
        "json 应落在 blob_txt（开 zstd）"
    );

    let bin = common::pseudo_content(1024, 7);
    let out = blob::put_bytes(&ts.store, "image/jpeg", &bin)
        .await
        .expect("写入");
    assert!(
        ts.store
            .get(Cf::BlobBin, keys::content_key(out.hash))
            .await
            .expect("get")
            .is_some(),
        "jpeg 应落在 blob_bin（不压缩）"
    );
    assert!(
        ts.store
            .get(Cf::BlobTxt, keys::content_key(out.hash))
            .await
            .expect("get")
            .is_none(),
        "同一份内容不该同时出现在两个列族"
    );
}

#[tokio::test]
async fn 末块短块的分块与读回() {
    let ts = common::temp_store();

    // 30MB 成片：31457280 = 7×4MB + 2MB，共 8 块，末块只有 2MB。
    let size = 31_457_280usize;
    let data = common::pseudo_content(size, 42);
    let out = blob::put_bytes(&ts.store, "video/mp4", &data)
        .await
        .expect("写入");

    let meta = blob::meta(&ts.store, out.hash)
        .await
        .expect("元数据")
        .expect("应存在");
    match &meta.layout {
        BlobLayout::Chunked { chunks } => {
            assert_eq!(chunks.len(), 8, "应切成 8 块");
        }
        other => panic!("应为 Chunked，实际 {other:?}"),
    }

    // 读回内容的哈希必须与原文件一致——这一条同时验证了切块顺序、
    // 末块长度、以及拼装逻辑。
    let back = blob::read_all(&ts.store, out.hash)
        .await
        .expect("读")
        .expect("应存在");
    assert_eq!(back.len(), size);
    assert_eq!(
        blake3::hash(&back).as_bytes(),
        blake3::hash(&data).as_bytes(),
        "读回内容的 BLAKE3 应与原文件一致"
    );
}

#[tokio::test]
async fn 空内容可写可读() {
    let ts = common::temp_store();
    let out = blob::put_bytes(&ts.store, "text/plain", &[])
        .await
        .expect("写入");
    assert_eq!(out.size, 0);
    let back = blob::read_all(&ts.store, out.hash)
        .await
        .expect("读")
        .expect("应存在");
    assert!(back.is_empty());
}

// ---------------------------------------------------------------------------
// 去重
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 相同内容写两次不重复占用() {
    let ts = common::temp_store();
    let data = common::pseudo_content(2 * 1024 * 1024, 3);

    let a = blob::put_bytes(&ts.store, "image/jpeg", &data)
        .await
        .expect("第一次");
    assert!(!a.deduplicated, "第一次不该是去重命中");

    let b = blob::put_bytes(&ts.store, "image/jpeg", &data)
        .await
        .expect("第二次");
    assert!(b.deduplicated, "第二次应命中去重");
    assert_eq!(a.hash, b.hash, "同内容必须同哈希");
}

#[tokio::test]
async fn 大文件重复写入不增加块数() {
    let ts = common::temp_store();
    let data = common::pseudo_content(20 * 1024 * 1024, 11);

    blob::put_bytes(&ts.store, "video/mp4", &data)
        .await
        .expect("第一次");
    let after_first = ts
        .store
        .scan_prefix_keys(Cf::Chunk, Vec::new(), usize::MAX)
        .await
        .expect("扫描")
        .len();
    assert_eq!(after_first, 5, "20MB 应切成 5 块");

    // agent 重传相同素材是常态，全部块都该命中去重。
    for _ in 0..4 {
        blob::put_bytes(&ts.store, "video/mp4", &data)
            .await
            .expect("重传");
    }
    let after_retries = ts
        .store
        .scan_prefix_keys(Cf::Chunk, Vec::new(), usize::MAX)
        .await
        .expect("扫描")
        .len();
    assert_eq!(
        after_retries, after_first,
        "重传 4 次后块数不该增加（内容寻址的去重）"
    );
}

#[tokio::test]
async fn 部分相同的两个文件共享公共块() {
    let ts = common::temp_store();

    // 内容寻址按块去重，所以两个文件只要有字节对齐的相同块就能共享。
    // 这是「按块哈希寻址」而不是「文件哈希 + 序号」的直接收益。
    let common_part = common::pseudo_content(8 * 1024 * 1024, 5);
    let mut a = common_part.clone();
    a.extend_from_slice(&common::pseudo_content(4 * 1024 * 1024, 20));
    let mut b = common_part.clone();
    b.extend_from_slice(&common::pseudo_content(4 * 1024 * 1024, 30));

    blob::put_bytes(&ts.store, "video/mp4", &a)
        .await
        .expect("写 A");
    blob::put_bytes(&ts.store, "video/mp4", &b)
        .await
        .expect("写 B");

    let total = ts
        .store
        .scan_prefix_keys(Cf::Chunk, Vec::new(), usize::MAX)
        .await
        .expect("扫描")
        .len();
    // A 3 块 + B 3 块 = 6，但前 2 块相同，所以只有 4 块。
    assert_eq!(total, 4, "共享的 2 个块应只存一份");
}

// ---------------------------------------------------------------------------
// Range 读取
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 分块文件的区间读取逐字节正确() {
    let ts = common::temp_store();
    let size = 31_457_280usize;
    let data = common::pseudo_content(size, 77);
    let out = blob::put_bytes(&ts.store, "video/mp4", &data)
        .await
        .expect("写入");

    // 覆盖：跨块、块内、首字节、末字节、单字节、块边界前后、整块对齐。
    let cases: &[(u64, u64, &str)] = &[
        (0, 0, "首字节"),
        (0, 99, "开头 100 字节"),
        (MB4 - 1, MB4, "跨块边界两字节"),
        (MB4, 2 * MB4 - 1, "正好第 1 块整块"),
        (8_388_608, 12_582_911, "文档里的例子：第 2 块整块"),
        (MB4 - 10, 2 * MB4 + 9, "跨三块"),
        (size as u64 - 1, size as u64 - 1, "末字节"),
        (size as u64 - 4096, size as u64 - 1, "末尾 4KB"),
        (7 * MB4, size as u64 - 1, "整个末块（2MB 短块）"),
        (0, size as u64 - 1, "全量"),
    ];

    for &(start, end, why) in cases {
        let got = blob::read_range(&ts.store, out.hash, ByteRange { start, end })
            .await
            .unwrap_or_else(|e| panic!("{why} ({start}-{end}) 读取失败: {e}"));
        let want = &data[start as usize..=end as usize];
        assert_eq!(got.len(), want.len(), "{why} ({start}-{end}) 长度不对");
        assert_eq!(got, want, "{why} ({start}-{end}) 内容与原始切片不一致");
    }
}

#[tokio::test]
async fn 单块文件的区间读取逐字节正确() {
    let ts = common::temp_store();
    let data = common::pseudo_content(100_000, 21);
    let out = blob::put_bytes(&ts.store, "image/jpeg", &data)
        .await
        .expect("写入");

    for (start, end) in [(0u64, 0u64), (0, 999), (50_000, 50_099), (99_999, 99_999)] {
        let got = blob::read_range(&ts.store, out.hash, ByteRange { start, end })
            .await
            .expect("读取");
        assert_eq!(got, &data[start as usize..=end as usize], "{start}-{end}");
    }
}

#[tokio::test]
async fn 区间末尾越界按截断处理() {
    let ts = common::temp_store();
    let data = common::pseudo_content(1000, 9);
    let out = blob::put_bytes(&ts.store, "text/plain", &data)
        .await
        .expect("写入");

    // `bytes=0-` 会被解析成一个很大的 end，不该报错，而是读到文件末尾。
    let got = blob::read_range(
        &ts.store,
        out.hash,
        ByteRange {
            start: 0,
            end: u64::MAX - 1,
        },
    )
    .await
    .expect("末尾越界应截断");
    assert_eq!(got, data);
}

#[tokio::test]
async fn 区间起点越界返回错误() {
    let ts = common::temp_store();
    let data = common::pseudo_content(1000, 9);
    let out = blob::put_bytes(&ts.store, "text/plain", &data)
        .await
        .expect("写入");

    // 调用方要把这个错误映射成 416 并带上 `Content-Range: bytes */1000`。
    assert!(
        blob::read_range(
            &ts.store,
            out.hash,
            ByteRange {
                start: 1000,
                end: 2000
            }
        )
        .await
        .is_err(),
        "起点等于文件大小应报错"
    );
    assert!(
        blob::read_range(
            &ts.store,
            out.hash,
            ByteRange {
                start: 5000,
                end: 6000
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn 读不存在的内容返回none或错误() {
    let ts = common::temp_store();
    let fake = keys::ContentHash::from_bytes([0xee; 32]);
    assert!(blob::meta(&ts.store, fake).await.expect("查询").is_none());
    assert!(
        blob::read_all(&ts.store, fake)
            .await
            .expect("查询")
            .is_none()
    );
    assert!(
        blob::read_range(&ts.store, fake, ByteRange { start: 0, end: 10 })
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// 引用计数
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 引用计数随引用增减() {
    let ts = common::temp_store();
    let data = common::pseudo_content(1024, 4);
    let out = blob::put_bytes(&ts.store, "image/jpeg", &data)
        .await
        .expect("写入");

    let ref_key = keys::blob_ref_key(out.hash);
    assert_eq!(
        ts.store
            .counter(Cf::BlobMeta, ref_key.clone())
            .await
            .expect("读"),
        0,
        "只写内容不加引用——引用是调用方的事"
    );

    // 三个 FileEntry 引用同一份内容。
    for _ in 0..3 {
        let mut batch = one_click_cut_server::storage::Batch::new();
        blob::stage_reference(&ts.store, &mut batch, out.hash)
            .await
            .expect("加引用");
        ts.store.write(batch).await.expect("提交");
    }
    assert_eq!(
        ts.store
            .counter(Cf::BlobMeta, ref_key.clone())
            .await
            .expect("读"),
        3
    );

    let mut batch = one_click_cut_server::storage::Batch::new();
    let queued = blob::stage_dereference(&ts.store, &mut batch, out.hash, 1000)
        .await
        .expect("减引用");
    ts.store.write(batch).await.expect("提交");
    assert!(!queued, "还有 2 个引用，不该排进待删队列");
    assert_eq!(
        ts.store.counter(Cf::BlobMeta, ref_key).await.expect("读"),
        2
    );
}

#[tokio::test]
async fn 分块内容的每个块都被计数() {
    let ts = common::temp_store();
    let data = common::pseudo_content(20 * 1024 * 1024, 13);
    let out = blob::put_bytes(&ts.store, "video/mp4", &data)
        .await
        .expect("写入");

    let mut batch = one_click_cut_server::storage::Batch::new();
    blob::stage_reference(&ts.store, &mut batch, out.hash)
        .await
        .expect("加引用");
    ts.store.write(batch).await.expect("提交");

    let meta = blob::meta(&ts.store, out.hash)
        .await
        .expect("元数据")
        .expect("应存在");
    match &meta.layout {
        BlobLayout::Chunked { chunks } => {
            assert_eq!(chunks.len(), 5);
            for ch in chunks {
                assert_eq!(
                    ts.store
                        .counter(Cf::ChunkMeta, keys::chunk_ref_key(*ch))
                        .await
                        .expect("读"),
                    1,
                    "每个块都该被计数，否则 GC 会误删仍在使用的块"
                );
            }
        }
        other => panic!("应为 Chunked，实际 {other:?}"),
    }
}

#[tokio::test]
async fn 引用归零时排进待删队列() {
    let ts = common::temp_store();
    let data = common::pseudo_content(1024, 6);
    let out = blob::put_bytes(&ts.store, "image/jpeg", &data)
        .await
        .expect("写入");

    let mut batch = one_click_cut_server::storage::Batch::new();
    blob::stage_reference(&ts.store, &mut batch, out.hash)
        .await
        .expect("加引用");
    ts.store.write(batch).await.expect("提交");

    let now = 1_000_000u64;
    let mut batch = one_click_cut_server::storage::Batch::new();
    let queued = blob::stage_dereference(&ts.store, &mut batch, out.hash, now)
        .await
        .expect("减引用");
    ts.store.write(batch).await.expect("提交");
    assert!(queued, "归零应排进待删队列");

    // 队列项的到期时间要正好是 now + 1 小时：早了会删掉正在传输的内容。
    let rows = ts
        .store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Blob), usize::MAX)
        .await
        .expect("扫描");
    assert_eq!(rows.len(), 1);
    let (_, delete_after, hash) = keys::parse_gc_key(&rows[0]).expect("解析");
    assert_eq!(delete_after, now + blob::GC_DELAY_SECS);
    assert_eq!(hash, out.hash);
}

// ---------------------------------------------------------------------------
// 写放大
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 大文件写入没有成倍写放大() {
    let ts = common::temp_store();

    // ==== 这是 S2 最关键的一条验收 ====
    //
    // 如果 BlobDB 值分离没生效，compaction 会把内容反复搬运，落盘量远大于
    // 内容本身。30MB 内容落盘控制在 35MB 以内说明只写了一遍。
    let data = common::pseudo_content(30 * 1024 * 1024, 88);
    blob::put_bytes(&ts.store, "video/mp4", &data)
        .await
        .expect("写入");
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    let used = ts.disk_usage();
    let raw = data.len() as u64;
    assert!(
        used < raw + (5 << 20),
        "30MB 内容落盘 {used} 字节，超过 35MB——值分离可能没生效"
    );
    assert!(
        used > raw / 2,
        "落盘 {used} 字节明显少于内容本身，内容可能没写进去"
    );
}

#[tokio::test]
async fn 重复写入同一文件不增加占用() {
    let ts = common::temp_store();
    let data = common::pseudo_content(20 * 1024 * 1024, 99);

    blob::put_bytes(&ts.store, "video/mp4", &data)
        .await
        .expect("首次");
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");
    let after_first = ts.disk_usage();

    for _ in 0..5 {
        blob::put_bytes(&ts.store, "video/mp4", &data)
            .await
            .expect("重传");
    }
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");
    let after_retries = ts.disk_usage();

    // 允许一点元数据抖动，但绝不该接近翻倍。
    assert!(
        after_retries < after_first + (1 << 20),
        "重复写 5 次后占用从 {after_first} 涨到 {after_retries}，去重没生效"
    );
}
