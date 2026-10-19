//! S2 验收：用 `test/data/gen.sh` 生成的真实数据集量化写放大、去重与压缩收益。
//!
//! 与 `blob.rs` 里的合成内容测试是两回事：合成内容验证**逻辑正确**，
//! 这里验证**在真实数据构成下的空间表现**。计划文档 5.3 节的几个数字就是从
//! 这里来的。
//!
//! 数据集没生成时这些测试会跳过而不是失败——`gen.sh` 要造 120MB，
//! 不该强制每个人在跑单元测试前先等它。

#[path = "../common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};

use one_click_cut_server::storage::{Cf, blob};

fn dataset_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test/data/generated")
}

/// 数据集不存在时跳过，并说清怎么生成。
fn require_dataset() -> Option<PathBuf> {
    let dir = dataset_dir();
    if dir.join("big.mp4").exists() {
        Some(dir)
    } else {
        eprintln!("跳过：测试数据不存在，先跑 `bash test/data/gen.sh`");
        None
    }
}

/// 列出某个子目录下的文件，按名字排序保证可复现。
fn list_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn 真实成片的写入与读回() {
    let Some(dir) = require_dataset() else { return };
    let ts = common::temp_store();

    let data = std::fs::read(dir.join("big.mp4")).expect("读 big.mp4");
    assert_eq!(data.len(), 31_457_280, "big.mp4 应是 30MiB");

    let out = blob::put_bytes(&ts.store, "video/mp4", &data)
        .await
        .expect("写入");
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    let used = ts.disk_usage();
    let raw = data.len() as u64;
    println!(
        "30MB 成片：内容 {raw} 字节，落盘 {used} 字节（{:.2}×）",
        used as f64 / raw as f64
    );
    // 计划 5.3：落盘增量应在 30~35MB。超出就说明值分离没生效或内容被重复搬运。
    assert!(used < raw + (5 << 20), "落盘 {used} 超过 35MB，写放大异常");

    // 读回必须逐字节一致：这一条同时验证切块顺序与末块长度。
    let back = blob::read_all(&ts.store, out.hash)
        .await
        .expect("读")
        .expect("应存在");
    assert_eq!(
        blake3::hash(&back).as_bytes(),
        blake3::hash(&data).as_bytes(),
        "读回内容应与原文件完全一致"
    );
}

#[tokio::test]
async fn 副本命中去重不增加占用() {
    let Some(dir) = require_dataset() else { return };
    let ts = common::temp_store();

    let big = std::fs::read(dir.join("big.mp4")).expect("读 big.mp4");
    let dup = std::fs::read(dir.join("dup.mp4")).expect("读 dup.mp4");
    assert_eq!(big, dup, "dup.mp4 应是 big.mp4 的副本");

    blob::put_bytes(&ts.store, "video/mp4", &big)
        .await
        .expect("写 big");
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");
    let after_one = ts.disk_usage();

    // agent 反复重传相同素材是常态，这是内容寻址去重最直接的收益。
    let out = blob::put_bytes(&ts.store, "video/mp4", &dup)
        .await
        .expect("写 dup");
    assert!(out.deduplicated, "副本应命中去重");
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");
    let after_two = ts.disk_usage();

    println!("单份 {after_one} 字节 → 加副本后 {after_two} 字节");
    assert!(
        after_two < after_one + (1 << 20),
        "写入副本后占用从 {after_one} 涨到 {after_two}，去重没生效"
    );
}

#[tokio::test]
async fn 文本样本的zstd收益() {
    let Some(dir) = require_dataset() else { return };
    let ts = common::temp_store();

    let files = list_files(&dir.join("text"));
    assert_eq!(files.len(), 50, "应有 50 个文本样本");

    let mut raw_total = 0u64;
    for f in &files {
        let data = std::fs::read(f).expect("读文本");
        raw_total += data.len() as u64;
        blob::put_bytes(&ts.store, "application/json", &data)
            .await
            .expect("写入");
    }
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    let used = ts.disk_usage();
    println!(
        "50 个文本：原始 {raw_total} 字节，落盘 {used} 字节（压缩比 {:.2}×）",
        raw_total as f64 / used.max(1) as f64
    );
    // 计划 5.3：50 个 20KB 文本（约 1.4MB）落盘应明显小于 1MB。
    assert!(
        used < raw_total / 2,
        "原始 {raw_total} 字节落盘 {used} 字节，zstd 似乎没生效"
    );
}

