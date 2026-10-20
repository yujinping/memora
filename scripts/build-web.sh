#!/usr/bin/env bash
#
# 构建控制台前端，产物落到 `src/console_dist/`，供 `build.rs` 内嵌进二进制。
#
# @author yujinping
# @intent 为什么要先跑这一步：Rust 侧的内嵌是**构建期**完成的（`build.rs` 扫描
#         `src/console_dist/` 生成资源清单）。没有产物时 `cargo build` 会直接
#         panic 而不是产出一个「没有控制台的二进制」——刻意如此：空控制台二进制
#         能正常启动，用户要到打开浏览器时才发现 404，那时已无从定位。
#
# @intent 产物入库（而非 CI 临时生成）：这样 `git clone && cargo build` 依然可用，
#         不需要在构建机上装 Node 与 pnpm。代价是每次改前端都要把产物一并提交，
#         故本脚本末尾会提示产物文件清单，便于 `git add`。
#
# 用法：
#   scripts/build-web.sh            # 安装依赖（有 lockfile 则严格按锁文件）并构建
#   scripts/build-web.sh --no-install  # 只构建，跳过依赖安装

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEB_DIR="$REPO_ROOT/web"
DIST_DIR="$REPO_ROOT/src/console_dist"

install_deps=1
for arg in "$@"; do
  case "$arg" in
    --no-install) install_deps=0 ;;
    -h | --help)
      sed -n '3,20p' "${BASH_SOURCE[0]}"
      exit 0
      ;;
    *)
      echo "未知参数：$arg（可用：--no-install）" >&2
      exit 2
      ;;
  esac
done

if ! command -v pnpm >/dev/null 2>&1; then
  echo "未找到 pnpm。安装：npm i -g pnpm" >&2
  exit 1
fi

cd "$WEB_DIR"

if [ "$install_deps" -eq 1 ]; then
  if [ -f pnpm-lock.yaml ]; then
    # @intent 有锁文件时用 --frozen-lockfile：控制台产物会进二进制并分发给用户，
    #         依赖版本必须在「本机构建」与「他机构建」之间完全一致。
    pnpm install --frozen-lockfile
  else
    echo "提示：尚无 pnpm-lock.yaml，本次安装后请把它一并提交，以固定依赖版本。"
    pnpm install
  fi
fi

# @intent `build` 脚本内已串联 `vue-tsc -b`（类型检查）与 `vite build`；
#         不绕过它直接调 vite，否则类型错误会被静默放过。
#
# @intent 显式关掉 `verifyDepsBeforeRun`：pnpm 11 在每次 `run` 之前会做一次依赖
#         状态检查，发现 lockfile 与 node_modules 不符时**自动执行 `pnpm install`**。
#         对一个已经声明「不装依赖就只构建」的脚本来说，这是背着调用方改
#         `node_modules`；而依赖是否要更新，应当由上面那段 `install_deps` 决定。
pnpm run --config.verify-deps-before-run=false build

if [ ! -f "$DIST_DIR/index.html" ]; then
  echo "构建结束但 $DIST_DIR/index.html 不存在——请检查 web/vite.config.ts 的 outDir。" >&2
  exit 1
fi

echo
echo "已生成内嵌资源："
cd "$REPO_ROOT"
find src/console_dist -type f | sort | sed 's/^/  /'
echo
echo "提示：这些文件需要提交（保证 clone 后无需 Node 即可 cargo build）。"