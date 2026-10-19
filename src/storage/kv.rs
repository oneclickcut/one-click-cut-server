//! RocksDB 句柄、列族与访问封装。
//!
//! 两条纪律（存储文档第 9 节）：
//!
//! - **所有阻塞调用都走 `spawn_blocking`**，否则会堵住 tokio 工作线程，
//!   表现为大量上传时整个服务莫名超时。
//! - **RocksDB API 只出现在本文件**，上层拿到的是已经异步化、按列族分好的方法。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, DB, DBCompressionType, Direction,
    IteratorMode, MergeOperands, Options, ReadOptions, WriteBatch,
};

/// 列族。
///
/// 分列族的关键收益是**不同数据的 compaction 互不干扰**：mp4 的大量写入不会拖慢
/// 元数据读取，元数据的频繁小更新也不会触发内容列族的重写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cf {
    /// 用户、文件夹、画布、文件条目。小 value，读多写少，全量可缓存。
    Meta,
    /// 二级索引（用户名→uid、父节点→子节点、slug→画布）。只存指针，前缀扫描。
    Idx,
    /// 文本类小文件内容。BlobDB + zstd。
    BlobTxt,
    /// 二进制小文件内容。BlobDB + 不压缩。
    BlobBin,
    /// 大文件的定长块。BlobDB + 不压缩。
    Chunk,
    /// 每个 blob 的不可变元数据 + 引用计数。
    BlobMeta,
    /// 每个 chunk 的引用计数。merge operator 原子加减。
    ChunkMeta,
    /// agent token。带过期时间；旧版 refresh 记录已停用。
    Auth,
    /// 分块上传的中间状态。短命，定期清理。
    Stage,
    /// 待删队列。顺序写、顺序删。
    Gc,
    /// 公共任务队列，领取后删除。
    TaskQueue,
}

impl Cf {
    /// 全部列族。开库时按这个清单建，少一个就会在启动时报错而不是运行时。
    pub const ALL: [Cf; 11] = [
        Cf::Meta,
        Cf::Idx,
        Cf::BlobTxt,
        Cf::BlobBin,
        Cf::Chunk,
        Cf::BlobMeta,
        Cf::ChunkMeta,
        Cf::Auth,
        Cf::Stage,
        Cf::Gc,
        Cf::TaskQueue,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Meta => "meta",
            Self::Idx => "idx",
            Self::BlobTxt => "blob_txt",
            Self::BlobBin => "blob_bin",
            Self::Chunk => "chunk",
            Self::BlobMeta => "blobmeta",
            Self::ChunkMeta => "chunkmeta",
            Self::Auth => "auth",
            Self::Stage => "stage",
            Self::Gc => "gc",
            Self::TaskQueue => "task_queue",
        }
    }

    /// 是否是存放内容字节的列族（要开 BlobDB 值分离）。
    fn is_content(self) -> bool {
        matches!(self, Self::BlobTxt | Self::BlobBin | Self::Chunk)
    }

    /// 是否几乎只做点查（要开 point lookup 优化与 bloom filter）。
    fn is_point_lookup(self) -> bool {
        matches!(self, Self::Meta | Self::BlobMeta | Self::ChunkMeta)
    }

    /// 是否需要计数 merge operator。
    fn needs_counter_merge(self) -> bool {
        matches!(self, Self::BlobMeta | Self::ChunkMeta)
    }
}

/// 存储调参。默认值取自存储文档第 9 节的「运行时调参起点」。
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// 共享 block cache 大小。元数据热点全部缓存；内容走 blob 文件不占 block cache。
    pub block_cache_bytes: usize,
    pub write_buffer_bytes: usize,
    pub max_write_buffer_number: i32,
    pub target_file_size_bytes: u64,
    /// compaction 与 blob GC 的并行度。0 表示按 CPU 核数。
    pub max_background_jobs: i32,
    /// BlobDB：值超过这个大小就写到独立的 `.blob` 文件。
    pub min_blob_bytes: u64,
    pub blob_file_bytes: u64,
    /// 只对最旧的这个比例的 blob 文件做 GC，控制后台负载。
    pub blob_gc_age_cutoff: f64,
    /// 是否开 statistics。有几个百分点的开销，生产可关。
    pub statistics: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            block_cache_bytes: 8 << 30,
            write_buffer_bytes: 128 << 20,
            max_write_buffer_number: 4,
            target_file_size_bytes: 128 << 20,
            max_background_jobs: 0,
            min_blob_bytes: 4 << 10,
            blob_file_bytes: 512 << 20,
            blob_gc_age_cutoff: 0.25,
            statistics: true,
        }
    }
}

