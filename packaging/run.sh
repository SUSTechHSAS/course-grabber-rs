#!/usr/bin/env bash
# 一键运行：第一次会自动生成 config.json 并告诉你填什么。
set -euo pipefail
cd "$(dirname "$0")"

BIN=./course-grabber
[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN"; exit 1; }

# 程序自己也会做这件事（第一次运行时按内编模板生成 config.json），
# 这里只是把"下一步该干嘛"再说一遍，免得用户被一行报错吓到。
if [ ! -f config.json ]; then
  "$BIN" --offline || true
  echo "=============================================================="
  echo " 请填上你学校的域名、接口路径与候选教学班："
  echo "     \${EDITOR:-vi} $(pwd)/config.json"
  echo ""
  echo " 想用自动登录的话，再准备凭据文件（0600）："
  echo "     mkdir -p ~/.config/course-grabber"
  echo "     cat > ~/.config/course-grabber/credentials.json <<'JSON'"
  echo '     {"student_id": "你的学号", "password": "你的密码"}'
  echo "     JSON"
  echo "     chmod 600 ~/.config/course-grabber/credentials.json"
  echo "=============================================================="
  exit 0
fi

exec "$BIN" "$@"
