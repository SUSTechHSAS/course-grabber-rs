#!/usr/bin/env bash
# 打包成"下载即用"的压缩包（Linux / macOS）。
#
#     ./packaging/build.sh
#     产物: dist/course-grabber-<平台>-<架构>.{tar.gz,zip}
#
# 跟原版（Python + PyInstaller）比，这里几乎没有"打包"这件事可做：
# cargo build --release 出来的**就是一个**文件，识别模型和推理代码都编在里面。
# 所以这个脚本只负责"编译 → 摆到一个目录里 → 压缩"。
#
# Windows 不走这里（bash 未必有）；CI 里用 PowerShell 的 Compress-Archive 打 zip。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NAME=course-grabber

# 识别库（模型编在它里面）是 path 依赖，必须就位才能编译
if [ ! -f ../click-captcha-matcher-rs/Cargo.toml ]; then
  echo "✗ 找不到识别库 ../click-captcha-matcher-rs" >&2
  echo "  先 clone：git clone https://github.com/SUSTechHSAS/click-captcha-matcher-rs ../click-captcha-matcher-rs" >&2
  exit 2
fi

case "$(uname -s)" in
  Linux)  OS=linux ;;
  Darwin) OS=macos ;;
  *)      OS="$(uname -s | tr '[:upper:]' '[:lower:]')" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH=x64 ;;
  aarch64|arm64) ARCH=arm64 ;;
  *) ARCH="$(uname -m)" ;;
esac
TAG="$OS-$ARCH"

echo "+ cargo build --release"
cargo build --release

OUT="dist/$NAME-$TAG"
rm -rf "$OUT"
mkdir -p "$OUT"
cp "target/release/$NAME" "$OUT/$NAME"
chmod 755 "$OUT/$NAME"
cp config.example.json README.md LICENSE "$OUT/"
cp packaging/run.sh "$OUT/run.sh"
chmod 755 "$OUT/run.sh"

if [ "$OS" = macos ]; then
  ARCHIVE="dist/$NAME-$TAG.zip"
  rm -f "$ARCHIVE"
  (cd dist && zip -qr "$(basename "$ARCHIVE")" "$(basename "$OUT")")
else
  ARCHIVE="dist/$NAME-$TAG.tar.gz"
  rm -f "$ARCHIVE"
  tar -C dist -czf "$ARCHIVE" "$(basename "$OUT")"
fi

BIN_BYTES=$(stat -c %s "$OUT/$NAME" 2>/dev/null || stat -f %z "$OUT/$NAME")
echo
echo "✅ 产物: $ARCHIVE"
echo "   可执行文件 $(echo "scale=1; $BIN_BYTES/1024" | bc) KB（含识别模型）"
echo "   $(du -h "$ARCHIVE" | cut -f1) 压缩后"
