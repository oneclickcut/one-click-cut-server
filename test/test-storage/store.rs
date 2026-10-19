//! S1 存储底座测试：列族齐全、per-CF 选项生效、WriteBatch 原子、计数 merge 正确。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::keys::{CanvasId, ContentHash, Uid};
use one_click_cut_server::storage::records::{self, FileEntry, Role, User};
use one_click_cut_server::storage::{Batch, Cf, keys};

// ---------------------------------------------------------------------------
// 列族
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 所有列族都可读写() {
    let ts = common::temp_store();

    // 列族少一个就会在开库时报错，但「建出来了却不可用」也要验。
    for cf in Cf::ALL {
        let key = format!("probe-{}", cf.name()).into_bytes();
        let value = cf.name().as_bytes().to_vec();
        ts.store
            .put(cf, key.clone(), value.clone())
            .await
            .unwrap_or_else(|e| panic!("{} put 失败: {e}", cf.name()));
        let got = ts
            .store
            .get(cf, key)
            .await
            .unwrap_or_else(|e| panic!("{} get 失败: {e}", cf.name()));
        assert_eq!(got.as_deref(), Some(&value[..]), "{} 读回应一致", cf.name());
    }
    assert_eq!(Cf::ALL.len(), 11);
}

#[tokio::test]
async fn 列族之间键空间互相隔离() {
    let ts = common::temp_store();

    // 同一个 key 写到不同列族应互不影响——这是「元数据的频繁小更新不会触发
    // 内容列族重写」的前提。
    ts.store
        .put(Cf::Meta, b"k".to_vec(), b"in-meta".to_vec())
        .await
        .expect("put");
    ts.store
        .put(Cf::Idx, b"k".to_vec(), b"in-idx".to_vec())
        .await
        .expect("put");

    assert_eq!(
        ts.store
            .get(Cf::Meta, b"k".to_vec())
            .await
            .expect("get")
            .as_deref(),
        Some(&b"in-meta"[..])
    );
    assert_eq!(
        ts.store
            .get(Cf::Idx, b"k".to_vec())
            .await
            .expect("get")
            .as_deref(),
        Some(&b"in-idx"[..])
    );
    assert!(
        ts.store
            .get(Cf::Chunk, b"k".to_vec())
            .await
            .expect("get")
            .is_none(),
        "没写过的列族应查不到"
    );
}

#[tokio::test]
async fn 重开库后数据与列族都在() {
    let ts = common::temp_store();

    ts.store
        .put(Cf::Meta, b"persist".to_vec(), b"v".to_vec())
        .await
        .expect("put");
    ts.store
        .add(Cf::ChunkMeta, b"cnt".to_vec(), 3)
        .await
        .expect("merge");
    ts.store.flush().await.expect("flush");

    let ts = ts.reopen();

    assert_eq!(
        ts.store
            .get(Cf::Meta, b"persist".to_vec())
            .await
            .expect("get")
            .as_deref(),
        Some(&b"v"[..])
    );
    // merge operator 必须在重开时重新注册，否则计数读出来会是原始操作数。
    assert_eq!(
        ts.store
            .counter(Cf::ChunkMeta, b"cnt".to_vec())
            .await
            .expect("counter"),
        3,
        "重开库后计数应保持，说明 merge operator 已重新注册"
    );
}

