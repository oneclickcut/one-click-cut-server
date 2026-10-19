//! S8 压测：大量小文件写入。
//!
//! 这是全案最核心的负载：两万画布 × 350 文件 = 近千万个小文件。裸文件存会遇到
//! 千万 inode 问题，RocksDB 把它们合成几千个 SST——这个测试量化那个收益。
//!
//! **默认跑一个小规模**（够验证正确性与量级）。要跑大规模压测：
//!
//! ```bash
//! OCC_BENCH_FILES=100000 cargo test --test bench_small -- --nocapture --ignored
//! ```

#[path = "../common/mod.rs"]
mod common;

use std::time::Instant;

use one_click_cut_server::storage::{Cf, blob};

/// 从环境变量读规模，默认 5000。
fn bench_files() -> usize {
    std::env::var("OCC_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000)
}

#[tokio::test]
async fn 小文件写入吞吐与占用() {
    let ts = common::temp_store();
    let n = bench_files();
    // 200KB 是一个真实素材的平均大小（总览文档第 9 节）。这里用 4KB
    // 让默认规模跑得快；大规模压测时靠 OCC_BENCH_FILES 调数量而不是大小。
    let size = 4096;

    let started = Instant::now();
    for i in 0..n {
        let data = common::pseudo_content(size, i as u64);
        blob::put_bytes(&ts.store, "image/jpeg", &data)
            .await
            .expect("写入");
    }
    let write_elapsed = started.elapsed();

    ts.store.flush().await.expect("flush");
    let flush_elapsed = started.elapsed();
    ts.store.compact_all().await.expect("compact");
    let total_elapsed = started.elapsed();

    let raw = (n * size) as u64;
    let used = ts.disk_usage();
    let sst = count_ext(ts.store.path(), "sst");
    let blob_files = count_ext(ts.store.path(), "blob");

    println!("=== {n} 个 {size} 字节小文件 ===");
    println!(
        "  写入 {:.2}s（{:.0} 文件/秒），+flush {:.2}s，+compact {:.2}s",
        write_elapsed.as_secs_f64(),
        n as f64 / write_elapsed.as_secs_f64(),
        flush_elapsed.as_secs_f64(),
        total_elapsed.as_secs_f64()
    );
    println!(
        "  原始 {raw} 字节 → 落盘 {used} 字节（{:.2}×）",
        used as f64 / raw as f64
    );
    println!("  物理文件：{sst} 个 SST + {blob_files} 个 blob");

    // ==== 这是「不用裸文件存」这条决策的量化收益 ====
    // n 个逻辑文件在磁盘上表现为个位数到几十个物理文件。裸文件存的话
    // 就是 n 个 inode。
    let physical = sst + blob_files;
    assert!(
        physical < n / 10,
        "{n} 个逻辑文件应合并成远少于 {} 个物理文件，实际 {physical}",
        n / 10
    );

    let entries = ts
        .store
        .scan_prefix_keys(Cf::BlobBin, Vec::new(), usize::MAX)
        .await
        .expect("扫描")
        .len();
    assert_eq!(entries, n, "条目数应等于文件数");
}

#[tokio::test]
async fn 元数据点查延迟() {
    let ts = common::temp_store();
    let n = 20_000usize;

    // 先写一批元数据，再测点查——`meta` 列族开了 point lookup 与 bloom filter，
    // 这个数字决定了「千万级元数据能否全量吃进 block cache」的判断是否成立。
    let canvas = one_click_cut_server::storage::keys::CanvasId::new();
    let mut batch = one_click_cut_server::storage::Batch::new();
    let mut paths = Vec::with_capacity(n);
    for i in 0..n {
        let path = format!("scene{:03}/f{:05}.jpg", i / 100, i);
        let entry = one_click_cut_server::storage::records::FileEntry {
            v: 1,
            hash: one_click_cut_server::storage::keys::ContentHash::from_bytes(
                [(i % 251) as u8; 32],
            ),
            size: 204_800,
            updated_at: 1_789_727_178,
        };
        batch.put(
            Cf::Meta,
            one_click_cut_server::storage::keys::file_key(canvas, &path).expect("键"),
            one_click_cut_server::storage::records::encode(&entry).expect("编码"),
        );
        paths.push(path);
    }
    ts.store.write(batch).await.expect("提交");
    ts.store.flush().await.expect("flush");
    ts.store.compact_all().await.expect("compact");

    // 点查 1000 次，取平均。
    let sample = 1000;
    let started = Instant::now();
    for i in 0..sample {
        let path = &paths[(i * 17) % n];
        let got = ts
            .store
            .get(
                Cf::Meta,
                one_click_cut_server::storage::keys::file_key(canvas, path).expect("键"),
            )
            .await
            .expect("get");
        assert!(got.is_some());
    }
    let elapsed = started.elapsed();

    println!("=== {n} 条元数据的点查 ===");
    println!(
        "  {sample} 次点查 {:.3}s，平均 {:.1} µs/次",
        elapsed.as_secs_f64(),
        elapsed.as_micros() as f64 / sample as f64
    );

    // 一次点查应当在微秒量级。到毫秒说明 bloom filter 或 block cache 没起作用。
    let per_query_us = elapsed.as_micros() as f64 / sample as f64;
    assert!(
        per_query_us < 500.0,
        "平均点查 {per_query_us:.1} µs 过慢，检查 point lookup 与 bloom filter 配置"
    );

    // 一个画布的全部条目一次前缀扫描拿到——manifest 就靠这个。
    let started = Instant::now();
    let rows = ts
        .store
        .scan_prefix_keys(
            Cf::Meta,
            one_click_cut_server::storage::keys::file_prefix(canvas),
            usize::MAX,
        )
        .await
        .expect("扫描");
    println!(
        "  前缀扫描 {} 条耗时 {:.3}s",
        rows.len(),
        started.elapsed().as_secs_f64()
    );
    assert_eq!(rows.len(), n);
}

/// 递归数某个扩展名的文件个数。
fn count_ext(dir: &std::path::Path, ext: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            n += count_ext(&p, ext);
        } else if p.extension().is_some_and(|x| x == ext) {
            n += 1;
        }
    }
    n
}
