#!/usr/bin/env bash
# 生成测试数据到 test/data/generated/（该目录不进版本库）。
#
# 数据集刻意贴近一个真实画布的构成（见总览文档第 9 节的容量估算）：
#   small/*.jpg  300 个 200KB 不可压缩内容 —— 验批量写入与 blob_bin 不压缩
#   text/*.json   50 个  20KB 可压缩文本   —— 验 blob_txt 的 zstd 收益
#   big.mp4      31457280 字节             —— 8 个块（7 个 4MB + 末块 2MB）
#   dup.mp4      big.mp4 的副本            —— 验去重：占用不应再增长
#
# big.mp4 的大小刻意不是 4MB 整数倍：末块短块是最容易写错的地方。
set -euo pipefail

cd "$(dirname "$0")"
out="generated"

if [[ "${1:-}" == "--clean" ]]; then
    rm -rf "$out"
    echo "已清理 $out"
    exit 0
fi

mkdir -p "$out/small" "$out/text"

echo "生成 300 个 200KB 图片素材..."
for i in $(seq -w 1 300); do
    f="$out/small/f$i.jpg"
    [[ -f "$f" ]] || head -c 204800 /dev/urandom > "$f"
done

echo "生成 50 个 20KB 文本..."
for i in $(seq -w 1 50); do
    f="$out/text/t$i.json"
    if [[ ! -f "$f" ]]; then
        # 高度可压缩的重复文本，用来观察 zstd 的实际收益。
        {
            echo '{"scene":'"$i"',"frames":['
            for j in $(seq 1 400); do
                echo '  {"idx":'"$j"',"at":0.04,"kind":"frame","note":"repeatable text"},'
            done
            echo '  {"idx":0,"at":0,"kind":"end","note":"repeatable text"}'
            echo ']}'
        } > "$f"
    fi
done

echo "生成 30MB 成片（31457280 字节 = 7×4MB + 2MB，共 8 块）..."
if [[ ! -f "$out/big.mp4" ]]; then
    head -c 31457280 /dev/urandom > "$out/big.mp4"
fi

echo "生成去重用副本..."
cp -f "$out/big.mp4" "$out/dup.mp4"

echo
echo "完成。占用："
du -sh "$out"
echo "big.mp4 大小：$(stat -c %s "$out/big.mp4") 字节"