// ---------------------------------------------------------------------------
// WriteBatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 批次跨列族原子提交() {
    let ts = common::temp_store();
    let cid = CanvasId::new();
    let hash = ContentHash::from_bytes([7u8; 32]);

    // 这就是上传一个文件的真实形状：内容、元数据、计数、索引一次提交。
    let entry = FileEntry {
        v: 1,
        hash,
        size: 1234,
        updated_at: 1789727178,
    };
    let mut batch = Batch::new();
    batch
        .put(
            Cf::BlobBin,
            keys::content_key(hash),
            b"content-bytes".to_vec(),
        )
        .put(
            Cf::BlobMeta,
            keys::blob_meta_key(hash),
            records::encode(&records::BlobMeta::single(
                1234,
                records::ContentClass::Binary,
            ))
            .expect("编码"),
        )
        .add(Cf::BlobMeta, keys::blob_ref_key(hash), 1)
        .put(
            Cf::Meta,
            keys::file_key(cid, "scene01/f001.jpg").expect("键"),
            records::encode(&entry).expect("编码"),
        );
    assert_eq!(batch.len(), 4);

    ts.store.write(batch).await.expect("提交");

    // 四项应全部可见。
    assert!(
        ts.store
            .get(Cf::BlobBin, keys::content_key(hash))
            .await
            .expect("get")
            .is_some()
    );
    assert_eq!(
        ts.store
            .counter(Cf::BlobMeta, keys::blob_ref_key(hash))
            .await
            .expect("counter"),
        1
    );
    let raw = ts
        .store
        .get(
            Cf::Meta,
            keys::file_key(cid, "scene01/f001.jpg").expect("键"),
        )
        .await
        .expect("get")
        .expect("应存在");
    let back: FileEntry = records::decode(&raw).expect("解码");
    assert_eq!(back.hash, hash);
    assert_eq!(back.size, 1234);
}

#[tokio::test]
async fn 空批次是空操作() {
    let ts = common::temp_store();
    let batch = Batch::new();
    assert!(batch.is_empty());
    ts.store.write(batch).await.expect("空批次不该报错");
}

#[tokio::test]
async fn 批次内的删除与写入按顺序生效() {
    let ts = common::temp_store();
    ts.store
        .put(Cf::Meta, b"k".to_vec(), b"old".to_vec())
        .await
        .expect("put");

    let mut batch = Batch::new();
    batch
        .delete(Cf::Meta, b"k".to_vec())
        .put(Cf::Meta, b"k".to_vec(), b"new".to_vec());
    ts.store.write(batch).await.expect("提交");

    assert_eq!(
        ts.store
            .get(Cf::Meta, b"k".to_vec())
            .await
            .expect("get")
            .as_deref(),
        Some(&b"new"[..]),
        "同一批次里先删后写，最终应是写入的值"
    );
}

// ---------------------------------------------------------------------------
// 计数 merge operator
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 计数加减正确() {
    let ts = common::temp_store();
    let key = keys::chunk_ref_key(ContentHash::from_bytes([1u8; 32]));

    assert_eq!(
        ts.store
            .counter(Cf::ChunkMeta, key.clone())
            .await
            .expect("counter"),
        0,
        "没写过的计数应是 0"
    );

    for _ in 0..5 {
        ts.store
            .add(Cf::ChunkMeta, key.clone(), 1)
            .await
            .expect("加");
    }
    assert_eq!(
        ts.store
            .counter(Cf::ChunkMeta, key.clone())
            .await
            .expect("读"),
        5
    );

    for _ in 0..3 {
        ts.store
            .add(Cf::ChunkMeta, key.clone(), -1)
            .await
            .expect("减");
    }
    assert_eq!(
        ts.store
            .counter(Cf::ChunkMeta, key.clone())
            .await
            .expect("读"),
        2
    );
}

#[tokio::test]
async fn 计数减到零保留记录() {
    let ts = common::temp_store();
    let key = keys::chunk_ref_key(ContentHash::from_bytes([2u8; 32]));

    ts.store
        .add(Cf::ChunkMeta, key.clone(), 1)
        .await
        .expect("加");
    ts.store
        .add(Cf::ChunkMeta, key.clone(), -1)
        .await
        .expect("减");

    assert_eq!(
        ts.store
            .counter(Cf::ChunkMeta, key.clone())
            .await
            .expect("读"),
        0
    );
    // 记录保留而不是消失：GC 需要靠「计数存在且为 0」来判断内容可回收，
    // 也需要在删除前二次确认计数仍是 0。
    assert!(
        ts.store
            .get(Cf::ChunkMeta, key)
            .await
            .expect("get")
            .is_some(),
        "归零后记录应保留，GC 的二次确认依赖它"
    );
}

