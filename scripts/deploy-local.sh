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
#   scripts/deploy-local.sh --uninstall --dry-run     # 查看卸载会移除哪些路径（不做改动）
#   scripts/deploy-local.sh --uninstall --yes         # 执行卸载（撤销一次本地部署）
#
# 卸载顺序不可颠倒：停进程 → 摘客户端条目 → 移数据目录 → 移配置目录。
# 详见下文 uninstall_main 上方的注释。所有目标一律移入废纸篓，不做不可逆删除。
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

# ---------- 选项解析 ----------
UNINSTALL=0
ASSUME_YES=0
DRY_RUN=0
CLIENT_CONFIGS=()

usage() {
    cat <<'EOF'
用法：
  scripts/deploy-local.sh [选项]                 部署（默认行为）
  scripts/deploy-local.sh --uninstall [选项]     卸载：撤销一次本地部署

部署选项（以环境变量给出）：
  PORT=<端口>           指定端口            PROJECT=<名>        指定项目名
  FORCE_INIT=1          重建配置            MEMORA_BIN=<路径>   指定二进制
  MEMORA_CONFIG=<路径>  指定配置文件

卸载选项：
  --uninstall             执行卸载
  --yes, -y               跳过交互确认（非交互环境必需）
  --dry-run               只打印将移除的路径，不做任何改动
  --client-config <路径>  额外指定客户端 MCP 配置
                          （默认识别 ~/.workbuddy/mcp.json 与 ~/.cursor/mcp.json）
  -h, --help              显示本帮助

卸载顺序固定为：停进程 → 摘客户端条目 → 移数据目录 → 移配置目录。
所有目标一律移入废纸篓，不做不可逆删除。
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall) UNINSTALL=1 ;;
        -y|--yes)    ASSUME_YES=1 ;;
        --dry-run)   DRY_RUN=1 ;;
        --client-config)
            shift
            [ $# -gt 0 ] || die "--client-config 缺少参数（应为客户端配置文件的路径）"
            CLIENT_CONFIGS+=("$1")
            ;;
        -h|--help)   usage; exit 0 ;;
        *) die "未知选项：$1（可用：--uninstall / --yes / --dry-run / --client-config <路径> / --help）" ;;
    esac
    shift
done

# ---------- 卸载 ----------
#
# @intent 卸载必须与部署严格对称，且**顺序不可颠倒**。核心约束来自实例记录的位置：
#         `memora.pid` 就落在数据目录内（`$DATA_DIR/memora.pid`），因此「先删目录再停服务」
#         会让 `stop` 失去定位进程的唯一句柄——残留进程继续持有 SQLite 连接与监听端口，
#         只能靠 lsof + kill 手工了结。故顺序固定为：
#
#             停进程 → 摘客户端条目 → 移数据目录 → 移配置目录
#
#         配置必须**晚于**数据目录删除：若 `data_dir` 是自定义值，配置一丢，后续任何按配置
#         推导路径的操作都会退回内置默认值而指向错误位置。
#
#         第三个不对称点是客户端条目：部署时本脚本只**打印**那段 JSON 供用户手贴，从不写盘，
#         所以「本脚本创建过的东西」清单里没有它。若卸载只按该清单删，客户端会长期挂着一个
#         指向死端点的连接器——这正是卸载入口必须留在脚本层（而非二进制）的原因。

TRASH_DIR="${HOME}/.Trash"
# Linux 上按 XDG trash 规范回退（macOS 无此目录，保持 ~/.Trash）
[ -d "${TRASH_DIR}" ] || TRASH_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/Trash/files"

# 端口是否仍被监听。优先 lsof，被沙箱限制时回退到 bash 的 /dev/tcp（bash 3.2 支持）。
port_is_listening() {
    local port="$1"
    [ -n "${port}" ] || return 1
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"${port}" -sTCP:LISTEN >/dev/null 2>&1 && return 0
    fi
    (echo >"/dev/tcp/127.0.0.1/${port}") >/dev/null 2>&1 && return 0
    return 1
}

# 生成废纸篓内的唯一目标名，避免同一秒内多次操作互相覆盖。
trash_dest() {
    local base="$1"
    [ -d "${TRASH_DIR}" ] || die "废纸篓目录不存在（${TRASH_DIR}），拒绝直接删除。请手工处理待移除项。"
    local dest="${TRASH_DIR}/${base}-$(date +%Y%m%d-%H%M%S)"
    local i=1
    while [ -e "${dest}" ]; do
        dest="${TRASH_DIR}/${base}-$(date +%Y%m%d-%H%M%S)-${i}"
        i=$((i + 1))
    done
    printf '%s' "${dest}"
}

