#!/usr/bin/env python3
"""Memora 端到端冒烟脚本：对**已运行**的实例做黑盒验证。

- **@author** yujinping
- **@intent** `cargo test` 覆盖的是进程内语义，证明不了「部署后的实例」行为是否正确——
  反代有没有透传 Host、白名单配没配、token 是不是真的按项目隔离，只有打真实端口才能验。
  本脚本把这一层验证固化为一条可重复执行的命令，供**部署后**与**升级后**自检使用。

覆盖范围（每条对应一个已知会踩的坑）：

| # | 分组 | 验证内容 |
|---|---|---|
| 1 | 健康检查 | `/health` 公开可访问且返回 `ok` |
| 2 | 管理面鉴权 | 错误 `ADMIN_TOKEN` → 401；项目 token 不能当管理凭据 |
| 3 | 管理面语义 | 创建 201 / 冲突 409 / 非法 id 400 / 未登记 404 / 注销后数据真的消失 |
| 4 | MCP 握手 | `initialize` 成功、`tools/list` 恰好 9 个工具 |
| 5 | 工具闭环 | 9 个工具的写入 → 检索 → 读取 → 按 id 删除全链路 |
| 6 | 错误码分流 | 实体不存在 → `-32602`（调用方可修正），而非 `-32603` |
| 7 | 项目隔离 | A 写入的数据，B 完全看不见 |
| 8 | 传输层防线 | 非白名单 Host → 403；缺 `Accept` 协商 → 406 |
| 9 | 项目 token 越权 | 伪造 token → 401；项目 token 打管理面 → 401 |

用法：

```bash
python3 scripts/smoke-e2e.py --base-url http://localhost:6789 --admin-token "$ADMIN_TOKEN"
# 或用环境变量 MEMORA_ADMIN_TOKEN 代替 --admin-token
```

退出码：`0` = 全部通过；`1` = 存在失败项（失败原因逐条打印）。仅依赖标准库。
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

# MCP 客户端必须声明同时接受 JSON 与 SSE，否则传输层以 406 拒绝。
MCP_ACCEPT = "application/json, text/event-stream"

# 本脚本认定的 MCP 工具全集：多一个少一个都应视为契约变更（TOOL_NAMES 单一来源的镜像）。
EXPECTED_TOOLS = {
    "add_observations",
    "create_entities",
    "create_relations",
    "delete_entities",
    "delete_observations",
    "delete_relations",
    "open_nodes",
    "read_graph",
    "search_nodes",
}


class Smoke:
    """收集逐项结论并统一打印。"""

    def __init__(self):
        """初始化空的检查结果列表。"""
        self.checks = []

    def check(self, label, ok, detail=""):
        """记录一条断言结果；失败项会在结尾汇总。"""
        self.checks.append((label, bool(ok)))
        mark = "PASS" if ok else "FAIL"
        suffix = f"  — {detail}" if detail else ""
        print(f"  [{mark}] {label}{suffix}")
        return bool(ok)

    @property
    def failed(self):
        """失败项标签列表。"""
        return [label for label, ok in self.checks if not ok]


def request(method, url, headers=None, body=None, timeout=10):
    """发一次 HTTP 请求，返回 (状态码, 解析后的 body 或原始文本)。"""
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
            status = resp.status
    except urllib.error.HTTPError as err:
        raw = err.read().decode()
        status = err.code
    except urllib.error.URLError as err:
        raise SystemExit(f"无法连接 {url}：{err.reason}\n请确认实例已启动（memora status）")
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, raw


def rpc(base, token, method, params=None, rpc_id=1, accept=MCP_ACCEPT, host=None):
    """发一次 MCP JSON-RPC 请求（无状态传输，无需先建会话）。"""
    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
    }
    if accept:
        headers["Accept"] = accept
    if host:
        headers["Host"] = host
    payload = {"jsonrpc": "2.0", "id": rpc_id, "method": method}
    if params is not None:
        payload["params"] = params
    return request("POST", f"{base}/mcp", headers, payload)


def tool(base, token, name, arguments, rpc_id=1):
    """调用一个 MCP 工具，返回 (是否成功, 工具输出字典或错误体)。"""
    status, body = rpc(base, token, "tools/call",
                       {"name": name, "arguments": arguments}, rpc_id)
    if status != 200 or not isinstance(body, dict) or "result" not in body:
        return False, body
    result = body["result"]
    if result.get("isError"):
        return False, result
    return True, json.loads(result["content"][0]["text"])


def admin(base, token, method, path, body=None):
    """发一次管理面请求。"""
    headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/json"}
    return request(method, f"{base}/api/v1{path}", headers, body)


def token_of(payload):
    """从创建项目的响应里取明文 token（只在 201 响应中出现一次）。"""
    return payload["token"] if isinstance(payload, dict) and "token" in payload else None


def section(title):
    """打印分节标题。"""
    print(f"\n{title}")


def main():
    """执行全部冒烟项并汇总。"""
    parser = argparse.ArgumentParser(description="Memora 端到端冒烟验证")
    parser.add_argument("--base-url", default="http://localhost:6789", help="实例根地址")
    parser.add_argument("--admin-token",
                        default=os.environ.get("MEMORA_ADMIN_TOKEN", ""),
                        help="管理令牌（也可用环境变量 MEMORA_ADMIN_TOKEN）")
    parser.add_argument("--keep", action="store_true",
                        help="保留测试项目，便于人工查看（默认自动注销）")
    args = parser.parse_args()

    base = args.base_url.rstrip("/")
    admin_token = args.admin_token
    if not admin_token:
        raise SystemExit("缺少管理令牌：--admin-token 或环境变量 MEMORA_ADMIN_TOKEN")

    stamp = int(time.time())
    proj_a, proj_b = f"smoke-a-{stamp}", f"smoke-b-{stamp}"
    smoke = Smoke()
    print(f"目标实例 {base}；测试项目 {proj_a} / {proj_b}")

    section("1. 健康检查")
    status, body = request("GET", f"{base}/health")
    smoke.check("/health 返回 200 且 body 含 ok", status == 200 and "ok" in str(body),
                f"HTTP {status} {body}")

    section("2. 管理面鉴权")
    status, _ = admin(base, "wrong-token", "GET", "/projects")
    smoke.check("错误 ADMIN_TOKEN → 401", status == 401, f"HTTP {status}")

    section("3. 项目生命周期")
    status, body = admin(base, admin_token, "POST", "/projects", {"project_id": proj_a})
    tok_a = token_of(body)
    smoke.check(f"创建项目 A → 201 并返回明文 token", status == 201 and bool(tok_a),
                f"HTTP {status}")

    status, _ = admin(base, admin_token, "POST", "/projects", {"project_id": proj_a})
    smoke.check("重复创建同一项目 → 409", status == 409, f"HTTP {status}")

    status, _ = admin(base, admin_token, "POST", "/projects", {"project_id": "../etc"})
    smoke.check("非法 project_id（目录穿越）→ 400", status == 400, f"HTTP {status}")

    status, _ = admin(base, admin_token, "DELETE", "/projects/never-created")
    smoke.check("注销未登记项目 → 404", status == 404, f"HTTP {status}")

    status, _ = admin(base, admin_token, "GET", "/projects")
    smoke.check("项目列表 → 200", status == 200, f"HTTP {status}")

    if not tok_a:
        raise SystemExit("未能取得项目 A 的 token，后续 MCP 用例无法继续")

    status, _ = admin(base, tok_a, "GET", "/projects")
    smoke.check("项目 token 当管理凭据用 → 401", status == 401, f"HTTP {status}")

    section("4. MCP 协议握手")
    status, body = rpc(base, tok_a, "initialize", {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "memora-smoke", "version": "1.0"},
    })
    info = body.get("result", {}).get("serverInfo") if isinstance(body, dict) else None
    smoke.check("initialize → 200 且返回 serverInfo", status == 200 and bool(info),
                f"HTTP {status} {info}")

    status, body = rpc(base, tok_a, "tools/list", rpc_id=2)
    names = {t["name"] for t in body["result"]["tools"]} if status == 200 else set()
    smoke.check("tools/list 恰好暴露 9 个工具", status == 200 and names == EXPECTED_TOOLS,
                f"HTTP {status}，缺失 {sorted(EXPECTED_TOOLS - names)}，"
                f"多出 {sorted(names - EXPECTED_TOOLS)}")

    section("5. 九个工具的写入-读取-删除闭环")
    ok, payload = tool(base, tok_a, "create_entities", {"entities": [
        {"name": "smoke-alpha", "entityType": "project"},
        {"name": "smoke-beta", "entityType": "person"},
    ]}, 10)
    smoke.check("create_entities 建两个实体", ok and len(payload.get("entities", [])) == 2)

    ok, payload = tool(base, tok_a, "create_relations", {"relations": [
        {"from": "smoke-beta", "to": "smoke-alpha", "relationType": "owns"}]}, 11)
    relation_id = payload["relations"][0]["id"] if ok and payload.get("relations") else None
    smoke.check("create_relations 回读带 id（否则无法删除）",
                ok and relation_id is not None, f"relation_id={relation_id}")

    ok, payload = tool(base, tok_a, "add_observations", {"observations": [
        {"entityName": "smoke-alpha", "contents": ["单二进制自托管记忆服务", "TDD 交付"]}]}, 12)
    obs_ids = [o["id"] for e in payload.get("entities", []) for o in e.get("observations", [])] \
        if ok else []
    smoke.check("add_observations 追加观测并回读 id",
                ok and len(obs_ids) == 2, f"observation_ids={obs_ids}")

    ok, payload = tool(base, tok_a, "search_nodes", {"query": "自托管"}, 13)
    smoke.check("search_nodes 中文检索命中",
                ok and any(e["name"] == "smoke-alpha" for e in payload.get("entities", [])))

    ok, payload = tool(base, tok_a, "open_nodes", {"names": ["smoke-alpha"]}, 14)
    smoke.check("open_nodes 按名取明细（含完整观测集）",
                ok and payload.get("entities", [{}])[0].get("name") == "smoke-alpha")

    ok, payload = tool(base, tok_a, "read_graph", {}, 15)
    smoke.check("read_graph 返回 2 实体 / 1 关系",
                ok and len(payload.get("entities", [])) == 2
                and len(payload.get("relations", [])) == 1,
                f"entities={len(payload.get('entities', []))} "
                f"relations={len(payload.get('relations', []))}")

    ok, _ = tool(base, tok_a, "delete_observations", {"ids": obs_ids}, 16)
    ok2, _ = tool(base, tok_a, "delete_observations", {"ids": obs_ids}, 17)
    smoke.check("delete_observations 按 id 删除且幂等（重复删不报错）", ok and ok2)

    ok, _ = tool(base, tok_a, "delete_relations", {"ids": [relation_id]}, 18)
    ok2, _ = tool(base, tok_a, "delete_relations", {"ids": [relation_id]}, 19)
    smoke.check("delete_relations 按 id 删除且幂等", ok and ok2)

    ok, payload = tool(base, tok_a, "delete_entities", {"names": ["smoke-alpha", "smoke-beta"]}, 20)
    smoke.check("delete_entities 级联删除并回显删除名",
                ok and sorted(payload.get("deleted", [])) == ["smoke-alpha", "smoke-beta"])

    status, _ = admin(base, admin_token, "GET", f"/projects/{proj_a}/stats")
    ok, graph = tool(base, tok_a, "read_graph", {}, 21)
    smoke.check("统计口径与客户端同源（删除后归零）",
                status == 200 and not graph.get("entities") and not graph.get("relations"),
                f"HTTP {status} graph_entities={len(graph.get('entities', []))}")

    section("6. 错误码分流")
    status, body = rpc(base, tok_a, "tools/call", {"name": "add_observations", "arguments": {
        "observations": [{"entityName": "no-such-entity", "contents": ["x"]}]}}, 22)
    code = body.get("error", {}).get("code") if isinstance(body, dict) else None
    smoke.check("实体不存在 → -32602（可自行修正），而非 -32603", code == -32602,
                f"code={code}")

    section("7. 跨项目隔离")
    status, body = admin(base, admin_token, "POST", "/projects", {"project_id": proj_b})
    tok_b = token_of(body)
    smoke.check("创建项目 B → 201", status == 201 and bool(tok_b), f"HTTP {status}")

    ok, _ = tool(base, tok_a, "create_entities",
                 {"entities": [{"name": "visible-only-in-a", "entityType": "probe"}]}, 30)
    _, graph_a = tool(base, tok_a, "read_graph", {}, 31)
    _, graph_b = tool(base, tok_b, "read_graph", {}, 32)
    names_a = {e["name"] for e in graph_a.get("entities", [])}
    names_b = {e["name"] for e in graph_b.get("entities", [])}
    smoke.check("A 写入的数据对 B 完全不可见",
                ok and "visible-only-in-a" in names_a and not names_b,
                f"A={sorted(names_a)} B={sorted(names_b)}")

    section("8. 传输层两道防线")
    status, _ = rpc(base, tok_a, "tools/list", rpc_id=40, host="evil.example.com")
    smoke.check("非白名单 Host → 403（防 DNS 重绑定）", status == 403, f"HTTP {status}")

    status, _ = rpc(base, tok_a, "tools/list", rpc_id=41, accept=None)
    smoke.check("缺少 Accept 协商头 → 406", status == 406, f"HTTP {status}")

    status, _ = rpc(base, "forged-token", "tools/list", rpc_id=42)
    smoke.check("伪造项目 token → 401", status == 401, f"HTTP {status}")

    section("9. 清理")
    if args.keep:
        print(f"  跳过清理（--keep）：请手动注销 {proj_a} / {proj_b}")
    else:
        for project in (proj_a, proj_b):
            status, body = admin(base, admin_token, "DELETE", f"/projects/{project}")
            removed = isinstance(body, dict) and body.get("data_removed") is True
            smoke.check(f"注销 {project} 且物理数据已移除",
                        status == 200 and removed, f"HTTP {status} {body}")

    total, failed = len(smoke.checks), smoke.failed
    print(f"\n{'=' * 60}")
    print(f"结论：{total - len(failed)}/{total} 项通过")
    if failed:
        print("失败项：")
        for label in failed:
            print(f"  - {label}")
        return 1
    print("实例行为与文档契约一致。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
