//! S1 键编解码测试。
//!
//! 键空间是这个项目最不该出错的地方：键格式一旦上线就很难改，而错误的键
//! 不会报错、只会让数据静默查不到。所以这里把每种键的往返、每条校验规则、
//! 以及前缀扫描的边界都钉死。

use one_click_cut_server::storage::keys::{self, *};

#[test]
fn 画廊索引排序与严格解码() {
    let first = ShareId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let second = ShareId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
    let key = gallery_feed_key(first);
    assert!(key.starts_with(&gallery_feed_prefix()));
    assert!(key < gallery_feed_key(second));
    assert_eq!(parse_gallery_feed_key(&key).unwrap(), first);
    assert!(parse_gallery_feed_key(&project_share_key(ProjectId::new())).is_err());
    for raw in [
        b"gfeed\0ZZZZZZZZZZZZZZZZZZZZZZZZZZ".as_slice(),
        b"gfeed\0short",
        b"",
        b"gfeed\x0001arz3ndektsv4rrffq69g5fav",
    ] {
        assert!(parse_gallery_feed_key(raw).is_err());
    }
}

// ---------------------------------------------------------------------------
// id
// ---------------------------------------------------------------------------

#[test]
fn id往返与长度固定() {
    let uid = Uid::new();
    let text = uid.to_text();
    assert_eq!(text.len(), ID_LEN, "ULID 文本必须是 {ID_LEN} 字符");
    assert_eq!(Uid::parse(&text).expect("应能解析"), uid);
}

