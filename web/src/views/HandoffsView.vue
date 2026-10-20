<script setup lang="ts">
/**
 * 交接浏览：列表 + 详情。
 *
 * @author yujinping
 * @intent 「待接手」与「全部历史」是同一个接口的两个参数（`unacked`），
 *         而不是两个接口：交接的语义是「最新修订」，历史视图只是不过滤
 *         已接手的条目。两套接口迟早会出现排序口径不一致。
 *
 * @intent 详情必须在**点击时**单独取（`/handoffs/{task_id}`），而不是从列表
 *         数据里拼出来：列表接口返回的是**摘要**（`state_excerpt` 被截断、
 *         不含 `next_steps` / `decisions` / `open_questions`）。用摘要冒充详情
 *         会让用户以为「交接里只写了这么点」——这是旧手写控制台的一个真实缺陷。
 */
import { computed, onMounted, ref, watch } from "vue";
import { useRoute } from "vue-router";
import {
  getHandoff,
  listHandoffs,
  type Handoff,
  type HandoffSummary,
} from "../lib/api";
import { formatRelative, formatTime } from "../lib/format";

const route = useRoute();

/** 当前项目标识。 */
const projectId = computed(() => String(route.params.projectId ?? ""));

const items = ref<HandoffSummary[]>([]);
const loading = ref(true);
const loadError = ref("");

/** 只列待接手（默认）；关闭后看全部历史。 */
const unackedOnly = ref(true);
/** 前缀过滤。 */
const prefix = ref("");
/** 最多取多少条。 */
const limit = ref(50);

const selectedTaskId = ref<string | null>(null);
const detail = ref<Handoff | null>(null);
const detailLoading = ref(false);
const detailError = ref("");

/** 待接手条数（用于页头提示）。 */
const pendingCount = computed(
  () => items.value.filter((item) => !item.acked).length,
);

/**
 * 详情页的分块列表。
 *
 * @intent 提成 `computed` 而非在模板里写内联数组字面量：内联数组每次渲染都会
 *         重建，且其类型被推断成「部分成员才有 `mono`」的联合，模板里访问
 *         `block.mono` 会直接报 TS2339。显式标注 `mono?: boolean` 后，
 *         模板读到的就是稳定类型。
 */
const detailBlocks = computed<
  { key: string; title: string; items: string[]; mono?: boolean }[]
>(() => {
  const d = detail.value;
  if (!d) return [];
  return [
    { key: "next_steps", title: "下一步", items: d.next_steps },
    { key: "files", title: "关键文件 / 位置", items: d.files, mono: true },
    { key: "decisions", title: "已定事项", items: d.decisions },
    { key: "open_questions", title: "待拍板", items: d.open_questions },
  ];
});

async function load(): Promise<void> {
  loading.value = true;
  loadError.value = "";
  try {
    const result = await listHandoffs(projectId.value, {
      unacked: unackedOnly.value,
      limit: limit.value,
      ...(prefix.value.trim() ? { prefix: prefix.value.trim() } : {}),
    });
    items.value = result.handoffs;
    // 原先选中的条目可能已不在新列表里（被接手 / 被删除）：保留详情，
    // 但清掉选中态，避免列表与详情看起来对应但实际不相关。
    if (
      selectedTaskId.value &&
      !items.value.some((item) => item.task_id === selectedTaskId.value)
    ) {
      selectedTaskId.value = null;
      detail.value = null;
    }
  } catch (err) {
    loadError.value = err instanceof Error ? err.message : String(err);
    items.value = [];
  } finally {
    loading.value = false;
  }
}

async function open(taskId: string): Promise<void> {
  selectedTaskId.value = taskId;
  detailLoading.value = true;
  detailError.value = "";
  detail.value = null;
  try {
    detail.value = await getHandoff(projectId.value, taskId);
  } catch (err) {
    detailError.value = err instanceof Error ? err.message : String(err);
  } finally {
    detailLoading.value = false;
  }
}