impl StoreOptions {
    /// 测试用的小配置：缓存与 buffer 都压到最小，避免几十个并行测试各占一大块。
    pub fn small() -> Self {
        Self {
            block_cache_bytes: 16 << 20,
            write_buffer_bytes: 4 << 20,
            max_write_buffer_number: 2,
            target_file_size_bytes: 8 << 20,
            max_background_jobs: 2,
            blob_file_bytes: 16 << 20,
            ..Self::default()
        }
    }
}

/// 一批写操作。
///
/// 不直接暴露 `rocksdb::WriteBatch`：一是不让 RocksDB 类型漏到上层，
/// 二是这样批次可以跨 await 传递、可以在进 `spawn_blocking` 之前被检查与打日志。
#[derive(Default)]
pub struct Batch {
    ops: Vec<Op>,
}

enum Op {
    Put(Cf, Vec<u8>, Vec<u8>),
    Delete(Cf, Vec<u8>),
    Merge(Cf, Vec<u8>, Vec<u8>),
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, cf: Cf, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(Op::Put(cf, key.into(), value.into()));
        self
    }

    pub fn delete(&mut self, cf: Cf, key: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(Op::Delete(cf, key.into()));
        self
    }

    /// 对计数键做原子加减。`delta` 可以为负。
    pub fn add(&mut self, cf: Cf, key: impl Into<Vec<u8>>, delta: i64) -> &mut Self {
        debug_assert!(
            cf.needs_counter_merge(),
            "{} 列族没有注册计数 merge operator",
            cf.name()
        );
        self.ops
            .push(Op::Merge(cf, key.into(), encode_counter_delta(delta)));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

/// 存储句柄。克隆代价是一次 `Arc` 引用计数，可随意放进 axum state。
#[derive(Clone)]
pub struct Store {
    db: Arc<DB>,
    path: PathBuf,
    /// 持有共享 cache，保证它活得比 DB 长。
    _cache: Cache,
    /// 所有 Store 克隆共享；仅在阻塞线程中持有，不占用异步运行时线程。
    queue_pop_lock: Arc<Mutex<()>>,
}

impl Store {
    /// 用默认调参打开数据库。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, &StoreOptions::default())
    }

    /// 打开（不存在则创建）数据库与全部列族。
    ///
    /// 这是阻塞调用，只在启动路径上用，所以不包 `spawn_blocking`。
    pub fn open_with(path: impl AsRef<Path>, opts: &StoreOptions) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)
            .with_context(|| format!("创建数据目录失败: {}", path.display()))?;

        let cache = Cache::new_lru_cache(opts.block_cache_bytes);
        let db_opts = db_options(opts);

        let cfs: Vec<ColumnFamilyDescriptor> = Cf::ALL
            .iter()
            .map(|cf| ColumnFamilyDescriptor::new(cf.name(), cf_options(*cf, opts, &cache)))
            .collect();

        let db = DB::open_cf_descriptors(&db_opts, &path, cfs)
            .with_context(|| format!("打开 RocksDB 失败: {}", path.display()))?;

        Ok(Self {
            db: Arc::new(db),
            path,
            _cache: cache,
            queue_pop_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 原子领取队列前 limit 条；整批解码成功、删除提交成功后才返回。
    ///
    /// 锁在 blocking 内取得并释放，因此调用方取消 future 也不会提前解锁。
    /// WriteBatch 自身只保证删除原子性，读到删除之间仍必须由这把锁保护。
    pub async fn pop_queue<T, F>(&self, limit: usize, decode: F) -> Result<Vec<T>>
    where
        T: Send + 'static,
        F: Fn(&[u8]) -> Result<T> + Send + 'static,
    {
        anyhow::ensure!((1..=100).contains(&limit), "领取数量必须为 1 到 100");
        let db = Arc::clone(&self.db);
        let lock = Arc::clone(&self.queue_pop_lock);
        blocking(move || {
            let _guard = lock
                .lock()
                .map_err(|_| anyhow::anyhow!("任务队列领取锁中毒"))?;
            let handle = cf_handle(&db, Cf::TaskQueue)?;
            let mut batch = WriteBatch::default();
            let mut tasks = Vec::with_capacity(limit);
            for row in db.iterator_cf(&handle, IteratorMode::Start).take(limit) {
                let (key, value) = row.context("读取任务队列失败")?;
                tasks.push(decode(&value)?);
                batch.delete_cf(&handle, key);
            }
            if !tasks.is_empty() {
                db.write(batch).context("删除已领取任务失败")?;
            }
            Ok(tasks)
        })
        .await
    }

    /// 遍历队列当前的一致视图，不删除任务。
    ///
    /// 不分页的接口必须返回全部结果；在阻塞线程里直接解码，避免先复制一份
    /// 原始键值列表。结果仍与队列大小成正比，调用方应避免高频轮询大队列。
    pub async fn list_queue<T, F>(&self, decode: F) -> Result<Vec<T>>
    where
        T: Send + 'static,
        F: Fn(&[u8]) -> Result<T> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        blocking(move || {
            let handle = cf_handle(&db, Cf::TaskQueue)?;
            let mut tasks = Vec::new();
            for row in db.iterator_cf(&handle, IteratorMode::Start) {
                let (_, value) = row.context("读取任务队列失败")?;
                tasks.push(decode(&value)?);
            }
            Ok(tasks)
        })
        .await
    }

    /// 点查。
    pub async fn get(&self, cf: Cf, key: impl Into<Vec<u8>>) -> Result<Option<Vec<u8>>> {
        let db = Arc::clone(&self.db);
        let key = key.into();
        blocking(move || {
            let handle = cf_handle(&db, cf)?;
            db.get_cf(&handle, &key)
                .with_context(|| format!("{} get 失败", cf.name()))
        })
        .await
    }

    /// 写入单个键。
    pub async fn put(
        &self,
        cf: Cf,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
    ) -> Result<()> {
        let db = Arc::clone(&self.db);
        let key = key.into();
        let value = value.into();
        blocking(move || {
            let handle = cf_handle(&db, cf)?;
            db.put_cf(&handle, &key, &value)
                .with_context(|| format!("{} put 失败", cf.name()))
        })
        .await
    }

    /// 删除单个键。RocksDB 的删除只是写 tombstone，空间回收是异步的。
    pub async fn delete(&self, cf: Cf, key: impl Into<Vec<u8>>) -> Result<()> {
        let db = Arc::clone(&self.db);
        let key = key.into();
        blocking(move || {
            let handle = cf_handle(&db, cf)?;
            db.delete_cf(&handle, &key)
                .with_context(|| format!("{} delete 失败", cf.name()))
        })
        .await
    }

    /// 对计数键做一次原子加减，返回前不保证已可见（RocksDB merge 是惰性的）。
    pub async fn add(&self, cf: Cf, key: impl Into<Vec<u8>>, delta: i64) -> Result<()> {
        let mut batch = Batch::new();
        batch.add(cf, key, delta);
        self.write(batch).await
    }

    /// 读一个计数键。
    ///
    /// 负数只可能来自 bug（减多于加）。这里**不静默当成 0**，而是记 error 日志后
    /// 返回 0：静默会让引用计数错乱一直藏着，直到某天内容被误删才暴露。
    pub async fn counter(&self, cf: Cf, key: impl Into<Vec<u8>>) -> Result<i64> {
        let key = key.into();
        let raw = self.get(cf, key.clone()).await?;
        let Some(bytes) = raw else {
            return Ok(0);
        };
        let n = decode_counter(&bytes).with_context(|| format!("{} 计数值损坏", cf.name()))?;
        if n < 0 {
            tracing::error!(
                cf = cf.name(),
                count = n,
                "引用计数为负，说明有一次减操作没有配对的加操作"
            );
            return Ok(0);
        }
        Ok(n)
    }

    /// 原子提交一批写操作。
    ///
    /// RocksDB 的 WriteBatch **要么全部可见要么全部不可见**，这是「不会出现
    /// FileEntry 指向不存在的内容」的根据。
    pub async fn write(&self, batch: Batch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let db = Arc::clone(&self.db);
        blocking(move || {
            let mut wb = WriteBatch::default();
            for op in &batch.ops {
                match op {
                    Op::Put(cf, k, v) => wb.put_cf(&cf_handle(&db, *cf)?, k, v),
                    Op::Delete(cf, k) => wb.delete_cf(&cf_handle(&db, *cf)?, k),
                    Op::Merge(cf, k, v) => wb.merge_cf(&cf_handle(&db, *cf)?, k, v),
                }
            }
            db.write(wb).context("WriteBatch 提交失败")
        })
        .await
    }

    /// 前缀扫描，返回全部匹配的键值对。
    ///
    /// **只用于结果集有界的场景**（一个画布的文件、一层目录的子节点）。无界扫描
    /// 用 [`Self::scan_prefix_limit`] 分页，否则一个几十万条的前缀会把内存打满。
    pub async fn scan_prefix(
        &self,
        cf: Cf,
        prefix: impl Into<Vec<u8>>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_prefix_limit(cf, prefix, usize::MAX).await
    }

    /// 前缀扫描，最多返回 `limit` 条。
    pub async fn scan_prefix_limit(
        &self,
        cf: Cf,
        prefix: impl Into<Vec<u8>>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let db = Arc::clone(&self.db);
        let prefix = prefix.into();
        blocking(move || {
            let handle = cf_handle(&db, cf)?;

            let mut ro = ReadOptions::default();
            // 给迭代器设上界，RocksDB 就能提前结束而不是一路扫到列族末尾。
            if let Some(upper) = prefix_upper_bound(&prefix) {
                ro.set_iterate_upper_bound(upper);
            }

            let iter =
                db.iterator_cf_opt(&handle, ro, IteratorMode::From(&prefix, Direction::Forward));

            let mut out = Vec::new();
            for item in iter {
                let (k, v) = item.with_context(|| format!("{} 扫描失败", cf.name()))?;
                // 上界是优化，不是保证（前缀全 0xff 时算不出上界），所以仍要判前缀。
                if !k.starts_with(&prefix) {
                    break;
                }
                out.push((k.to_vec(), v.to_vec()));
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    /// 按键顺序读取一页，严格跳过 after；游标键已删除也能继续。
    /// 每次调用使用独立的迭代器视图，不保证跨页快照一致性。
    pub async fn scan_prefix_page(
        &self,
        cf: Cf,
        prefix: Vec<u8>,
        after: Option<Vec<u8>>,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        anyhow::ensure!(limit > 0, "分页数量必须大于零");
        anyhow::ensure!(
            after.as_ref().is_none_or(|key| key.starts_with(&prefix)),
            "游标不属于扫描前缀"
        );
        let db = Arc::clone(&self.db);
        blocking(move || {
            let handle = cf_handle(&db, cf)?;
            let upper = prefix_upper_bound(&prefix);
            let mut ro = ReadOptions::default();
            ro.set_iterate_lower_bound(prefix.clone());
            // 反向扫描从上界定位后向前走，避免在上界之外 seek 的未定义行为。
            if !descending && let Some(upper) = &upper {
                ro.set_iterate_upper_bound(upper.clone());
            }
            let mut iter = db.raw_iterator_cf_opt(&handle, ro);
            if descending {
                if let Some(start) = after.as_ref().or(upper.as_ref()) {
                    iter.seek_for_prev(start);
                    if iter.key() == Some(start.as_slice()) {
                        iter.prev();
                    }
                } else {
                    iter.seek_to_last();
                }
            } else {
                iter.seek(after.as_ref().unwrap_or(&prefix));
                if after
                    .as_ref()
                    .is_some_and(|key| iter.key() == Some(key.as_slice()))
                {
                    iter.next();
                }
            }
            let mut out = Vec::new();
            while out.len() < limit {
                let Some((key, value)) = iter.item() else {
                    break;
                };
                if !key.starts_with(&prefix) {
                    break;
                }
                out.push((key.to_vec(), value.to_vec()));
                if descending {
                    iter.prev();
                } else {
                    iter.next();
                }
            }
            iter.status()
                .with_context(|| format!("{} 分页扫描失败", cf.name()))?;
            Ok(out)
        })
        .await
    }

    /// 只扫键，不读值。用于子节点索引（值是空的）与对账。
    pub async fn scan_prefix_keys(
        &self,
        cf: Cf,
        prefix: impl Into<Vec<u8>>,
        limit: usize,
    ) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .scan_prefix_limit(cf, prefix, limit)
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .collect())
    }

    /// 刷 memtable 到 SST。
    pub async fn flush(&self) -> Result<()> {
        let db = Arc::clone(&self.db);
        blocking(move || {
            for cf in Cf::ALL {
                db.flush_cf(&cf_handle(&db, cf)?)
                    .with_context(|| format!("{} flush 失败", cf.name()))?;
            }
            Ok(())
        })
        .await
    }

    /// 手动触发全量 compaction。
    ///
    /// 删除大量数据后要跑一次，让 blob GC 真正把空间还给文件系统——RocksDB 删除
    /// 只是写 tombstone，空间回收是异步的。
    pub async fn compact_all(&self) -> Result<()> {
        let db = Arc::clone(&self.db);
        blocking(move || {
            for cf in Cf::ALL {
                db.compact_range_cf(&cf_handle(&db, cf)?, None::<&[u8]>, None::<&[u8]>);
            }
            Ok(())
        })
        .await
    }

    /// 生成一致性快照到指定目录。
    ///
    /// # 这是单机单盘方案里唯一的防线
    ///
    /// 全部内容都在这一个目录里，所以备份不是可选项。checkpoint 相比 `cp -a`
    /// 有两个关键区别：
    ///
    /// - **原子**：不会拍到一个「元数据新、内容旧」的中间状态。RocksDB 会先把
    ///   MANIFEST 与 WAL 处理好，再对 SST 与 blob 文件建硬链接。
    /// - **不阻塞写入**：硬链接几乎不占额外空间，也不用停机。
    ///
    /// `target` 必须不存在——RocksDB 拒绝往已有目录里写 checkpoint，
    /// 这是它防止覆盖掉一份好备份的方式。
    pub async fn checkpoint(&self, target: impl AsRef<Path>) -> Result<()> {
        let db = Arc::clone(&self.db);
        let target = target.as_ref().to_path_buf();
        blocking(move || {
            if target.exists() {
                anyhow::bail!("快照目标已存在: {}", target.display());
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建备份父目录失败: {}", parent.display()))?;
            }
            let cp =
                rocksdb::checkpoint::Checkpoint::new(&db).context("创建 checkpoint 句柄失败")?;
            cp.create_checkpoint(&target)
                .with_context(|| format!("生成 checkpoint 失败: {}", target.display()))
        })
        .await
    }

    /// RocksDB statistics 文本。用来看 compaction 读写字节，验证 BlobDB 没在搬内容。
    pub fn statistics(&self) -> Option<String> {
        self.db.property_value("rocksdb.dbstats").ok().flatten()
    }

    /// 某个列族的估算键数量。admin 的统计接口用。
    pub async fn estimated_keys(&self, cf: Cf) -> Result<u64> {
        let db = Arc::clone(&self.db);
        blocking(move || {
            let handle = cf_handle(&db, cf)?;
            Ok(db
                .property_int_value_cf(&handle, "rocksdb.estimate-num-keys")
                .ok()
                .flatten()
                .unwrap_or(0))
        })
        .await
    }
}

/// 取列族句柄。
///
/// 列族在开库时全部建好、之后不再增删，所以用默认的 `SingleThreaded` 模式即可，
/// `cf_handle` 返回借用而不用过 `Arc`。取不到只可能是代码 bug（清单与实际不一致）。
fn cf_handle(db: &DB, cf: Cf) -> Result<&rocksdb::ColumnFamily> {
    db.cf_handle(cf.name())
        .with_context(|| format!("列族 {} 不存在", cf.name()))
}

/// 把一段阻塞的 RocksDB 操作丢到阻塞线程池。
///
/// 上层不要自己拼 `spawn_blocking`，统一走这里，方便以后加埋点与超时。
async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .context("存储线程池任务被取消或 panic")?
}

/// 数据库级选项。
fn db_options(opts: &StoreOptions) -> Options {
    let mut o = Options::default();
    o.create_if_missing(true);
    o.create_missing_column_families(true);

    let jobs = if opts.max_background_jobs > 0 {
        opts.max_background_jobs
    } else {
        available_cores()
    };
    o.set_max_background_jobs(jobs);

    // 避免大文件写入时一次性刷脏页造成延迟毛刺。
    o.set_bytes_per_sync(1 << 20);
    o.set_keep_log_file_num(8);

    if opts.statistics {
        o.enable_statistics();
    }
    o
}

/// 按列族给选项。这里是存储文档第 3 节与第 9 节的直接落地。
fn cf_options(cf: Cf, opts: &StoreOptions, cache: &Cache) -> Options {
    let mut o = Options::default();
    o.set_write_buffer_size(opts.write_buffer_bytes);
    o.set_max_write_buffer_number(opts.max_write_buffer_number);
    o.set_target_file_size_base(opts.target_file_size_bytes);

    if cf.is_content() {
        // ==== 这是「全部内容入 KV」方案里最关键的一条配置 ====
        //
        // 默认情况下 value 存在 LSM 的 SST 里，每次 compaction 都要把它重新读写
        // 一遍——3TB 的图片与视频被反复搬运，写放大会成倍吃掉 SSD 寿命。
        // BlobDB 把超过 min_blob_size 的值写到独立的 .blob 文件，
        // LSM 里只留 key 与一个指针，compaction 只搬指针。
        o.set_enable_blob_files(true);
        o.set_min_blob_size(opts.min_blob_bytes);
        o.set_blob_file_size(opts.blob_file_bytes);
        o.set_enable_blob_gc(true);
        o.set_blob_gc_age_cutoff(opts.blob_gc_age_cutoff);

        // LSM 部分只压 key 与指针，开销极低。
        o.set_compression_type(DBCompressionType::Lz4);

        // 内容的压缩按可压缩性区分：文本收益很高，已压缩的图片视频再压是纯 CPU 浪费。
        o.set_blob_compression_type(match cf {
            Cf::BlobTxt => DBCompressionType::Zstd,
            _ => DBCompressionType::None,
        });
    } else {
        o.set_compression_type(DBCompressionType::Lz4);
    }

    // 注意顺序：optimize_for_point_lookup 会整体替换 block based table options，
    // 所以必须先调它，再装我们自己的 table options，否则共享 cache 与 bloom 设置
    // 会被它悄悄覆盖掉。
    if cf.is_point_lookup() {
        o.optimize_for_point_lookup(0);
    }

    let mut table = BlockBasedOptions::default();
    // 共享一个 cache，而不是每个列族各分一块——热点集中在元数据上，
    // 固定切分会让元数据分不到它该拿的那部分。
    table.set_block_cache(cache);
    if cf.is_point_lookup() {
        table.set_bloom_filter(10.0, false);
        table.set_whole_key_filtering(true);
    }
    o.set_block_based_table_factory(&table);

    if cf.needs_counter_merge() {
        o.set_merge_operator_associative("counter_add", counter_merge);
    }

    o
}

fn available_cores() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(2)
}

