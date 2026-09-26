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
  echo " 最省事：跑配置界面，照里面的「三步向导」走。"
  echo ""
  echo "     $BIN tui"
  echo ""
  echo "   ① 粘一条选课页网址（浏览器里 Ctrl-L 全选地址栏、Ctrl-C）"
  echo "      —— 域名、接口前缀、页面路径会自动填好"
  echo "   ② 填学号密码（用来自动登录；界面里保存就是 0600）"
  echo "   ③ 让程序登录把课程目录拉下来，空格勾选候选教学班"
  echo "   三步做完按 s 保存，按 q 退出。"
  echo ""
  echo " 不想用界面也行，手写 config.json（填域名、接口路径与候选教学班）："
  echo "     \${EDITOR:-vi} $(pwd)/config.json"
  echo ""
  echo " 凭据文件（用自动登录的话，0600 —— 界面里填就不用手工建）："
  echo "     mkdir -p ~/.config/course-grabber"
  echo "     cat > ~/.config/course-grabber/credentials.json <<'JSON'"
  echo '     {"student_id": "你的学号", "password": "你的密码"}'
  echo "     JSON"
  echo "     chmod 600 ~/.config/course-grabber/credentials.json"
  echo "=============================================================="
  exit 0
fi

exec "$BIN" "$@"