#[tokio::test]
async fn 计数不会变成负数() {
    let ts = common::temp_store();
    let key = keys::chunk_ref_key(ContentHash::from_bytes([3u8; 32]));

    // 多减一次只可能来自 bug。读取侧钳到 0 并记 error 日志，
    // 而不是把负数原样返回给 GC——否则 GC 的判断会彻底乱掉。
    ts.store
        .add(Cf::ChunkMeta, key.clone(), 1)
        .await
        .expect("加");
    ts.store
        .add(Cf::ChunkMeta, key.clone(), -1)
        .await
        .expect("减");
    ts.store
        .add(Cf::ChunkMeta, key.clone(), -1)
        .await
        .expect("多减一次");

    assert_eq!(
        ts.store.counter(Cf::ChunkMeta, key).await.expect("读"),
        0,
        "负计数应被钳到 0"
    );
}

#[tokio::test]
async fn 批次内多次加减合并为一次() {
    let ts = common::temp_store();
    let key = keys::blob_ref_key(ContentHash::from_bytes([4u8; 32]));

    // 覆盖写的真实形状：新内容 +1、旧内容 -1 在同一批次里。
    let mut batch = Batch::new();
    batch
        .add(Cf::BlobMeta, key.clone(), 1)
        .add(Cf::BlobMeta, key.clone(), 1)
        .add(Cf::BlobMeta, key.clone(), -1);
    ts.store.write(batch).await.expect("提交");

    assert_eq!(ts.store.counter(Cf::BlobMeta, key).await.expect("读"), 1);
}

#[tokio::test]
async fn 计数在flush与compaction后仍正确() {
    let ts = common::temp_store();
    let key = keys::chunk_ref_key(ContentHash::from_bytes([5u8; 32]));

    // 这条测试针对 associative merge operator 的经典陷阱：部分合并的输出会被
    // 当成新的操作数继续参与合并。如果值编码与操作数编码不一致，
    // flush / compaction 触发部分合并之后计数就会静默错掉。
    for _ in 0..10 {
        ts.store
            .add(Cf::ChunkMeta, key.clone(), 1)
            .await
            .expect("加");
    }
    ts.store.flush().await.expect("flush");
    for _ in 0..4 {
        ts.store
            .add(Cf::ChunkMeta, key.clone(), -1)
            .await
            .expect("减");
    }
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    assert_eq!(
        ts.store.counter(Cf::ChunkMeta, key).await.expect("读"),
        6,
        "跨 flush 与 compaction 的部分合并不应改变结果"
    );
}

#[tokio::test]
async fn 并发加减结果正确() {
    let ts = common::temp_store();
    let key = keys::chunk_ref_key(ContentHash::from_bytes([6u8; 32]));

    // merge operator 的意义就在这里：读-改-写会丢更新，merge 不会。
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let store = ts.store.clone();
        let k = key.clone();
        tasks.push(tokio::spawn(async move {
            store.add(Cf::ChunkMeta, k, 1).await.expect("加");
        }));
    }
    for t in tasks {
        t.await.expect("任务完成");
    }

    assert_eq!(
        ts.store.counter(Cf::ChunkMeta, key).await.expect("读"),
        50,
        "50 个并发 +1 必须正好得到 50，丢更新就说明用了读-改-写"
    );
}