#[tokio::test]
async fn 三百个图片素材的写入() {
    let Some(dir) = require_dataset() else { return };
    let ts = common::temp_store();

    // 一个真实画布约 300 个 200KB 素材。这里验证的是「大量小文件」这个
    // 全案最核心的场景：裸文件存会遇到千万 inode 问题，RocksDB 把它们
    // 合成几千个 SST，问题消失。
    let files = list_files(&dir.join("small"));
    assert_eq!(files.len(), 300, "应有 300 个图片素材");

    let started = std::time::Instant::now();
    let mut raw_total = 0u64;
    for f in &files {
        let data = std::fs::read(f).expect("读素材");
        raw_total += data.len() as u64;
        blob::put_bytes(&ts.store, "image/jpeg", &data)
            .await
            .expect("写入");
    }
    let elapsed = started.elapsed();
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    let used = ts.disk_usage();
    let entries = ts
        .store
        .scan_prefix_keys(Cf::BlobBin, Vec::new(), usize::MAX)
        .await
        .expect("扫描")
        .len();
    println!(
        "300 个素材：原始 {raw_total} 字节，落盘 {used} 字节，耗时 {:?}（{:.0} 文件/秒）",
        elapsed,
        300.0 / elapsed.as_secs_f64()
    );
    assert_eq!(entries, 300, "应有 300 条内容");
    // 随机字节不可压缩，所以落盘应接近原始大小；明显超出就是写放大。
    assert!(
        used < raw_total + (10 << 20),
        "落盘 {used} 远超原始 {raw_total}，写放大异常"
    );
}

#[tokio::test]
async fn 完整画布的混合负载() {
    let Some(dir) = require_dataset() else { return };
    let ts = common::temp_store();

    // 按总览文档第 9 节的典型画布构成跑一遍：300 图 + 50 文本 + 1 成片。
    // 文档估算单画布约 150MB，这里的数据集约 90MB（成片只有一个）。
    let mut raw_total = 0u64;
    for f in list_files(&dir.join("small")) {
        let data = std::fs::read(&f).expect("读");
        raw_total += data.len() as u64;
        blob::put_bytes(&ts.store, "image/jpeg", &data)
            .await
            .expect("写");
    }
    for f in list_files(&dir.join("text")) {
        let data = std::fs::read(&f).expect("读");
        raw_total += data.len() as u64;
        blob::put_bytes(&ts.store, "application/json", &data)
            .await
            .expect("写");
    }
    let big = std::fs::read(dir.join("big.mp4")).expect("读");
    raw_total += big.len() as u64;
    blob::put_bytes(&ts.store, "video/mp4", &big)
        .await
        .expect("写");

    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    let used = ts.disk_usage();
    let blob_files = std::fs::read_dir(ts.store.path())
        .expect("读目录")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "blob"))
        .count();
    let sst_files = std::fs::read_dir(ts.store.path())
        .expect("读目录")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "sst"))
        .count();

    println!(
        "完整画布：原始 {raw_total} 字节 → 落盘 {used} 字节；{sst_files} 个 SST + {blob_files} 个 blob 文件"
    );
    // 351 个文件在磁盘上表现为个位数的文件——这就是「不用裸文件存」换到的东西。
    assert!(
        sst_files + blob_files < 50,
        "351 个逻辑文件应合并成很少的物理文件，实际 {sst_files} SST + {blob_files} blob"
    );
    assert!(used < raw_total + (10 << 20), "落盘 {used} 超出预期");
}
