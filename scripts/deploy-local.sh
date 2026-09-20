#!/usr/bin/env bash
#
# Memora（忆庐）本地部署脚本 —— 一条命令把服务在本机跑起来，并交付可直接粘贴的客户端配置。
#
# @author yujinping
# @intent 把「构建 → 生成配置 → 启动 → 健康自检 → 建首个项目 → 输出客户端配置」这 6 步
#         固化成一条命令，消除手工部署时最容易出错的两个环节：
#           1. 忘了把 admin_token 从模板占位符改成随机值（等于管理面无鉴权）；
#           2. 用「旧二进制」测「新代码」——步骤 1 会把这一点显式提示出来。
#         所有步骤幂等，可安全重复执行；已存在的配置与项目凭据一律复用，不覆盖。
#
# 用法：
#   scripts/deploy-local.sh                          # 默认：端口 6789，项目 my-project
#   PORT=6790 PROJECT=demo scripts/deploy-local.sh   # 换端口与项目名
#   FORCE_INIT=1 scripts/deploy-local.sh             # 强制重新生成配置（覆盖旧配置）
#   MEMORA_BIN=/usr/local/bin/memora scripts/deploy-local.sh   # 用已安装的二进制
#
# 停止：<二进制> stop        查看：<二进制> status（未运行时退出码 3）

set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${MEMORA_BIN:-${REPO_ROOT}/target/release/memora}
PROJECT=${PROJECT:-my-project}
WANT_PORT=${PORT:-}
FORCE_INIT=${FORCE_INIT:-0}

# 与 src/config.rs 的默认路径解析保持一致：环境变量优先，未设置才回退固定默认值。
CONFIG=${MEMORA_CONFIG:-${XDG_CONFIG_HOME:-${HOME}/.config}/memora/config.toml}

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\n[失败] %s\n' "$*" >&2; exit 1; }

# 从 TOML 取标量值。刻意不用 python：macOS 12 自带的 python3 是 3.9，没有 tomllib，
# 而本脚本要能在「只有系统 bash + curl」的机器上直接跑。先删注释行，再取首个匹配，
# 避免把注释里出现的 `key =` 当成配置读到。
tok() {
    [ -f "${CONFIG}" ] || return 1
    sed -e '/^[[:space:]]*#/d' "${CONFIG}" \
        | sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" \
        | head -n 1 \
        | sed -e 's/^"//' -e 's/"[[:space:]]*$//' -e "s/^'//" -e "s/'[[:space:]]*$//"
}

# ---------- 1. 构建：确保二进制不早于源码 ----------
if [ ! -x "${BIN}" ]; then
    say "构建 release 二进制（首次约 1 分钟）"
    (cd "${REPO_ROOT}" && cargo build --release)
else
    echo "==> 复用已有二进制：${BIN}"
    echo "    若源码有改动，请先 cargo build --release，否则测的是旧版本行为"
fi

# ---------- 2. 生成配置：绝不留下占位 token ----------
if [ -f "${CONFIG}" ] && [ "${FORCE_INIT}" != "1" ]; then
    say "配置已存在，跳过 init"
    echo "    配置文件：${CONFIG}"
    echo "    重建配置：FORCE_INIT=1 scripts/deploy-local.sh"
else
    say "生成配置：${CONFIG}"
    # 随机 token 由 openssl 生成后经 --admin-token 注入：init 只在「未指定」时才写
    # 占位值，显式传入才能保证落盘的就是真实凭据。
    init_args=(init --admin-token "$(openssl rand -hex 32)")
    if [ -n "${WANT_PORT}" ]; then
        init_args+=(--port "${WANT_PORT}")
    fi
    if [ "${FORCE_INIT}" = "1" ]; then
        init_args+=(--force)
    fi
    "${BIN}" "${init_args[@]}"
fi

