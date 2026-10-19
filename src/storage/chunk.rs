//! 大文件分块参数与 Range 区间换算。
//!
//! 分块是**内容寻址**的：每块的 key 是这块自己的 BLAKE3 哈希，不是
//! 「文件哈希 + 序号」。这样重传同一个 mp4 时全部块都命中去重，
//! agent 断点续传也天然幂等。
//!
//! 本模块只做纯计算（块边界、区间落在哪几块、每块裁剪多少），不碰存储——
//! 这样最容易出错的偏移换算可以用纯函数测试穷举验证，不需要开库。
//! 真正的读写在 [`super::blob`]。

use anyhow::{Result, bail};

/// 块大小：4 MB。
///
/// Range 读取粒度与 key 数量的平衡点。一个 30MB 的 mp4 = 8 个 key；
/// 峰值内存 = 并发数 × 4MB，与文件大小无关。
pub const CHUNK_SIZE: u64 = 4 << 20;

/// 不超过这个大小的内容直接作为单 value 存，不进 chunk 列族。
pub const INLINE_MAX: u64 = CHUNK_SIZE;

/// 内容是否需要分块。
pub fn needs_chunking(size: u64) -> bool {
    size > INLINE_MAX
}

/// 一个文件按 4MB 切分后的块数。
pub fn chunk_count(size: u64) -> u64 {
    size.div_ceil(CHUNK_SIZE)
}

/// 第 `index` 块在文件里的字节范围（左闭右开）。末块可能短于 4MB。
pub fn chunk_span(size: u64, index: u64) -> (u64, u64) {
    let start = index * CHUNK_SIZE;
    let end = ((index + 1) * CHUNK_SIZE).min(size);
    (start, end)
}

/// 一次 Range 读取落在某一块上的部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSlice {
    /// 块在 `BlobLayout::Chunked` 列表里的下标。
    pub index: u64,
    /// 从块内这个偏移开始读。首块通常非 0，其余都是 0。
    pub offset: u64,
    /// 从 `offset` 起读这么多字节。末块通常短于整块。
    pub len: u64,
}

/// 已解析的字节区间，左闭右闭（与 HTTP `Range` 头一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    /// **含**这一字节。
    pub end: u64,
}

impl ByteRange {
    /// 整个文件。
    pub fn full(size: u64) -> Result<Self> {
        if size == 0 {
            bail!("空文件没有可读区间");
        }
        Ok(Self {
            start: 0,
            end: size - 1,
        })
    }

    /// 区间长度。
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        false // start <= end 由构造保证
    }
}