# 把路径移入废纸篓——不做不可逆删除。不存在的路径视为已完成（幂等）。
to_trash() {
    local p="$1"
    [ -e "${p}" ] || { echo "    不存在，跳过：${p}"; return 0; }
    local dest
    dest=$(trash_dest "$(basename -- "${p}")")
    if [ "${DRY_RUN}" = 1 ]; then
        echo "    [dry-run] 将移入废纸篓：${p}  ->  ${dest}"
        return 0
    fi
    mv "${p}" "${dest}" && echo "    已移入废纸篓：${dest}"
}

# 规范化路径：经 realpath 消解符号链接后再比较。
#
# @intent **两侧必须同源规范化**，且未存在的路径也要规范化。两处都踩过：
#         1. macOS 上 `/tmp` 经 `pwd -P` 变成 `/private/tmp`。若拿解析后的目标比对未解析的
#            `${HOME}`，护栏会静默失效（实测 `data_dir = $HOME` 被放行）。
#         2. 只对「已存在」的路径做 realpath 时，未存在的路径得不到规范化，与同级已存在路径
#            比对时会出现「同一目录两种写法」而绕过护栏（实测未创建 `$HOME/Documents` 时被放行）。
#         故先上溯到最近的已存在祖先，再拼回未存在的尾部。
canon() {
    local p="$1" suffix=""
    [ -n "${p}" ] || return 1
    while [ ! -e "${p}" ] && [ "${p}" != "/" ] && [ "${p}" != "." ]; do
        suffix="/$(basename -- "${p}")${suffix}"
        p=$(dirname -- "${p}")
    done
    local base
    base=$(cd -- "${p}" 2>/dev/null && pwd -P) || base="${p}"
    # base 为根时不能直接拼后缀（会得到 "//Documents"）；后缀也为空时说明目标就是根。
    if [ "${base}" != "/" ]; then
        printf '%s%s' "${base}" "${suffix}"
    elif [ -n "${suffix}" ]; then
        printf '%s' "${suffix}"
    else
        printf '/'
    fi
}

# 范围护栏：拒绝把「过宽」的目录交给卸载流程。`data_dir` 是配置里用户可写的字符串，
# 一个笔误（或一条被改坏的配置）就足以让移除范围超出预期，故在动手前先卡住。
guard_broad() {
    local target="$1" label="$2"
    [ -n "${target}" ] || die "拒绝执行：${label}解析为空"
    local real h r
    real=$(canon "${target}")
    h=$(canon "${HOME}")
    r=$(canon "${REPO_ROOT}")
    case "${real}" in
        # 系统 / 根级目录。原始形式与 macOS 解析后形式都要列——/tmp 与 /private/tmp 是同一个
        # 目录的两个写法，只列其中一个就会被另一条路径绕过。
        /|/tmp|/private/tmp|/var|/private/var|/etc|/private/etc|/usr|/private|/System|/Library|/Applications|/Volumes|/Users|/home|/srv|/mnt|/media|/opt)
            die "拒绝执行：${label}解析为系统级目录 ${real}，卸载流程不会移除它"
            ;;
        # 家目录本身，及其下普遍存放生活/系统内容的目录。注意**只拒绝这些目录本身**，
        # 不拒绝其下用户自建的子目录——`data_dir = ~/Documents/memora` 是有意为之，
        # 而 `data_dir = ~/Documents` 才是那个会毁掉整个文档目录的笔误。
        "${h}"|"${h}/.config"|"${h}/.local"|"${h}/.local/share"|"${h}/.local/state"|"${h}/.cache"|"${h}/Library"|"${h}/Desktop"|"${h}/Documents"|"${h}/Downloads"|"${h}/Pictures"|"${h}/Movies"|"${h}/Music")
            die "拒绝执行：${label}解析为 ${real}，范围过大，卸载流程不会移除它"
            ;;
        # 仓库自身
        "${r}"|"${r}/target"|"${r}/src"|"${r}/deploy"|"${r}/docs")
            die "拒绝执行：${label}解析为仓库目录 ${real}，卸载流程不会移除它"
            ;;
    esac
    # 兜底：至少两级路径成分（如 /a/b），排除 /x 这类顶层挂载点
    local comps
    comps=$(printf '%s' "${real}" | awk -F/ '{n=0; for (i=1;i<=NF;i++) if ($i!="") n++; print n}')
    [ "${comps}" -ge 2 ] || die "拒绝执行：${label}解析为 ${real}，层级过浅（需 ≥2 级），疑似配置写错"
    echo "    ${label}：${real}"
}