CFG_PORT=$(tok port)
DATA_DIR=$(tok data_dir)
ADMIN=$(tok admin_token)
[ -n "${CFG_PORT}" ] || die "无法从 ${CONFIG} 读到 port，请检查配置语法"
[ -n "${ADMIN}" ] || die "配置里没有 admin_token，管理面将全部 401"

# ---------- 3. 启动 ----------
if "${BIN}" status >/dev/null 2>&1; then
    say "服务已在运行，跳过启动"
else
    # 端口预检：`memora start` 自身已有这道检查（spawn 前试绑端口），此处再提前一层，
    # 是为了给出**本脚本语境**下的补救命令（`PORT=<其他端口>`）——二进制只会建议 `--port`。
    if curl -fsS --max-time 2 "http://127.0.0.1:${CFG_PORT}/health" >/dev/null 2>&1; then
        die "端口 ${CFG_PORT} 上已有另一个 memora 实例在服务，但本配置对应的实例记录不存在。请换端口重跑（PORT=6790 scripts/deploy-local.sh），或先停掉占用者。"
    fi
    say "启动服务（后台常驻）"
    "${BIN}" start
fi

# ---------- 4. 健康自检 ----------
say "验证 /health"
HEALTH=$(curl -fsS "http://127.0.0.1:${CFG_PORT}/health" 2>/dev/null) \
    || die "健康检查失败，日志见 ${DATA_DIR}/memora.log"
[ "${HEALTH}" = "ok" ] || die "健康检查响应异常：${HEALTH}"
echo "    /health -> ${HEALTH}"

# ---------- 5. 项目与凭据（幂等：凭据落盘后不再重建项目） ----------
CRED_FILE=${DATA_DIR}/.${PROJECT}.json
if [ -f "${CRED_FILE}" ]; then
    say "复用已有项目凭据"
    echo "    凭据文件：${CRED_FILE}"
    TOKEN=$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' "${CRED_FILE}")
else
    say "创建项目：${PROJECT}"
    RESP=$(curl -sS -X POST "http://127.0.0.1:${CFG_PORT}/api/v1/projects" \
        -H "Authorization: Bearer ${ADMIN}" \
        -H 'Content-Type: application/json' \
        -d "{\"project_id\":\"${PROJECT}\"}")
    TOKEN=$(printf '%s' "${RESP}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
    if [ -z "${TOKEN}" ]; then
        die "创建项目失败，响应：${RESP}（若是 409，说明项目已登记但凭据丢失；先注销再重建：curl -X DELETE http://127.0.0.1:${CFG_PORT}/api/v1/projects/${PROJECT} -H 'Authorization: Bearer <admin_token>'）"
    fi
    umask 077
    printf '%s' "${RESP}" > "${CRED_FILE}"
    echo "    明文 token 已保存到 ${CRED_FILE}"
    echo "    （只出现这一次，服务端仅存 SHA-256；丢失只能注销重建）"
fi

# ---------- 6. 交付客户端配置 ----------
say "客户端配置（项目 ${PROJECT}；本机 HTTP，无需 Caddy）"
cat <<JSON
{
  "mcpServers": {
    "memora": {
      "url": "http://127.0.0.1:${CFG_PORT}/mcp",
      "headers": { "Authorization": "Bearer ${TOKEN}" }
    }
  }
}
JSON
echo "    WorkBuddy: ~/.workbuddy/mcp.json    Cursor: ~/.cursor/mcp.json"

say "常用命令"
echo "    注意：上方 JSON 与下方命令含明文凭据，请勿连同终端输出一起分享"
cat <<EOF
  status  : ${BIN} status       # 未运行时退出码 3
  log     : tail -f ${DATA_DIR}/memora.log
  stop    : ${BIN} stop
  restart : ${BIN} restart      # 改过 ${CONFIG} 后用它生效
  smoke   : python3 ${REPO_ROOT}/scripts/smoke-e2e.py --base-url http://127.0.0.1:${CFG_PORT} --admin-token ${ADMIN}
EOF