// ---------------------------------------------------------------------------
// 前缀扫描
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 前缀扫描按字典序返回且不越界() {
    let ts = common::temp_store();
    let a = CanvasId::new();
    let b = CanvasId::new();

    // 故意乱序写入，扫描应按路径字典序返回——manifest 直接用这个顺序。
    for path in [
        "timeline.json",
        "scene02/f001.jpg",
        "final.mp4",
        "scene01/f001.jpg",
    ] {
        ts.store
            .put(
                Cf::Meta,
                keys::file_key(a, path).expect("键"),
                b"x".to_vec(),
            )
            .await
            .expect("put");
    }
    // 隔壁画布也写点东西，验证不会被扫进来。
    ts.store
        .put(
            Cf::Meta,
            keys::file_key(b, "other.jpg").expect("键"),
            b"y".to_vec(),
        )
        .await
        .expect("put");

    let rows = ts
        .store
        .scan_prefix(Cf::Meta, keys::file_prefix(a))
        .await
        .expect("扫描");
    let paths: Vec<String> = rows
        .iter()
        .map(|(k, _)| keys::parse_file_key(k).expect("解析").1)
        .collect();

    assert_eq!(
        paths,
        vec![
            "final.mp4",
            "scene01/f001.jpg",
            "scene02/f001.jpg",
            "timeline.json"
        ],
        "应按路径字典序返回，且只含画布 A 的文件"
    );
}

#[tokio::test]
async fn 前缀扫描可限量() {
    let ts = common::temp_store();
    let cid = CanvasId::new();
    for i in 0..50 {
        ts.store
            .put(
                Cf::Meta,
                keys::file_key(cid, &format!("f{i:03}.jpg")).expect("键"),
                b"x".to_vec(),
            )
            .await
            .expect("put");
    }

    let rows = ts
        .store
        .scan_prefix_limit(Cf::Meta, keys::file_prefix(cid), 10)
        .await
        .expect("扫描");
    assert_eq!(rows.len(), 10, "limit 应生效");

    let keys_only = ts
        .store
        .scan_prefix_keys(Cf::Meta, keys::file_prefix(cid), usize::MAX)
        .await
        .expect("扫描");
    assert_eq!(keys_only.len(), 50);
}