# 判定客户端配置里 memora 条目的归属状态，输出 one of:
#   no-entry / whole-file / stripped / invalid / no-tool
# 用 python3 而非 jq：macOS 自带 python3（3.9 起 json 即标准库），jq 需另外安装。
# 这与上方 tok() 刻意不用 python 的理由不冲突——那里需要的是 3.11+ 的 tomllib。
client_state() {
    command -v python3 >/dev/null 2>&1 || { echo "no-tool"; return 0; }
    python3 - "$1" <<'PY'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as fh:
        d = json.load(fh)
except Exception:
    print("invalid"); raise SystemExit(0)
srv = d.get("mcpServers") if isinstance(d, dict) else None
if not isinstance(srv, dict) or "memora" not in srv:
    print("no-entry"); raise SystemExit(0)
# 只剩 memora 一条、且没有其它顶层键 → 该文件就是为 memora 建的，可整体移走
if len(srv) == 1 and set(d.keys()) == {"mcpServers"}:
    print("whole-file")
else:
    print("stripped")
PY
}

# 摘除 memora 条目：改写后的内容顶替原位置，**原文件先进废纸篓**（而非就地覆盖）。
# 新文件以 umask 077 创建，因此权限为 600——它含明文 token，不应比原来更宽松。
client_strip() {
    local f="$1" tmp="${1}.memora-tmp.$$"
    (umask 077; python3 - "${f}" "${tmp}" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as fh:
    d = json.load(fh)
d["mcpServers"].pop("memora", None)
with open(sys.argv[2], "w", encoding="utf-8") as fh:
    json.dump(d, fh, indent=2, ensure_ascii=False)
    fh.write("\n")
PY
    )
    to_trash "${f}"
    mv "${tmp}" "${f}"
}

# 停服务，并以「端口真的不再监听」为唯一判据——而不是以 `stop` 的退出码为准。
stop_service() {
    if [ -x "${BIN}" ]; then
        "${BIN}" stop || echo "    stop 返回非零，继续核查端口占用"
    else
        echo "    二进制不可执行（${BIN}），改用端口定位进程"
    fi
    [ -n "${CFG_PORT}" ] || { echo "    端口未知（配置不可读），无法核查剩余监听，请手工确认进程已退出"; return 0; }
    local tries=0
    while [ "${tries}" -lt 10 ]; do
        port_is_listening "${CFG_PORT}" || break
        sleep 0.3
        tries=$((tries + 1))
    done
    if port_is_listening "${CFG_PORT}"; then
        local pid
        pid=$(lsof -tnP -iTCP:"${CFG_PORT}" -sTCP:LISTEN 2>/dev/null | head -n 1)
        die "端口 ${CFG_PORT} 仍有进程在监听（pid=${pid:-未知}）。卸载已中止，未删除任何数据；请先终止该进程后重跑"
    fi
    echo "    端口 ${CFG_PORT} 已释放"
}

confirm_or_die() {
    if [ "${ASSUME_YES}" = 1 ]; then
        return 0
    fi
    if [ ! -t 0 ]; then
        die "非交互环境：请显式加 --yes 确认，或先 --dry-run 查看将被移除的路径"
    fi
    printf '\n以上路径将被移入废纸篓。输入 yes 继续：'
    local ans=""
    read -r ans
    [ "${ans}" = "yes" ] || die "已取消，未做任何改动"
}

