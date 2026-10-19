//! S0 存储冒烟测试：确认 RocksDB 能编译、能开库、能读写、重开后数据还在。
//!
//! 这个文件的价值是**把「rocksdb 依赖真的可用」这件事钉在 CI 里**。首次编译
//! librocksdb-sys 要几分钟且依赖 libclang，失败模式很多（找不到 libclang、
//! C++ 编译器版本、磁盘不够），越早在测试里暴露越好。
//!
//! S1 加了列族之后接口从 `get(key)` 变成 `get(cf, key)`，这里跟着改用 `Cf::Meta`。
//! 列族划分与选项、键编解码、merge operator 分别由 `store.rs` 与 `keys.rs` 覆盖。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::Cf;

#[tokio::test]
async fn 开库写读删一轮() {
    let ts = common::temp_store();

    // 读不存在的键返回 None，不是错误。
    let missing = ts
        .store
        .get(Cf::Meta, b"nope".to_vec())
        .await
        .expect("get 不该失败");
    assert!(missing.is_none(), "不存在的键应返回 None");

    ts.store
        .put(Cf::Meta, b"k1".to_vec(), b"v1".to_vec())
        .await
        .expect("put 失败");

    let got = ts
        .store
        .get(Cf::Meta, b"k1".to_vec())
        .await
        .expect("get 失败");
    assert_eq!(got.as_deref(), Some(&b"v1"[..]), "读回的值应与写入一致");

    ts.store
        .delete(Cf::Meta, b"k1".to_vec())
        .await
        .expect("delete 失败");
    let gone = ts
        .store
        .get(Cf::Meta, b"k1".to_vec())
        .await
        .expect("get 失败");
    assert!(gone.is_none(), "删除后应读不到");
}

#[tokio::test]
async fn 重开库后数据仍在() {
    let ts = common::temp_store();

    ts.store
        .put(Cf::Meta, b"persist".to_vec(), b"value".to_vec())
        .await
        .expect("put 失败");
    ts.store.flush().await.expect("flush 失败");

    // 关库再按同一路径打开。
    let ts = ts.reopen();

    let got = ts
        .store
        .get(Cf::Meta, b"persist".to_vec())
        .await
        .expect("get 失败");
    assert_eq!(
        got.as_deref(),
        Some(&b"value"[..]),
        "重开库后应能读到之前写入的值"
    );
}

#[tokio::test]
async fn 二进制键值不被截断() {
    let ts = common::temp_store();

    // 键空间用 0x00 作分隔符，所以必须确认含 0x00 的字节串不会被当成 C 字符串截断。
    let key = vec![b'f', 0x00, b'a', 0x00, b'b', b'.', b'j', b'p', b'g'];
    let value = vec![0x00, 0xff, 0x00, 0x01, 0x7f, 0x80];

    ts.store
        .put(Cf::Meta, key.clone(), value.clone())
        .await
        .expect("put 失败");

    let got = ts.store.get(Cf::Meta, key).await.expect("get 失败");
    assert_eq!(got.as_deref(), Some(&value[..]), "含 0x00 的键值应完整保留");
}

#[tokio::test]
async fn 大值可原样读回() {
    let ts = common::temp_store();

    // S2 会把超过 4MB 的内容切块，这里只验证「单个 4MB value 本身没问题」。
    let value: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

    ts.store
        .put(Cf::BlobBin, b"big".to_vec(), value.clone())
        .await
        .expect("put 失败");

    let got = ts
        .store
        .get(Cf::BlobBin, b"big".to_vec())
        .await
        .expect("get 失败");
    assert_eq!(got.as_deref(), Some(&value[..]), "4MB 值应原样读回");
}