#[tokio::test]
async fn 空前缀扫描返回空而不是报错() {
    let ts = common::temp_store();
    let rows = ts
        .store
        .scan_prefix(Cf::Meta, keys::file_prefix(CanvasId::new()))
        .await
        .expect("扫描");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn 子节点前缀扫描只返回本层() {
    let ts = common::temp_store();
    let uid = Uid::new();
    let parent = keys::FolderId::new();
    let other = keys::FolderId::new();

    for _ in 0..3 {
        let child = keys::FolderId::new();
        ts.store
            .put(
                Cf::Idx,
                keys::child_key(
                    uid,
                    keys::ParentId::Folder(parent),
                    keys::ChildKind::Folder,
                    &child.to_text(),
                )
                .expect("键"),
                Vec::new(),
            )
            .await
            .expect("put");
    }
    // 另一个父节点下的子节点，以及根层的子节点，都不该被扫到。
    ts.store
        .put(
            Cf::Idx,
            keys::child_key(
                uid,
                keys::ParentId::Folder(other),
                keys::ChildKind::Project,
                &keys::ProjectId::new().to_text(),
            )
            .expect("键"),
            Vec::new(),
        )
        .await
        .expect("put");
    ts.store
        .put(
            Cf::Idx,
            keys::child_key(
                uid,
                keys::ParentId::Root,
                keys::ChildKind::Project,
                &keys::ProjectId::new().to_text(),
            )
            .expect("键"),
            Vec::new(),
        )
        .await
        .expect("put");

    let rows = ts
        .store
        .scan_prefix_keys(
            Cf::Idx,
            keys::child_prefix(uid, keys::ParentId::Folder(parent)),
            usize::MAX,
        )
        .await
        .expect("扫描");
    assert_eq!(rows.len(), 3, "只应返回该父节点下的 3 个子节点");
}

// ---------------------------------------------------------------------------
// 记录编解码在真实存储上的往返
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 用户记录往返() {
    let ts = common::temp_store();
    let uid = Uid::new();

    let user = User {
        v: 1,
        username: "alice".into(),
        pwd_hash: "$argon2id$v=19$m=19456,t=2,p=1$salt$hash".into(),
        role: Role::Admin,
        created_at: 1789727178,
        bytes_used: 0,
        quota_bytes: 0,
        disabled: false,
    };
    ts.store
        .put(
            Cf::Meta,
            keys::user_key(uid),
            records::encode(&user).expect("编码"),
        )
        .await
        .expect("put");

    let raw = ts
        .store
        .get(Cf::Meta, keys::user_key(uid))
        .await
        .expect("get")
        .expect("应存在");
    let back: User = records::decode(&raw).expect("解码");
    assert_eq!(back.username, "alice");
    assert!(back.role.is_admin());
    assert!(!back.disabled);
}

// ---------------------------------------------------------------------------
// 选项与运行状态
// ---------------------------------------------------------------------------

#[tokio::test]
async fn statistics可读取() {
    let ts = common::temp_store();
    ts.store
        .put(Cf::Meta, b"k".to_vec(), b"v".to_vec())
        .await
        .expect("put");
    ts.store.flush().await.expect("flush");

    // S2 要靠这个数字验证 BlobDB 没有在 compaction 里搬内容。
    let stats = ts.store.statistics();
    assert!(stats.is_some(), "statistics 应已开启");
    assert!(
        stats.as_deref().is_some_and(|s| !s.is_empty()),
        "statistics 文本不该为空"
    );
}

#[tokio::test]
async fn 估算键数可读取() {
    let ts = common::temp_store();
    for i in 0..100 {
        ts.store
            .put(Cf::Meta, format!("k{i:03}").into_bytes(), b"v".to_vec())
            .await
            .expect("put");
    }
    ts.store.flush().await.expect("flush");

    let n = ts.store.estimated_keys(Cf::Meta).await.expect("估算");
    // 估算值不精确，只要不是 0 就说明接上了。
    assert!(n > 0, "估算键数应大于 0，实际 {n}");
}

#[tokio::test]
async fn 内容列族确实把值分离到blob文件() {
    let ts = common::temp_store();

    // ==== 这是 S1 最关键的一条验收 ====
    //
    // BlobDB 值分离是「全部内容入 KV」方案能否长期运行的前提：没开的话
    // 每次 compaction 都要把 3TB 的图片视频重新读写一遍，写放大会成倍
    // 吃掉 SSD 寿命。选项写了不代表生效，所以这里用「磁盘上是否真的出现
    // .blob 文件」来实证，而不是回读选项值。
    let value: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();
    for i in 0..4u8 {
        let mut h = [0u8; 32];
        h[0] = i;
        ts.store
            .put(
                Cf::Chunk,
                keys::content_key(ContentHash::from_bytes(h)),
                value.clone(),
            )
            .await
            .expect("put");
    }
    ts.store.flush().await.expect("flush");

    let blob_files = count_files_with_ext(ts.data_dir(), "blob");
    assert!(
        blob_files > 0,
        "内容列族应把大值写进独立的 .blob 文件，实际一个都没有——\
         说明 enable_blob_files 没生效"
    );
}

#[tokio::test]
async fn 元数据列族不产生blob文件() {
    let ts = common::temp_store();

    // 反向验证：元数据列族不该开值分离。元数据 value 很小，走 blob 文件
    // 只会多一次寻址，点查反而变慢。
    for i in 0..200 {
        ts.store
            .put(Cf::Meta, format!("k{i:04}").into_bytes(), vec![7u8; 128])
            .await
            .expect("put");
    }
    ts.store.flush().await.expect("flush");

    assert_eq!(
        count_files_with_ext(ts.data_dir(), "blob"),
        0,
        "元数据列族不该产生 .blob 文件"
    );
}

/// 递归数某个扩展名的文件个数。
fn count_files_with_ext(dir: &std::path::Path, ext: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            n += count_files_with_ext(&p, ext);
        } else if p.extension().is_some_and(|x| x == ext) {
            n += 1;
        }
    }
    n
}