uninstall_main() {
    say "卸载：解析目标路径"
    local config_dir data_dir
    config_dir=$(dirname -- "${CONFIG}")

    data_dir=$(tok data_dir || true)
    if [ -z "${data_dir}" ]; then
        data_dir="${XDG_DATA_HOME:-${HOME}/.local/share}/memora"
        echo "    配置不可读，data_dir 退回默认：${data_dir}"
        echo "    注意：若当初以 --data-dir 指定过自定义目录，本次覆盖不到它，需手工清理"
    fi
    CFG_PORT=$(tok port || true)

    guard_broad "${data_dir}" "数据目录"
    guard_broad "${config_dir}" "配置目录"

    # 前置检查：废纸篓不可用就**在此刻**拒绝，而不是走到第 3 步才发现——那会留下
    # 「服务已停、客户端条目已摘、数据却还在」的半成品状态，比直接不开始更难收拾。
    if [ "${DRY_RUN}" != 1 ] && [ ! -d "${TRASH_DIR}" ]; then
        die "废纸篓目录不存在（${TRASH_DIR}）。本脚本不做不可逆删除，故拒绝执行；请先创建该目录，或手工处理上述路径"
    fi

    # 客户端配置：默认探测两个常见位置，外加 --client-config 显式指定项
    local client_files=()
    local c
    for c in "${HOME}/.workbuddy/mcp.json" "${HOME}/.cursor/mcp.json"; do
        [ -f "${c}" ] && client_files+=("${c}")
    done
    if [ ${#CLIENT_CONFIGS[@]} -gt 0 ]; then
        for c in "${CLIENT_CONFIGS[@]}"; do
            [ -f "${c}" ] && client_files+=("${c}")
        done
    fi

    local client_desc="<无>"
    if [ ${#client_files[@]} -gt 0 ]; then
        client_desc="${client_files[*]}"
    fi

    say "将执行以下 4 步（全部移入废纸篓 ${TRASH_DIR}）"
    echo "  1) 停止服务        端口 ${CFG_PORT:-<未知>}；二进制 ${BIN}"
    echo "  2) 摘除客户端条目  ${client_desc}"
    echo "  3) 移走数据目录    ${data_dir}"
    echo "  4) 移走配置目录    ${config_dir}"

    if [ "${DRY_RUN}" = 1 ]; then
        say "dry-run：以上仅为计划，未做任何改动"
        exit 0
    fi
    confirm_or_die

    say "1/4 停止服务"
    stop_service

    say "2/4 摘除客户端条目"
    local f st manual=0
    if [ ${#client_files[@]} -gt 0 ]; then
        for f in "${client_files[@]}"; do
            st=$(client_state "${f}")
            case "${st}" in
                no-entry)   echo "    无 memora 条目，跳过：${f}" ;;
                whole-file) to_trash "${f}" ;;
                stripped)
                    client_strip "${f}"
                    echo "    已摘除 memora 条目，原文件备份在废纸篓：${f}"
                    ;;
                invalid)
                    echo "    [需人工处理] JSON 无法解析，未改动：${f}"
                    manual=1
                    ;;
                no-tool)
                    echo "    [需人工处理] 未找到 python3，无法安全摘除条目。请手工删除该文件中的 mcpServers.memora：${f}"
                    manual=1
                    ;;
            esac
        done
    else
        echo "    未发现客户端配置"
    fi

    say "3/4 移走数据目录"
    to_trash "${data_dir}"

    say "4/4 移走配置目录"
    to_trash "${config_dir}"

    # ---------- 校验 ----------
    say "校验"
    local ok=1
    [ -e "${data_dir}" ] && { echo "    [异常] 数据目录仍存在：${data_dir}"; ok=0; }
    [ -e "${config_dir}" ] && { echo "    [异常] 配置目录仍存在：${config_dir}"; ok=0; }
    if [ -n "${CFG_PORT}" ] && port_is_listening "${CFG_PORT}"; then
        echo "    [异常] 端口 ${CFG_PORT} 仍在监听"
        ok=0
    fi
    [ "${ok}" = 1 ] && echo "    数据目录、配置目录、监听端口均已清理"

    say "卸载完成"
    echo "    备份归档（若生成过）与 target/ 不在卸载范围，按需自行清理"
    [ "${manual}" = 1 ] && echo "    有客户端配置需人工处理，见上"
    echo "    已移入废纸篓的内容可用 Finder 恢复：open ${TRASH_DIR}"
    exit 0
}

if [ "${UNINSTALL}" = 1 ]; then
    uninstall_main
fi

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

# 带 `|| true`：tok 在配置文件缺失时返回非零，而 `var=$(…)` 的失败在 set -e 下会**直接终止
# 脚本**，使下面几行精心写的报错永远执行不到。吞掉它，把判断交给下面的显式检查。
CFG_PORT=$(tok port || true)
DATA_DIR=$(tok data_dir || true)
ADMIN=$(tok admin_token || true)
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
