---
name: memora-handoff
description: Use when 涉及跨会话 / 跨机器的任务交接：在工作检查点用 handoff_save 留下交接，用 handoff_list / handoff_load / handoff_ack 接手他人的交接，或存在多条待接手、需要请用户挑选时。
---

## 保存：写一条别人接得住的交接

接手的人只能看到一行摘要，你的完整上下文对他不存在。

**`goal` 回答「这项工作是什么」**，让陌生人三秒判断要不要接：

| ✗ 写了你做了什么 | ✓ 写这项工作是什么 |
|---|---|
| 改了 repo.rs 和 contract.rs | 把 handoff 并入 memora：契约 H1–H18 已全过，待实现 CLI |

**`state` 第一行写精确卡点或进度**——列表摘要展示的就是这一行：

| ✗ 模糊 | ✓ 可定位 |
|---|---|
| 今天做了很多 | 卡在 h09：sqlite_file 的 drop_project 未先摘连接缓存就删目录 |

**`next_steps` 让零记忆的人直接开工**——文件写到路径、命令写全、验收标准写明：

```json
[
  "CARGO_TARGET_DIR=/tmp/memora-target cargo test h09",
  "仍红则查 src/storage/sqlite_file.rs 的 drop_project：先摘连接缓存再 remove_dir_all"
]
```

**`session_id` 稳定可区分**（如 `zed-20261014-2130`），他人才能在列表里认出
「这是哪个会话留下的」。

**检查点才保存**：一个子任务完成、一个改变方向的决策落定、即将开跑长任务。
不是每次工具调用之后。

## 接手：呈现候选给用户挑

多条待接手时，渲染编号表格（goal、state 摘录、client、会话标签、时间），
请用户报编号挑选。**绝不要求用户念 task_id**——零 ID 主路径的设计意义就在于此；
目标唯一确定后，才把 task_id 传给 load / ack。

> 本文件是 Memora 交接的**可选质量层**：判方向所需的基础规则已随 MCP
> `instructions` 在连接时下发。删掉本文件，功能照常可用，只是交接质量靠自觉。
> 设计见 `docs/handoff-integration-design.md` §5.6。
