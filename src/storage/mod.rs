//! 第三层：存储引擎。不认识业务概念，只认识字节与键。
//!
//! 依赖方向：`http` → `domain` → `storage`，不许反向。

pub mod blob;
pub mod chunk;
pub mod gc;
pub mod keys;
pub mod kv;
pub mod records;

pub use kv::{Batch, Cf, Store, StoreOptions};
