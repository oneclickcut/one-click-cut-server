//! `Range` 请求头的解析。
//!
//! 只支持单区间。多区间请求返回 `None` 让调用方回 416——主流播放器不用它，
//! 而支持它要引入 multipart/byteranges 响应体，复杂度与收益完全不匹配。
//!
//! 放成独立模块是因为这是纯字符串解析：可以穷举测试，不需要起服务、不需要开库。

use crate::storage::chunk::ByteRange;

/// 一次 `Range` 头的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// 没有 `Range` 头，返回完整内容（200）。
    None,
    /// 单区间，可满足。
    Single(ByteRange),
    /// 语法合法但无法满足（起点越界、多区间、末尾 N 字节超过文件大小等），
    /// 调用方返回 416 并带 `Content-Range: bytes */{size}`。
    Unsatisfiable,
    /// 语法不合法。按 RFC 9110，**不合法的 `Range` 头应当被忽略**，
    /// 当成没带这个头处理，返回 200 全量。
    Invalid,
}

/// 解析 `Range` 头。
///
/// `size` 是资源总大小，用来解析 `bytes=-500`（末尾 500 字节）与判断越界。
pub fn parse(header: Option<&str>, size: u64) -> RangeSpec {
    let Some(raw) = header else {
        return RangeSpec::None;
    };
    let raw = raw.trim();

    // 只认 bytes 单位。其他单位按 RFC 应当忽略。
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeSpec::Invalid;
    };
    let spec = spec.trim();
    if spec.is_empty() {
        return RangeSpec::Invalid;
    }

    // 多区间：语法合法但我们不支持，返回 416 而不是 Invalid——
    // 后者会静默返回全量，让客户端以为自己拿到了想要的那几段。
    if spec.contains(',') {
        return RangeSpec::Unsatisfiable;
    }

    let Some((start_raw, end_raw)) = spec.split_once('-') else {
        return RangeSpec::Invalid;
    };
    let start_raw = start_raw.trim();
    let end_raw = end_raw.trim();

    // 空文件对任何区间都无法满足。
    if size == 0 {
        return RangeSpec::Unsatisfiable;
    }

    match (start_raw.is_empty(), end_raw.is_empty()) {
        // `bytes=-N`：末尾 N 字节。
        (true, false) => {
            let Ok(n) = end_raw.parse::<u64>() else {
                return RangeSpec::Invalid;
            };
            if n == 0 {
                // `bytes=-0` 无意义，按 RFC 无法满足。
                return RangeSpec::Unsatisfiable;
            }
            // N 超过文件大小时取整个文件，这是 RFC 的规定而不是错误。
            let start = size.saturating_sub(n);
            RangeSpec::Single(ByteRange {
                start,
                end: size - 1,
            })
        }
        // `bytes=N-`：从 N 到末尾。
        (false, true) => {
            let Ok(start) = start_raw.parse::<u64>() else {
                return RangeSpec::Invalid;
            };
            if start >= size {
                return RangeSpec::Unsatisfiable;
            }
            RangeSpec::Single(ByteRange {
                start,
                end: size - 1,
            })
        }
        // `bytes=A-B`：闭区间。
        (false, false) => {
            let (Ok(start), Ok(end)) = (start_raw.parse::<u64>(), end_raw.parse::<u64>()) else {
                return RangeSpec::Invalid;
            };
            if start > end || start >= size {
                return RangeSpec::Unsatisfiable;
            }
            // 末尾越界按截断处理：`bytes=0-999999` 请求一个小文件是合法的。
            RangeSpec::Single(ByteRange {
                start,
                end: end.min(size - 1),
            })
        }
        // `bytes=-` 两边都空，不合法。
        (true, true) => RangeSpec::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u64 = 1000;

    fn single(start: u64, end: u64) -> RangeSpec {
        RangeSpec::Single(ByteRange { start, end })
    }

    #[test]
    fn 没有range头() {
        assert_eq!(parse(None, SIZE), RangeSpec::None);
    }

    #[test]
    fn 闭区间() {
        assert_eq!(parse(Some("bytes=0-99"), SIZE), single(0, 99));
        assert_eq!(parse(Some("bytes=100-199"), SIZE), single(100, 199));
        assert_eq!(parse(Some("bytes=0-0"), SIZE), single(0, 0), "单字节");
        assert_eq!(
            parse(Some("bytes=999-999"), SIZE),
            single(999, 999),
            "末字节"
        );
    }

    #[test]
    fn 开区间到末尾() {
        assert_eq!(parse(Some("bytes=0-"), SIZE), single(0, 999), "全量");
        assert_eq!(parse(Some("bytes=500-"), SIZE), single(500, 999));
        assert_eq!(parse(Some("bytes=999-"), SIZE), single(999, 999));
    }

    #[test]
    fn 末尾n字节() {
        assert_eq!(parse(Some("bytes=-500"), SIZE), single(500, 999));
        assert_eq!(parse(Some("bytes=-1"), SIZE), single(999, 999));
        // N 超过文件大小时取整个文件——这是 RFC 的规定，不是错误。
        assert_eq!(parse(Some("bytes=-5000"), SIZE), single(0, 999));
    }

    #[test]
    fn 末尾越界按截断处理() {
        // 播放器常发 `bytes=0-<很大的数>` 探测文件大小，不能报错。
        assert_eq!(parse(Some("bytes=0-999999"), SIZE), single(0, 999));
        assert_eq!(parse(Some("bytes=900-999999"), SIZE), single(900, 999));
    }

    #[test]
    fn 起点越界无法满足() {
        // 调用方要返回 416 并带 `Content-Range: bytes */1000`。
        assert_eq!(parse(Some("bytes=1000-"), SIZE), RangeSpec::Unsatisfiable);
        assert_eq!(
            parse(Some("bytes=1000-2000"), SIZE),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(
            parse(Some("bytes=5000-6000"), SIZE),
            RangeSpec::Unsatisfiable
        );
    }

    #[test]
    fn 起点大于终点无法满足() {
        assert_eq!(parse(Some("bytes=100-50"), SIZE), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn 多区间返回无法满足而不是忽略() {
        // 返回 Invalid 会静默给出全量内容，让客户端以为拿到了它要的那几段。
        // 416 才是诚实的答复。
        assert_eq!(
            parse(Some("bytes=0-99,200-299"), SIZE),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(parse(Some("bytes=0-,-100"), SIZE), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn 空文件无法满足任何区间() {
        assert_eq!(parse(Some("bytes=0-"), 0), RangeSpec::Unsatisfiable);
        assert_eq!(parse(Some("bytes=0-0"), 0), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn 语法不合法应被忽略() {
        // 按 RFC 9110，不合法的 Range 头当成没带处理，返回 200 全量。
        for h in [
            "",
            "items=0-99",
            "bytes",
            "bytes=",
            "bytes=-",
            "bytes=abc-def",
            "bytes=0-abc",
            "bytes=abc-",
            "0-99",
        ] {
            assert_eq!(
                parse(Some(h), SIZE),
                RangeSpec::Invalid,
                "{h:?} 应判为语法不合法"
            );
        }
    }

    #[test]
    fn 容忍空白() {
        assert_eq!(parse(Some(" bytes=0-99 "), SIZE), single(0, 99));
        assert_eq!(parse(Some("bytes= 0 - 99 "), SIZE), single(0, 99));
    }

    #[test]
    fn 零字节末尾请求无法满足() {
        assert_eq!(parse(Some("bytes=-0"), SIZE), RangeSpec::Unsatisfiable);
    }
}