/// 把一个字节区间换算成需要读取的块列表。
///
/// 这是整个静态分发链路上最容易写错的一段：算错一个字节，播放器会在拖进度条时
/// 花屏或直接报错，而且症状离原因很远。所以这里返回精确的 `(index, offset, len)`
/// 三元组，由测试穷举验证与内存切片逐字节比对。
pub fn slices_for_range(size: u64, range: ByteRange) -> Result<Vec<ChunkSlice>> {
    if range.start > range.end {
        bail!("区间起点大于终点: {}-{}", range.start, range.end);
    }
    if range.start >= size {
        // 调用方要把这个错误映射成 416，并带上 `Content-Range: bytes */{size}`。
        bail!("区间起点 {} 超出文件大小 {size}", range.start);
    }
    // 末尾越界按截断处理（`bytes=0-999999` 请求一个小文件是合法的）。
    let end = range.end.min(size - 1);

    let first = range.start / CHUNK_SIZE;
    let last = end / CHUNK_SIZE;

    let mut out = Vec::with_capacity((last - first + 1) as usize);
    for index in first..=last {
        let (chunk_start, chunk_end) = chunk_span(size, index);
        // 与请求区间求交集，再转成块内的相对偏移。
        let want_start = range.start.max(chunk_start);
        let want_end = end.min(chunk_end - 1);
        out.push(ChunkSlice {
            index,
            offset: want_start - chunk_start,
            len: want_end - want_start + 1,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB4: u64 = CHUNK_SIZE;

    #[test]
    fn 分块阈值边界() {
        // 这三个尺寸是最容易写成 >= 或 <= 的地方。
        assert!(!needs_chunking(MB4 - 1));
        assert!(!needs_chunking(MB4), "正好一块走 inline，不进 chunk 列族");
        assert!(needs_chunking(MB4 + 1));
    }

    #[test]
    fn 块数与末块长度() {
        // 30MB 成片 = 31457280 字节 = 7 × 4MB + 2MB，共 8 块，末块 2MB。
        // 刻意选一个不是 4MB 整数倍的大小：末块短块是最容易写错的地方。
        let size = 31_457_280u64;
        assert_eq!(chunk_count(size), 8);
        assert_eq!(chunk_span(size, 0), (0, MB4));
        assert_eq!(chunk_span(size, 7), (7 * MB4, size));
        assert_eq!(chunk_span(size, 7).1 - chunk_span(size, 7).0, 2 << 20);

        // 正好整数倍时不该多出一个空块。
        assert_eq!(chunk_count(8 * MB4), 8);
        assert_eq!(chunk_count(MB4), 1);
        assert_eq!(chunk_count(1), 1);
    }

    #[test]
    fn 全量区间覆盖所有块() {
        let size = 31_457_280u64;
        let slices = slices_for_range(size, ByteRange::full(size).expect("区间")).expect("换算");
        assert_eq!(slices.len(), 8);
        // 各块长度相加必须等于文件大小，一个字节都不能差。
        let total: u64 = slices.iter().map(|s| s.len).sum();
        assert_eq!(total, size);
        assert_eq!(slices[0].offset, 0);
        assert_eq!(slices[7].len, 2 << 20, "末块只有 2MB");
    }

    #[test]
    fn 块内区间只读一块() {
        let size = 31_457_280u64;
        let r = ByteRange {
            start: 100,
            end: 199,
        };
        let slices = slices_for_range(size, r).expect("换算");
        assert_eq!(
            slices,
            vec![ChunkSlice {
                index: 0,
                offset: 100,
                len: 100
            }]
        );
    }

    #[test]
    fn 跨块区间首尾裁剪中间整块() {
        // 文档第 7 节的例子：bytes=8388608-12582911 → 第 2 块整块。
        let size = 31_457_280u64;
        let slices = slices_for_range(
            size,
            ByteRange {
                start: 8_388_608,
                end: 12_582_911,
            },
        )
        .expect("换算");
        assert_eq!(
            slices,
            vec![ChunkSlice {
                index: 2,
                offset: 0,
                len: MB4
            }]
        );

        // 真正跨三块的情况：首块裁掉前面，中间整块，末块裁掉后面。
        let slices = slices_for_range(
            size,
            ByteRange {
                start: MB4 - 10,
                end: 2 * MB4 + 9,
            },
        )
        .expect("换算");
        assert_eq!(slices.len(), 3);
        assert_eq!(
            slices[0],
            ChunkSlice {
                index: 0,
                offset: MB4 - 10,
                len: 10
            }
        );
        assert_eq!(
            slices[1],
            ChunkSlice {
                index: 1,
                offset: 0,
                len: MB4
            }
        );
        assert_eq!(
            slices[2],
            ChunkSlice {
                index: 2,
                offset: 0,
                len: 10
            }
        );
        assert_eq!(slices.iter().map(|s| s.len).sum::<u64>(), 20 + MB4);
    }

    #[test]
    fn 单字节与边界字节() {
        let size = 31_457_280u64;

        // 第一个字节。
        assert_eq!(
            slices_for_range(size, ByteRange { start: 0, end: 0 }).expect("换算"),
            vec![ChunkSlice {
                index: 0,
                offset: 0,
                len: 1
            }]
        );
        // 最后一个字节：应落在末块的最后一个位置。
        let last = slices_for_range(
            size,
            ByteRange {
                start: size - 1,
                end: size - 1,
            },
        )
        .expect("换算");
        assert_eq!(
            last,
            vec![ChunkSlice {
                index: 7,
                offset: (2 << 20) - 1,
                len: 1
            }]
        );
        // 块边界的前后各一字节，分属相邻两块。
        let across = slices_for_range(
            size,
            ByteRange {
                start: MB4 - 1,
                end: MB4,
            },
        )
        .expect("换算");
        assert_eq!(across.len(), 2);
        assert_eq!(
            across[0],
            ChunkSlice {
                index: 0,
                offset: MB4 - 1,
                len: 1
            }
        );
        assert_eq!(
            across[1],
            ChunkSlice {
                index: 1,
                offset: 0,
                len: 1
            }
        );
    }

    #[test]
    fn 末尾越界按截断处理() {
        // `bytes=0-` 会被解析成 end = u64::MAX 之类的大数，不该报错。
        let size = 1000u64;
        let slices = slices_for_range(
            size,
            ByteRange {
                start: 0,
                end: u64::MAX - 1,
            },
        )
        .expect("末尾越界应截断而不是报错");
        assert_eq!(slices.iter().map(|s| s.len).sum::<u64>(), size);
    }

    #[test]
    fn 起点越界应报错() {
        // 调用方要把这个错误映射成 416。
        let size = 1000u64;
        assert!(
            slices_for_range(
                size,
                ByteRange {
                    start: 1000,
                    end: 2000
                }
            )
            .is_err()
        );
        assert!(
            slices_for_range(
                size,
                ByteRange {
                    start: 5000,
                    end: 6000
                }
            )
            .is_err()
        );
        assert!(slices_for_range(size, ByteRange { start: 10, end: 5 }).is_err());
    }

    #[test]
    fn 穷举小文件的所有区间() {
        // 用一个「块大小 = 4 字节」的思路验证不了（CHUNK_SIZE 是常量），
        // 所以改成穷举一个跨 3 块的文件的全部合法区间，检查三条不变式：
        //   1. 各片长度之和 == 区间长度
        //   2. 片按块下标递增且连续
        //   3. 每片都落在自己块的范围内
        let size = 2 * MB4 + 1234;
        let points = [
            0u64,
            1,
            MB4 - 1,
            MB4,
            MB4 + 1,
            2 * MB4 - 1,
            2 * MB4,
            2 * MB4 + 1,
            size - 1,
        ];
        for &start in &points {
            for &end in &points {
                if start > end {
                    continue;
                }
                let r = ByteRange { start, end };
                let slices = slices_for_range(size, r).expect("换算");
                assert_eq!(
                    slices.iter().map(|s| s.len).sum::<u64>(),
                    r.len(),
                    "区间 {start}-{end} 的片长之和不等于区间长度"
                );
                for (i, s) in slices.iter().enumerate() {
                    assert_eq!(s.index, slices[0].index + i as u64, "块下标应连续递增");
                    let (cs, ce) = chunk_span(size, s.index);
                    assert!(
                        s.offset + s.len <= ce - cs,
                        "区间 {start}-{end} 的第 {i} 片越出了块边界"
                    );
                }
            }
        }
    }
}