/// 前缀扫描的上界：把前缀最后一个非 `0xff` 字节 +1。
///
/// 全 `0xff` 的前缀算不出上界，返回 `None`，此时靠调用方的前缀判断兜底。
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 计数 merge operator
// ---------------------------------------------------------------------------

/// 计数值与增量都编码成 8 字节小端 i64。
///
/// # 为什么值和操作数是同一种编码
///
/// 用的是 RocksDB 的 **associative** merge operator：同一个函数既做
/// 「已有值 + 一批操作数」的完整合并，也做「一批操作数之间」的部分合并。
/// 部分合并的输出会被当成新的操作数继续参与后续合并，所以**值类型必须与
/// 操作数类型一致**，否则部分合并的结果会被按另一种格式解释。
///
/// 如果值用 u32 计数、操作数用 i64 增量，就会在部分合并时把「计数」当成「增量」，
/// 得到静默错误的引用计数——这是本项目里最难查的一类 bug，所以统一成 i64 加法。
fn encode_counter_delta(delta: i64) -> Vec<u8> {
    delta.to_le_bytes().to_vec()
}

fn decode_counter(bytes: &[u8]) -> Result<i64> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("计数值应为 8 字节，实际 {}", bytes.len()))?;
    Ok(i64::from_le_bytes(arr))
}