#[test]
fn id按创建时间字典序递增() {
    // ULID 的前 10 字符是毫秒时间戳，所以字典序 = 创建时间序。
    // 前缀扫描依赖这个性质来「按创建时间返回」，不需要额外的时间索引。
    let mut texts: Vec<String> = Vec::new();
    for _ in 0..5 {
        texts.push(CanvasId::new().to_text());
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let mut sorted = texts.clone();
    sorted.sort();
    assert_eq!(texts, sorted, "先创建的 id 字典序应更小");
}

#[test]
fn id拒绝非法输入() {
    assert!(Uid::parse("").is_err(), "空串");
    assert!(Uid::parse("TOOSHORT").is_err(), "长度不足");
    assert!(Uid::parse(&"A".repeat(ID_LEN + 1)).is_err(), "长度超出");
    // Crockford base32 不含 I / L / O / U，这些字符应被拒。
    assert!(Uid::parse(&"I".repeat(ID_LEN)).is_err(), "非法字符 I");
}

#[test]
fn 不同类型的id不会混用() {
    // 这条测试其实是靠编译器保证的：下面这行如果取消注释就编译不过，
    // 因为 folder_key 的第二个参数是 FolderId 而不是 CanvasId。
    //
    //   let _ = keys::folder_key(Uid::new(), CanvasId::new());
    //
    // 留着注释是为了说明「为什么不用统一的 Id 类型」——把 uid 与 canvas_id
    // 传反了会写出一条永远查不到的 key，这种 bug 只能靠类型系统挡。
    let uid = Uid::new();
    let fid = FolderId::new();
    let cid = CanvasId::new();
    assert_ne!(keys::folder_key(uid, fid), keys::canvas_key(uid, cid));
}

// ---------------------------------------------------------------------------
// meta 列族
// ---------------------------------------------------------------------------

#[test]
fn 文件条目键往返() {
    let cid = CanvasId::new();
    for path in ["a.jpg", "scene01/f001.jpg", "a/b/c/d/e.srt", "中文名.md"] {
        let key = keys::file_key(cid, path).expect("应能构造");
        let (back_cid, back_path) = keys::parse_file_key(&key).expect("应能解析");
        assert_eq!(back_cid, cid, "canvas_id 应往返");
        assert_eq!(back_path, path, "path 应往返: {path}");
    }
}

#[test]
fn 文件条目键按路径字典序排列() {
    // manifest 需要按路径排序的条目列表，而前缀扫描天然按 key 字典序返回。
    // 只要 path 是 key 的末尾组件，就不需要在内存里再排一次。
    let cid = CanvasId::new();
    let paths = [
        "final.mp4",
        "scene01/f001.jpg",
        "scene01/f002.jpg",
        "scene02/f001.jpg",
        "timeline.json",
    ];
    let mut keys_built: Vec<Vec<u8>> = paths
        .iter()
        .map(|p| keys::file_key(cid, p).expect("构造"))
        .collect();
    let expected = keys_built.clone();
    keys_built.sort();
    assert_eq!(keys_built, expected, "路径字典序应等于 key 字典序");
}

#[test]
fn 文件前缀不会扫到隔壁画布() {
    // 最危险的场景：两个 canvas_id 字典序相邻。前缀必须带上分隔符，
    // 否则 `f|{A}` 会匹配到 `f|{AB...}`（虽然 ULID 定长使其不会发生，
    // 但依赖「定长」这个隐含假设是脆弱的）。
    let a = CanvasId::new();
    let b = CanvasId::new();
    assert_ne!(a, b);

    let prefix_a = keys::file_prefix(a);
    let key_b = keys::file_key(b, "x.jpg").expect("构造");
    assert!(
        !key_b.starts_with(&prefix_a),
        "画布 B 的文件键不该命中画布 A 的前缀"
    );

    let key_a = keys::file_key(a, "x.jpg").expect("构造");
    assert!(
        key_a.starts_with(&prefix_a),
        "画布 A 的文件键应命中自己的前缀"
    );
}

// ---------------------------------------------------------------------------
// 画布内目录键
// ---------------------------------------------------------------------------

#[test]
fn 目录键往返() {
    let cid = CanvasId::new();
    for path in ["scene01", "scene01/raw", "a/b/c", "中文目录/子目录"] {
        let key = keys::canvas_dir_key(cid, path).expect("应能构造");
        let (back_cid, back_path) = keys::parse_canvas_dir_key(&key).expect("应能解析");
        assert_eq!(back_cid, cid);
        assert_eq!(back_path, path, "目录路径应往返: {path}");
    }
}

#[test]
fn 目录键与文件键互不干扰() {
    // ==== 这条是整个键空间设计里最容易出错的地方 ====
    // 文件键是 `f` `0x00` …，目录键是 `f` `d` `0x00` …。第二个字节
    // （`0x00` vs `d`）已经把两者分开，所以两边的前缀扫描互相扫不到对方。
    let cid = CanvasId::new();
    let file_key = keys::file_key(cid, "scene01/f001.jpg").expect("构造");
    let dir_key = keys::canvas_dir_key(cid, "scene01").expect("构造");

    let file_prefix = keys::file_prefix(cid);
    let dir_prefix = keys::canvas_dir_prefix(cid);

    assert!(file_key.starts_with(&file_prefix));
    assert!(dir_key.starts_with(&dir_prefix));
    assert!(
        !dir_key.starts_with(&file_prefix),
        "目录记录不该被文件前缀扫到，否则 list_files 会返回目录"
    );
    assert!(
        !file_key.starts_with(&dir_prefix),
        "文件条目不该被目录前缀扫到"
    );

    // 两个全局前缀也不能互相包含。
    assert!(!keys::canvas_dir_prefix_all().starts_with(&[b'f', 0x00]));
    assert!(!file_prefix.starts_with(&keys::canvas_dir_prefix_all()));
}

#[test]
fn 子树前缀带尾斜杠才不会扫到同名前缀的兄弟() {
    // `scene01` 不带尾斜杠的话会把 `scene010/x.jpg` 一起扫进来。
    let cid = CanvasId::new();
    let under = keys::file_prefix_under(cid, "scene01").expect("构造");

    let inside = keys::file_key(cid, "scene01/f001.jpg").expect("构造");
    let sibling = keys::file_key(cid, "scene010/f001.jpg").expect("构造");
    let itself = keys::file_key(cid, "scene01").expect("构造");

    assert!(inside.starts_with(&under));
    assert!(!sibling.starts_with(&under), "scene010 不是 scene01 的子项");
    assert!(!itself.starts_with(&under), "同名文件不属于这个目录的子树");

    // 目录侧同理，而且 `dir_path` 自己那条记录不在子树前缀里。
    let dir_under = keys::canvas_dir_prefix_under(cid, "scene01").expect("构造");
    assert!(
        keys::canvas_dir_key(cid, "scene01/raw")
            .expect("构造")
            .starts_with(&dir_under)
    );
    assert!(
        !keys::canvas_dir_key(cid, "scene01")
            .expect("构造")
            .starts_with(&dir_under),
        "目录自己那条记录要单独 get，不在子树前缀里"
    );
}

#[test]
fn 目录路径段数上限比文件少一段() {
    let cid = CanvasId::new();
    // 15 段合法：剩一段留给文件名。
    let ok = (0..keys::MAX_DIR_SEGMENTS)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    keys::validate_dir_path(&ok).expect("15 段应合法");
    keys::canvas_dir_key(cid, &ok).expect("应能构造");

    // 16 段作为文件 path 合法，作为目录不合法。
    let deep = (0..keys::MAX_PATH_SEGMENTS)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    keys::validate_path(&deep).expect("16 段作为文件 path 合法");
    assert!(
        keys::validate_dir_path(&deep).is_err(),
        "16 段目录会建出一个永远放不进文件的目录"
    );
}

#[test]
fn 目录路径复用文件路径的校验规则() {
    // 共用一套规则是有意的：两者落在同一个字符串空间里，两套规则会让
    // 「文件 a/b 与目录 a/b 冲突」这类判断无从下手。
    for bad in ["", "/abs", "trailing/", "a//b", "a/./b", "a/../b", "a\\b"] {
        assert!(
            keys::validate_dir_path(bad).is_err(),
            "{bad:?} 不该是合法目录路径"
        );
    }
}

// ---------------------------------------------------------------------------
// idx 列族
// ---------------------------------------------------------------------------

#[test]
fn 子节点索引往返() {
    let uid = Uid::new();
    let parent = FolderId::new();
    let child = FolderId::new();

    let key = keys::child_key(
        uid,
        ParentId::Folder(parent),
        ChildKind::Folder,
        &child.to_text(),
    )
    .expect("构造");
    let (kind, id) = keys::parse_child_key(&key).expect("解析");
    assert_eq!(kind, ChildKind::Folder);
    assert_eq!(id, child.to_text());
}

#[test]
fn 根层子节点与具名父节点分属不同前缀() {
    let uid = Uid::new();
    let folder = FolderId::new();

    let root_prefix = keys::child_prefix(uid, ParentId::Root);
    let folder_prefix = keys::child_prefix(uid, ParentId::Folder(folder));
    assert_ne!(root_prefix, folder_prefix);

    let root_child = keys::child_key(
        uid,
        ParentId::Root,
        ChildKind::Project,
        &keys::ProjectId::new().to_text(),
    )
    .expect("构造");
    assert!(root_child.starts_with(&root_prefix));
    assert!(
        !root_child.starts_with(&folder_prefix),
        "根层的子节点不该出现在某个文件夹的列表里"
    );
}

#[test]
fn 根层排在所有具名父节点之前() {
    // ParentId::Root 编码成 26 个 '0'，而 '0' 是 Crockford base32 的最小字符，
    // 所以根层的索引永远排在最前面——扫描根层时不用跳过别的父节点。
    let uid = Uid::new();
    let root = keys::child_prefix(uid, ParentId::Root);
    let named = keys::child_prefix(uid, ParentId::Folder(FolderId::new()));
    assert!(root < named, "根层前缀应小于任何具名父节点前缀");
}

#[test]
fn 同一父节点下文件夹与项目可共存() {
    let uid = Uid::new();
    let parent = ParentId::Folder(FolderId::new());
    let prefix = keys::child_prefix(uid, parent);

    let f =
        keys::child_key(uid, parent, ChildKind::Folder, &FolderId::new().to_text()).expect("构造");
    let c = keys::child_key(
        uid,
        parent,
        ChildKind::Project,
        &keys::ProjectId::new().to_text(),
    )
    .expect("构造");

    assert!(
        f.starts_with(&prefix) && c.starts_with(&prefix),
        "两者同前缀"
    );
    assert_ne!(f, c);
    assert_eq!(
        keys::parse_child_key(&f).expect("解析").0,
        ChildKind::Folder
    );
    assert_eq!(
        keys::parse_child_key(&c).expect("解析").0,
        ChildKind::Project
    );
}

#[test]
fn slug键只含slug且定长() {
    let key = keys::canvas_slug_key("k7Qm3xB9pLdR").expect("构造");
    assert!(key.starts_with(b"cs\0"));
    assert!(key.ends_with(b"k7Qm3xB9pLdR"));
    // `cs` + 0x00 + 12 字符 slug。键里**没有**所有者信息——这是
    // `/f/{slug}/{path}` 不必暴露用户名或 uid 的前提。
    assert_eq!(key.len(), 3 + SLUG_LEN, "键应是定长 15 字节");

    // 非法 slug 在构造键这一层就被拦住，不会污染键空间。
    assert!(keys::canvas_slug_key("short").is_err());
    assert!(keys::canvas_slug_key("k7Qm3xB9pLd!").is_err());
}

// ---------------------------------------------------------------------------
// 内容相关列族
// ---------------------------------------------------------------------------

#[test]
fn 哈希键往返() {
    let h = ContentHash::from_bytes([0xab; HASH_LEN]);
    for key in [
        keys::blob_meta_key(h),
        keys::blob_ref_key(h),
        keys::chunk_ref_key(h),
    ] {
        assert_eq!(keys::parse_hash_key(&key).expect("解析"), h);
    }
}

#[test]
fn 含零字节的哈希也能正确往返() {
    // 哈希是原始 32 字节，必然会出现含 0x00 的情况。这时按分隔符切分就会切错，
    // 所以解析必须按偏移做。全 0 是最极端的情况。
    let h = ContentHash::from_bytes([0u8; HASH_LEN]);
    let key = keys::blob_meta_key(h);
    assert_eq!(keys::parse_hash_key(&key).expect("解析"), h);

    let mut mixed = [0u8; HASH_LEN];
    mixed[0] = 0x00;
    mixed[1] = 0xff;
    mixed[15] = 0x00;
    let h2 = ContentHash::from_bytes(mixed);
    assert_eq!(
        keys::parse_hash_key(&keys::blob_meta_key(h2)).expect("解析"),
        h2
    );
}

#[test]
fn 三种内容元数据前缀互不重叠() {
    let h = ContentHash::from_bytes([1u8; HASH_LEN]);
    let meta = keys::blob_meta_key(h);
    let bref = keys::blob_ref_key(h);
    let kref = keys::chunk_ref_key(h);
    assert_ne!(meta, bref);
    assert_ne!(meta, kref);
    assert_ne!(bref, kref);

    // blobmeta 列族里同时住着 `b|`（不可变元数据）与 `r|`（计数），
    // 对账扫描要能只扫其中一类。
    assert!(meta.starts_with(&keys::blob_meta_prefix()));
    assert!(!bref.starts_with(&keys::blob_meta_prefix()));
    assert!(kref.starts_with(&keys::chunk_ref_prefix()));
}

#[test]
fn 内容键就是裸哈希() {
    let h = ContentHash::from_bytes([0x5c; HASH_LEN]);
    assert_eq!(keys::content_key(h), h.as_bytes().to_vec());
    assert_eq!(keys::content_key(h).len(), HASH_LEN);
}

#[test]
fn 哈希hex往返与短前缀() {
    let mut bytes = [0u8; HASH_LEN];
    bytes[0] = 0x1a;
    bytes[1] = 0x2b;
    bytes[2] = 0x3c;
    bytes[3] = 0x4d;
    let h = ContentHash::from_bytes(bytes);

    assert_eq!(h.to_hex8(), "1a2b3c4d", "?v= 用的是 hex 前 8 位");
    assert_eq!(h.to_hex().len(), HASH_LEN * 2);
    assert!(h.to_hex().starts_with("1a2b3c4d"));
    assert_eq!(ContentHash::parse_hex(&h.to_hex()).expect("解析"), h);

    assert!(ContentHash::parse_hex("abc").is_err(), "长度不对");
    assert!(
        ContentHash::parse_hex(&"z".repeat(64)).is_err(),
        "非 hex 字符"
    );
}

// ---------------------------------------------------------------------------
// gc 与 stage
// ---------------------------------------------------------------------------

#[test]
fn gc键往返() {
    let h = ContentHash::from_bytes([9u8; HASH_LEN]);
    for kind in [GcKind::Blob, GcKind::Chunk] {
        let key = keys::gc_key(kind, 1789727178, h);
        let (k, ts, hash) = keys::parse_gc_key(&key).expect("解析");
        assert_eq!(k, kind);
        assert_eq!(ts, 1789727178);
        assert_eq!(hash, h);
    }
}

#[test]
fn gc键按到期时间字典序排列() {
    // GC 任务靠这个性质从队列头顺序扫到第一个未到期的项就停，不用全表遍历。
    // 时间戳必须是大端；小端会让这个性质完全失效，所以这条测试是防回归的关键。
    let h = ContentHash::from_bytes([0u8; HASH_LEN]);
    let mut keys_built: Vec<Vec<u8>> = [1u64, 255, 256, 65535, 65536, 1789727178, u64::MAX]
        .iter()
        .map(|ts| keys::gc_key(GcKind::Blob, *ts, h))
        .collect();
    let expected = keys_built.clone();
    keys_built.sort();
    assert_eq!(keys_built, expected, "到期时间序应等于 key 字典序");
}

#[test]
fn gc键的时间戳含零字节仍可解析() {
    // 时间戳 1 的大端编码是 00 00 00 00 00 00 00 01——七个 0x00。
    // 按分隔符切分必然切错，所以这条测试锁死「按偏移解析」。
    let h = ContentHash::from_bytes([0xff; HASH_LEN]);
    let (_, ts, hash) = keys::parse_gc_key(&keys::gc_key(GcKind::Chunk, 1, h)).expect("解析");
    assert_eq!(ts, 1);
    assert_eq!(hash, h);
}

#[test]
fn gc两类队列前缀分开() {
    let h = ContentHash::from_bytes([3u8; HASH_LEN]);
    let blob = keys::gc_key(GcKind::Blob, 100, h);
    let chunk = keys::gc_key(GcKind::Chunk, 100, h);
    assert!(blob.starts_with(&keys::gc_prefix(GcKind::Blob)));
    assert!(!blob.starts_with(&keys::gc_prefix(GcKind::Chunk)));
    assert!(chunk.starts_with(&keys::gc_prefix(GcKind::Chunk)));
}

#[test]
fn 分片键按序号排列且可解析() {
    let uid = UploadId::new();
    let mut built: Vec<Vec<u8>> = (0u32..300).map(|n| keys::stage_part_key(uid, n)).collect();
    let expected = built.clone();
    built.sort();
    assert_eq!(built, expected, "分片号序应等于 key 字典序（4 字节大端）");

    // 跨 256 边界是小端编码最容易暴露的地方。
    assert_eq!(
        keys::parse_stage_part_key(&keys::stage_part_key(uid, 0)).expect("解析"),
        0
    );
    assert_eq!(
        keys::parse_stage_part_key(&keys::stage_part_key(uid, 255)).expect("解析"),
        255
    );
    assert_eq!(
        keys::parse_stage_part_key(&keys::stage_part_key(uid, 256)).expect("解析"),
        256
    );
    assert_eq!(
        keys::parse_stage_part_key(&keys::stage_part_key(uid, u32::MAX)).expect("解析"),
        u32::MAX
    );

    let prefix = keys::stage_part_prefix(uid);
    assert!(keys::stage_part_key(uid, 7).starts_with(&prefix));
    assert!(
        !keys::stage_part_key(UploadId::new(), 7).starts_with(&prefix),
        "别的上传会话不该命中这个前缀"
    );
}

// ---------------------------------------------------------------------------
// 校验：分隔符注入与路径规则
// ---------------------------------------------------------------------------

#[test]
fn 分隔符注入被拒() {
    // 这是整套校验存在的首要理由：如果 path 里能塞进 0x00，
    // 攻击者就能构造出落在别的画布前缀下的 key。
    let cid = CanvasId::new();
    assert!(keys::file_key(cid, "a\u{0}b.jpg").is_err(), "path 含 0x00");
    assert!(keys::username_key("al\u{0}ice").is_err(), "用户名含 0x00");
    assert!(
        keys::validate_display_name("名\u{0}字").is_err(),
        "显示名含 0x00"
    );
}

#[test]
fn 非法路径逐条被拒() {
    let cases: &[(&str, &str)] = &[
        ("", "空路径"),
        ("/abs.jpg", "绝对路径"),
        ("a/", "以斜杠结尾"),
        ("a//b.jpg", "空段"),
        ("../etc/passwd", "相对段 .."),
        ("a/../b.jpg", "中间的 .."),
        ("./a.jpg", "相对段 ."),
        ("a\\b.jpg", "反斜杠"),
        ("a\nb.jpg", "换行符"),
        ("a\tb.jpg", "制表符"),
    ];
    for (path, why) in cases {
        assert!(
            keys::validate_path(path).is_err(),
            "应拒绝（{why}）: {path:?}"
        );
    }
}

#[test]
fn 路径长度与段数上限() {
    assert!(keys::validate_path(&"a".repeat(MAX_PATH_LEN)).is_ok());
    assert!(keys::validate_path(&"a".repeat(MAX_PATH_LEN + 1)).is_err());

    let ok: String = (0..MAX_PATH_SEGMENTS)
        .map(|_| "a")
        .collect::<Vec<_>>()
        .join("/");
    assert!(
        keys::validate_path(&ok).is_ok(),
        "正好 {MAX_PATH_SEGMENTS} 段"
    );

    let too_many: String = (0..MAX_PATH_SEGMENTS + 1)
        .map(|_| "a")
        .collect::<Vec<_>>()
        .join("/");
    assert!(keys::validate_path(&too_many).is_err());
}

#[test]
fn 合法路径被接受() {
    for path in [
        "a.jpg",
        "scene01/f001.jpg",
        "a/b/c/d/e/f/g.srt",
        "中文 文件名.md",
        "with.多个.点.json",
        "dash-and_underscore.txt",
    ] {
        keys::validate_path(path).unwrap_or_else(|e| panic!("应接受 {path:?}: {e}"));
    }
}

#[test]
fn 用户名规则() {
    for ok in [
        "alice",
        "a",
        "a_b-c.d",
        "User123",
        &"u".repeat(MAX_USERNAME_LEN),
    ] {
        keys::validate_username(ok).unwrap_or_else(|e| panic!("应接受 {ok:?}: {e}"));
    }
    for bad in [
        "",
        "has space",
        "中文名",
        "a/b",
        "a@b",
        &"u".repeat(MAX_USERNAME_LEN + 1),
    ] {
        assert!(keys::validate_username(bad).is_err(), "应拒绝 {bad:?}");
    }
}

#[test]
fn slug规则() {
    keys::validate_slug("k7Qm3xB9pLdR").expect("12 字符 base62");
    assert!(keys::validate_slug("short").is_err(), "长度不足");
    assert!(keys::validate_slug("k7Qm3xB9pLdRx").is_err(), "长度超出");
    assert!(
        keys::validate_slug("k7Qm3xB9pLd-").is_err(),
        "非 base62 字符"
    );
}

#[test]
fn 显示名规则比用户名宽松() {
    // 名字不进 key，所以中文、空格、emoji 都行——只挡控制字符。
    for ok in ["夏日海边", "My Canvas", "a b  c", "🎬 成片"] {
        keys::validate_display_name(ok).unwrap_or_else(|e| panic!("应接受 {ok:?}: {e}"));
    }
    for bad in ["", "   ", "a\u{0}b", "a\nb"] {
        assert!(keys::validate_display_name(bad).is_err(), "应拒绝 {bad:?}");
    }
}

#[test]
fn 解析函数对垃圾输入返回错误而不是panic() {
    // 解析的输入来自磁盘，损坏的键不该让整个服务崩掉。
    let garbage: &[&[u8]] = &[
        b"",
        b"f",
        b"f\0",
        b"x\0whatever",
        &[0xff, 0xfe, 0xfd],
        b"ch\0short",
        b"pb\0tooshort",
        b"s\0nope",
    ];
    for g in garbage {
        let _ = keys::parse_file_key(g);
        let _ = keys::parse_child_key(g);
        let _ = keys::parse_gc_key(g);
        let _ = keys::parse_hash_key(g);
        let _ = keys::parse_stage_part_key(g);
    }
    // 走到这里就说明没 panic。再确认几个确实返回了 Err。
    assert!(keys::parse_file_key(b"f\0").is_err());
    assert!(keys::parse_gc_key(b"pb\0tooshort").is_err());
    assert!(keys::parse_hash_key(b"").is_err());
}

#[test]
fn 自动共享键空间项目隔离且不污染旧前缀() {
    let uid = Uid::new();
    let a = ProjectId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let b = ProjectId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
    let cid = CanvasId::new();
    let gid = GenerationId::new();
    let ready = gallery_ready_key(a, cid);
    let done = gallery_done_key(a, gid);
    assert!(ready.starts_with(&gallery_ready_prefix(a)));
    assert!(!ready.starts_with(&gallery_ready_prefix(b)));
    assert!(!gallery_ready_key(b, cid).starts_with(&gallery_ready_prefix(a)));
    assert!(done.starts_with(&gallery_done_prefix(a)));
    assert!(done.starts_with(&gallery_done_prefix_all()));
    assert!(!done.starts_with(&gallery_done_prefix(b)));
    let all = [
        gallery_preference_key(uid),
        gallery_control_key(a),
        ready,
        done,
        project_share_key(a),
    ];
    for (i, key) in all.iter().enumerate() {
        assert!(!key.starts_with(&user_prefix()));
        assert!(!key.starts_with(&gallery_feed_prefix()));
        for other in all.iter().skip(i + 1) {
            assert_ne!(key, other);
        }
    }
}
