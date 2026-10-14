---
name: memora-handoff
description: Use when writing or reviewing a handoff at a work checkpoint, or when presenting several pending handoffs for the user to choose from, to make the entries recognizable and the choice easy.
---

- **Make the entry recognizable.** The next person reads a one-line list, not your
  full context. Write `goal` as what the work IS ("把 handoff 并入 memora"), not
  what you did ("改了 repo.rs")。Put the blocking detail in the first line of
  `state` — that is the excerpt shown in the list.
- **`next_steps` must be executable by someone with no memory of this session**:
  name files by path, name the command to run, name the check that proves it worked.
- **Prefer a few concrete steps over a summary.** Three actionable steps beat ten
  vague ones; the reader wants to start, not to review.
- **Pass a stable `session_id`** (e.g. `zed-20261014-2130`) so the entry is
  distinguishable from your other sessions in someone else's list.
- **Presenting candidates**: show a numbered table with goal, the state excerpt,
  client, session label and time; ask the user to pick a number. Never make the user
  read a task_id out loud.
- **A meaningful checkpoint** is: a sub-task finished, a decision made that changes
  direction, or you are about to run something long. Not after every tool call.

> 这是 Memora 交接功能的**可选质量层**：判方向所需的基础规则在 MCP `instructions`
> 里随连接下发（连接即得）。删掉本文件，交接功能完全可用——只是写得糙。
> 见 `docs/handoff-integration-design.md` §5.6。