#[tokio::test]
async fn 文本内容列族的压缩确实生效() {
    let ts = common::temp_store();

    // blob_txt 开 zstd、blob_bin 不压缩。用高度可压缩的同一份内容分别写入，
    // 落盘大小应有明显差距——这是「按可压缩性分列族」这条设计的实证。
    // 每份 2MB，远超 min_blob_size(4KB)，所以会走 blob 文件并应用 blob 压缩。
    let text: Vec<u8> = "repeatable text ".repeat(128 * 1024).into_bytes();
    assert_eq!(text.len(), 2 << 20, "样本大小应为 2MB");

    for i in 0..8u8 {
        let mut h = [0u8; 32];
        h[0] = i;
        let hash = ContentHash::from_bytes(h);
        ts.store
            .put(Cf::BlobTxt, keys::content_key(hash), text.clone())
            .await
            .expect("put");
    }
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    let used = ts.disk_usage();
    let raw = (text.len() * 8) as u64;
    assert!(
        used < raw / 2,
        "8 份高度可压缩文本共 {raw} 字节，落盘 {used} 字节，zstd 似乎没生效"
    );
}

#[tokio::test]
async fn 游标分页定位缺失键并限制前缀范围() {
    let ts = common::temp_store();
    for key in [b"a0", b"p1", b"p3", b"p5", b"q0"] {
        ts.store
            .put(Cf::Meta, key.to_vec(), key.to_vec())
            .await
            .unwrap();
    }
    for (descending, after, expected) in [
        (false, None, vec!["p1", "p3"]),
        (true, None, vec!["p5", "p3"]),
        (false, Some("p3"), vec!["p5"]),
        (true, Some("p3"), vec!["p1"]),
        (false, Some("p2"), vec!["p3", "p5"]),
        (true, Some("p2"), vec!["p1"]),
        (false, Some("p5"), vec![]),
        (true, Some("p1"), vec![]),
    ] {
        let rows = ts
            .store
            .scan_prefix_page(
                Cf::Meta,
                b"p".to_vec(),
                after.map(|s| s.as_bytes().to_vec()),
                descending,
                2,
            )
            .await
            .unwrap();
        let actual: Vec<_> = rows
            .iter()
            .map(|(k, v)| {
                assert_eq!(k, v);
                std::str::from_utf8(k).unwrap()
            })
            .collect();
        assert_eq!(actual, expected, "descending={descending}, after={after:?}");
    }
    // 前缀上界本身恰好存在时，也不得混入相邻范围。
    ts.store.put(Cf::Meta, b"q".to_vec(), vec![]).await.unwrap();
    let rows = ts
        .store
        .scan_prefix_page(Cf::Meta, b"p".to_vec(), None, true, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].0, b"p5");
    for descending in [false, true] {
        assert!(
            ts.store
                .scan_prefix_page(Cf::Meta, b"o".to_vec(), None, descending, 2)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        ts.store
            .scan_prefix_page(Cf::Meta, b"p".to_vec(), None, false, 0)
            .await
            .is_err()
    );
    assert!(
        ts.store
            .scan_prefix_page(Cf::Meta, b"p".to_vec(), Some(b"q0".to_vec()), false, 2)
            .await
            .is_err()
    );
    // 无有限上界的前缀仍可倒序读取。
    for key in [vec![255, 1], vec![255, 2]] {
        ts.store.put(Cf::Meta, key, vec![]).await.unwrap();
    }
    let rows = ts
        .store
        .scan_prefix_page(Cf::Meta, vec![255], None, true, 10)
        .await
        .unwrap();
    assert_eq!(
        rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec![vec![255, 2], vec![255, 1]]
    );
}