/// 计数合并：已有值（没有则 0）加上全部增量。
///
/// **这里故意不做 0 的下钳位。** 部分合并时看不到已有值，如果把
/// `(+1, -2)` 钳成 0，后面再叠加已有值 1 就会得到 1 而不是正确的 0。
/// 下钳位放在读取侧（[`Store::counter`]）做，并且会记 error 日志。
fn counter_merge(
    _key: &[u8],
    existing: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let mut sum: i64 = match existing {
        Some(bytes) => match decode_counter(bytes) {
            Ok(n) => n,
            Err(e) => {
                // 返回 None 会让这个键的读取直接失败，比静默归零更容易被发现。
                tracing::error!(error = %e, "计数 merge 读到损坏的已有值");
                return None;
            }
        },
        None => 0,
    };

    for op in operands {
        match decode_counter(op) {
            Ok(d) => sum = sum.saturating_add(d),
            Err(e) => {
                tracing::error!(error = %e, "计数 merge 读到损坏的操作数");
                return None;
            }
        }
    }

    Some(sum.to_le_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 列族名单不重复且齐全() {
        let mut names: Vec<&str> = Cf::ALL.iter().map(|c| c.name()).collect();
        names.sort_unstable();
        let mut dedup = names.clone();
        dedup.dedup();
        assert_eq!(names, dedup, "列族名不能重复");
        assert_eq!(names.len(), 11, "原有 10 个列族加公共任务队列");
    }

    #[test]
    fn 计数编解码往返() {
        for n in [0i64, 1, -1, i64::MAX, i64::MIN, 12345] {
            let bytes = encode_counter_delta(n);
            assert_eq!(decode_counter(&bytes).expect("解码"), n);
        }
        assert!(decode_counter(&[1, 2, 3]).is_err(), "长度不对应报错");
    }

    #[test]
    fn 前缀上界计算() {
        assert_eq!(prefix_upper_bound(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(prefix_upper_bound(&[b'a', 0xff]), Some(b"b".to_vec()));
        // 全 0xff 没有上界，只能靠前缀判断兜底。
        assert_eq!(prefix_upper_bound(&[0xff, 0xff]), None);
        assert_eq!(prefix_upper_bound(b""), None);
    }

    #[test]
    fn 内容列族与点查列族划分互斥() {
        for cf in Cf::ALL {
            assert!(
                !(cf.is_content() && cf.is_point_lookup()),
                "{} 不该同时是内容列族与点查列族",
                cf.name()
            );
        }
        assert_eq!(
            Cf::ALL.iter().filter(|c| c.is_content()).count(),
            3,
            "blob_txt / blob_bin / chunk 三个内容列族"
        );
    }
}