/** 列表项上的状态标签。 */
function statusLabel(item: HandoffSummary): string {
  return item.acked ? "已接手" : "待接手";
}

watch([unackedOnly, prefix, limit], () => {
  void load();
});

watch(projectId, () => {
  selectedTaskId.value = null;
  detail.value = null;
  void load();
});

onMounted(load);
</script>

<template>
  <div class="flex h-full min-h-0 flex-col">
    <div class="mb-4 flex flex-wrap items-end justify-between gap-3">
      <div>
        <RouterLink
          to="/projects"
          class="text-xs text-faint transition-colors hover:text-ink"
        >
          ← 项目
        </RouterLink>
        <h1 class="mt-1 text-lg font-semibold">
          交接
          <span class="mono ml-1 text-sm font-normal text-brand">{{
            projectId
          }}</span>
        </h1>
        <p class="mt-1 text-xs text-muted">
          跨客户端 / 跨机器的工作接力：目标、进展、下一步、已定事项与待拍板问题。
          <span v-if="pendingCount > 0" class="text-warn">
            当前视图中有 {{ pendingCount }} 条待接手。
          </span>
        </p>
      </div>

      <div class="flex flex-wrap items-center gap-2">
        <input
          v-model="prefix"
          type="search"
          class="w-40 rounded-md border border-line bg-surface-2 px-3 py-1.5 text-xs placeholder:text-faint"
          placeholder="task_id 前缀"
        />
        <select
          v-model.number="limit"
          class="rounded-md border border-line bg-surface-2 px-2 py-1.5 text-xs"
        >
          <option :value="20">20 条</option>
          <option :value="50">50 条</option>
          <option :value="200">200 条</option>
        </select>
        <label
          class="flex cursor-pointer items-center gap-1.5 rounded-md border border-line bg-surface-2 px-2.5 py-1.5 text-xs text-muted"
        >
          <input v-model="unackedOnly" type="checkbox" class="accent-[var(--color-brand)]" />
          只看待接手
        </label>
        <button
          class="rounded-md border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink disabled:opacity-50"
          :disabled="loading"
          @click="load"
        >
          {{ loading ? "刷新中…" : "刷新" }}
        </button>
      </div>
    </div>

    <p
      v-if="loadError"
      class="rounded-lg border border-danger/40 bg-surface px-4 py-3 text-sm text-danger"
    >
      加载失败：{{ loadError }}
    </p>

    <p
      v-else-if="loading"
      class="rounded-lg border border-line bg-surface px-4 py-6 text-sm text-muted"
    >
      加载中…
    </p>

    <div v-else class="grid min-h-0 flex-1 gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)]">
      <!-- 列表 -->
      <section class="flex min-h-0 flex-col rounded-lg border border-line bg-surface">
        <div class="border-b border-line px-3 py-2 text-xs text-faint">
          共 {{ items.length }} 条
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto">
          <p
            v-if="items.length === 0"
            class="px-3 py-8 text-center text-xs text-muted"
          >
            {{
              unackedOnly
                ? "没有待接手的交接。取消「只看待接手」可查看历史。"
                : "这个项目还没有交接记录。"
            }}
          </p>
          <button
            v-for="item in items"
            :key="item.task_id"
            class="block w-full border-b border-line/50 px-3 py-2.5 text-left transition-colors last:border-0 hover:bg-surface-2"
            :class="selectedTaskId === item.task_id ? 'bg-surface-2' : ''"
            @click="open(item.task_id)"
          >
            <div class="flex items-baseline justify-between gap-2">
              <span class="mono truncate text-xs text-brand">{{ item.task_id }}</span>
              <span
                class="shrink-0 rounded border px-1.5 py-0.5 text-[10px]"
                :class="
                  item.acked
                    ? 'border-line bg-surface-3 text-faint'
                    : 'border-warn/50 bg-warn/10 text-warn'
                "
              >
                {{ statusLabel(item) }}
              </span>
            </div>
            <p class="mt-1 truncate text-sm">{{ item.goal }}</p>
            <p class="mt-0.5 truncate text-[11px] text-faint">
              {{ item.state_excerpt || "（无进展描述）" }}
            </p>
            <div class="mt-1 flex flex-wrap items-center gap-2 text-[10px] text-faint">
              <span>rev {{ item.revision }}</span>
              <span v-if="item.client">· {{ item.client }}</span>
              <span v-if="item.session_id" class="mono">· {{ item.session_id }}</span>
              <span>· {{ item.next_step_count }} 步待办</span>
              <span class="ml-auto">{{ formatRelative(item.created_at) }}</span>
            </div>
          </button>
        </div>
      </section>

      <!-- 详情 -->
      <section class="flex min-h-0 flex-col rounded-lg border border-line bg-surface">
        <div v-if="detailLoading" class="px-4 py-6 text-sm text-muted">加载详情…</div>

        <div v-else-if="detailError" class="px-4 py-6 text-sm text-danger">
          详情加载失败：{{ detailError }}
        </div>

        <div
          v-else-if="!detail"
          class="flex flex-1 items-center justify-center px-6 py-10 text-center text-xs text-muted"
        >
          从左侧选择一条交接查看完整内容（下一步、关键文件、已定事项、待拍板问题）。
        </div>

        <template v-else>
          <div class="border-b border-line px-4 py-3">
            <div class="flex flex-wrap items-baseline justify-between gap-2">
              <h2 class="mono break-all text-sm font-semibold text-brand">
                {{ detail.task_id }}
              </h2>
              <span
                class="rounded border px-1.5 py-0.5 text-[10px]"
                :class="
                  detail.acked_at
                    ? 'border-line bg-surface-3 text-faint'
                    : 'border-warn/50 bg-warn/10 text-warn'
                "
              >
                {{ detail.acked_at ? "已接手" : "待接手" }}
              </span>
            </div>
            <p class="mt-1 text-[11px] text-faint">
              rev {{ detail.revision }} · 创建于 {{ formatTime(detail.created_at) }}
              <span v-if="detail.client"> · 写入方 {{ detail.client }}</span>
              <span v-if="detail.session_id" class="mono">
                · 会话 {{ detail.session_id }}
              </span>
              <span v-if="detail.acked_at">
                · {{ formatTime(detail.acked_at) }} 由
                {{ detail.acked_by || "未声明" }} 接手
              </span>
            </p>
          </div>

          <div class="min-h-0 flex-1 space-y-4 overflow-y-auto px-4 py-3">
            <div>
              <h3 class="mb-1 text-[11px] font-medium uppercase tracking-wide text-faint">
                目标
              </h3>
              <p class="whitespace-pre-wrap break-words text-sm">{{ detail.goal }}</p>
            </div>

            <div>
              <h3 class="mb-1 text-[11px] font-medium uppercase tracking-wide text-faint">
                当前进展
              </h3>
              <p class="whitespace-pre-wrap break-words text-sm leading-relaxed">
                {{ detail.state || "（未填写）" }}
              </p>
            </div>

            <div v-for="block in detailBlocks" :key="block.key">
              <h3 class="mb-1 text-[11px] font-medium uppercase tracking-wide text-faint">
                {{ block.title }}
                <span class="normal-case tracking-normal">（{{ block.items.length }}）</span>
              </h3>
              <p v-if="block.items.length === 0" class="text-xs text-faint">
                无
              </p>
              <ul v-else class="space-y-1">
                <li
                  v-for="(entry, index) in block.items"
                  :key="index"
                  class="flex gap-2 text-sm leading-relaxed"
                >
                  <span class="select-none text-faint">·</span>
                  <span
                    class="min-w-0 whitespace-pre-wrap break-words"
                    :class="block.mono ? 'mono text-xs' : ''"
                  >
                    {{ entry }}
                  </span>
                </li>
              </ul>
            </div>
          </div>
        </template>
      </section>
    </div>
  </div>
</template>