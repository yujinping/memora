<script setup lang="ts">
/**
 * 记忆浏览：列表 + 详情。
 *
 * @author yujinping
 * @intent 布局是「左列表 / 右详情」而不是「表格 + 弹窗」：记忆的详情
 *         （实体 + 全部观测 + 关联关系）体量差异很大，弹窗要么撑爆屏幕、
 *         要么强制内部滚动。分栏让详情区可以独立滚动，列表保持在视野内。
 *
 * @intent 数据来自 `read_graph`（一次取全图）而不是逐条查询：与 MCP 的
 *         `read_graph` 同一个仓库方法，因此控制台看到的与 AI 客户端写入的
 *         必然是同一份数据，不存在「两套读取逻辑互相漂移」。
 *         代价是全图一次性载入内存——对自托管的记忆库规模（千级实体）可忽略。
 *
 * @intent 过滤在**前端**做而不是每敲一个字打一次接口：全图已经在手，
 *         本地过滤是零延迟的；而按关键字请求会有「输入过程中的 404 / 空结果闪烁」。
 */
import { computed, onMounted, ref, watch } from "vue";
import { useRoute } from "vue-router";
import { getGraph, type Entity, type Graph, type Relation } from "../lib/api";
import { formatTime } from "../lib/format";

const route = useRoute();

/** 当前项目标识（来自路由参数）。 */
const projectId = computed(() => String(route.params.projectId ?? ""));

const graph = ref<Graph | null>(null);
const loading = ref(true);
const loadError = ref("");

/** 关键字（实体名 / 类型 / 观测内容）。 */
const keyword = ref("");
/** 选中的实体名；`null` 表示未选中。 */
const selectedName = ref<string | null>(null);
/** 详情区标签页：记忆（观测）或关系。 */
const detailTab = ref<"observations" | "relations">("observations");

/** 按关键字过滤后的实体。 */
const filteredEntities = computed<Entity[]>(() => {
  const all = graph.value?.entities ?? [];
  const needle = keyword.value.trim().toLowerCase();
  if (!needle) return all;
  return all.filter((entity) => {
    if (entity.name.toLowerCase().includes(needle)) return true;
    if (entity.entity_type.toLowerCase().includes(needle)) return true;
    if (entity.source.toLowerCase().includes(needle)) return true;
    return entity.observations.some((obs) =>
      obs.content.toLowerCase().includes(needle),
    );
  });
});

/** 当前选中的实体。 */
const selected = computed<Entity | null>(() => {
  if (!selectedName.value) return null;
  const all = graph.value?.entities ?? [];
  return all.find((e) => e.name === selectedName.value) ?? null;
});

/** 与选中实体相关的关系（无论方向）。 */
const relatedRelations = computed<Relation[]>(() => {
  const name = selectedName.value;
  if (!name) return [];
  const all = graph.value?.relations ?? [];
  return all.filter((r) => r.from_name === name || r.to_name === name);
});

/** 关系列表（在「关系」标签页里展示全部）。 */
const allRelations = computed<Relation[]>(() => {
  const all = graph.value?.relations ?? [];
  const needle = keyword.value.trim().toLowerCase();
  if (!needle) return all;
  return all.filter(
    (r) =>
      r.from_name.toLowerCase().includes(needle) ||
      r.to_name.toLowerCase().includes(needle) ||
      r.relation_type.toLowerCase().includes(needle),
  );
});

/** 观测总数（用于页头）。 */
const observationTotal = computed(() =>
  (graph.value?.entities ?? []).reduce(
    (sum, entity) => sum + entity.observations.length,
    0,
  ),
);

async function load(): Promise<void> {
  loading.value = true;
  loadError.value = "";
  try {
    graph.value = await getGraph(projectId.value);
    // 载入后不自动选中：自动选中第一条会让「刚打开就有一屏内容」
    // 掩盖了「列表里还有别的」这一事实，而用户往往需要先扫一眼列表。
    selectedName.value = null;
  } catch (err) {
    loadError.value = err instanceof Error ? err.message : String(err);
    graph.value = null;
  } finally {
    loading.value = false;
  }
}

/** 选中一个实体，并把详情切回观测页。 */
function select(entity: Entity): void {
  selectedName.value = entity.name;
  detailTab.value = "observations";
}

// 路由参数变化（在项目间切换）时重新取数；同一组件被复用，故必须显式 watch。
watch(projectId, () => {
  keyword.value = "";
  selectedName.value = null;
  void load();
});

onMounted(load);
</script>

<template>
  <div class="flex h-full min-h-0 flex-col">
    <div class="mb-4 flex flex-wrap items-end justify-between gap-3">
      <div>
        <div class="flex items-center gap-2">
          <RouterLink
            to="/projects"
            class="text-xs text-faint transition-colors hover:text-ink"
          >
            ← 项目
          </RouterLink>
        </div>
        <h1 class="mt-1 text-lg font-semibold">
          记忆
          <span class="mono ml-1 text-sm font-normal text-brand">{{
            projectId
          }}</span>
        </h1>
        <p v-if="graph" class="mt-1 text-xs text-muted">
          {{ graph.entities.length }} 个实体 ·
          {{ observationTotal }} 条观测 ·
          {{ graph.relations.length }} 条关系
        </p>
      </div>
      <div class="flex items-center gap-2">
        <input
          v-model="keyword"
          type="search"
          class="w-56 rounded-md border border-line bg-surface-2 px-3 py-1.5 text-xs placeholder:text-faint"
          placeholder="按名称 / 类型 / 内容过滤…"
        />
        <button
          class="rounded-md border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink disabled:opacity-50"
          :disabled="loading"
          @click="load"
        >
          {{ loading ? "刷新中…" : "刷新" }}
        </button>
      </div>
    </div>

    <p v-if="loadError" class="rounded-lg border border-danger/40 bg-surface px-4 py-3 text-sm text-danger">
      加载失败：{{ loadError }}
    </p>

    <p
      v-else-if="loading"
      class="rounded-lg border border-line bg-surface px-4 py-6 text-sm text-muted"
    >
      加载中…
    </p>

    <div
      v-else-if="graph && graph.entities.length === 0 && graph.relations.length === 0"
      class="rounded-lg border border-line bg-surface px-4 py-10 text-center text-sm text-muted"
    >
      这个项目还没有记忆。让 AI 客户端通过 MCP 写入后即可在此查看。
    </div>

    <div v-else class="grid min-h-0 flex-1 gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.25fr)]">
      <!-- 列表 -->
      <section class="flex min-h-0 flex-col rounded-lg border border-line bg-surface">
        <div class="border-b border-line px-3 py-2 text-xs text-faint">
          实体 {{ filteredEntities.length }} / {{ graph?.entities.length ?? 0 }}
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto">
          <p
            v-if="filteredEntities.length === 0"
            class="px-3 py-6 text-center text-xs text-muted"
          >
            没有匹配的实体。
          </p>
          <button
            v-for="entity in filteredEntities"
            :key="entity.name"
            class="block w-full border-b border-line/50 px-3 py-2.5 text-left transition-colors last:border-0 hover:bg-surface-2"
            :class="selectedName === entity.name ? 'bg-surface-2' : ''"
            @click="select(entity)"
          >
            <div class="flex items-baseline justify-between gap-2">
              <span class="mono truncate text-sm">{{ entity.name }}</span>
              <span
                class="shrink-0 rounded border border-line bg-surface-3 px-1.5 py-0.5 text-[10px] text-muted"
              >
                {{ entity.entity_type || "unknown" }}
              </span>
            </div>
            <div class="mt-1 flex items-center gap-2 text-[11px] text-faint">
              <span>{{ entity.observations.length }} 条观测</span>
              <span v-if="entity.source" class="truncate">· {{ entity.source }}</span>
            </div>
          </button>
        </div>
      </section>

      <!-- 详情 -->
      <section class="flex min-h-0 flex-col rounded-lg border border-line bg-surface">
        <template v-if="selected">
          <div class="border-b border-line px-4 py-3">
            <h2 class="mono break-all text-sm font-semibold">{{ selected.name }}</h2>
            <p class="mt-1 text-[11px] text-faint">
              {{ selected.entity_type || "unknown" }} · 创建于
              {{ formatTime(selected.created_at) }}
              <span v-if="selected.source"> · 来源 {{ selected.source }}</span>
            </p>
            <div class="mt-2 flex gap-1">
              <button
                class="rounded px-2 py-1 text-[11px] transition-colors"
                :class="
                  detailTab === 'observations'
                    ? 'bg-surface-3 text-ink'
                    : 'text-muted hover:text-ink'
                "
                @click="detailTab = 'observations'"
              >
                观测 {{ selected.observations.length }}
              </button>
              <button
                class="rounded px-2 py-1 text-[11px] transition-colors"
                :class="
                  detailTab === 'relations'
                    ? 'bg-surface-3 text-ink'
                    : 'text-muted hover:text-ink'
                "
                @click="detailTab = 'relations'"
              >
                关系 {{ relatedRelations.length }}
              </button>
            </div>
          </div>

          <div class="min-h-0 flex-1 overflow-y-auto px-4 py-3">
            <!-- 观测 -->
            <template v-if="detailTab === 'observations'">
              <p v-if="selected.observations.length === 0" class="text-xs text-muted">
                （无观测）
              </p>
              <ol v-else class="space-y-3">
                <li
                  v-for="obs in selected.observations"
                  :key="obs.id"
                  class="rounded-md border border-line bg-surface-2 px-3 py-2"
                >
                  <div class="mb-1 flex items-center gap-2 text-[10px] text-faint">
                    <span class="mono">#{{ obs.id }}</span>
                    <span>{{ formatTime(obs.created_at) }}</span>
                    <span v-if="obs.source">· {{ obs.source }}</span>
                  </div>
                  <!-- 观测内容常是分点写的，故用 pre-wrap 保留换行与缩进；
                       不用 v-html 拼 `<br>`：内容是 AI 写入的不可信数据，
                       任何解析 HTML 的写法都等于在管理界面里开一个存储型 XSS。 -->
                  <div class="whitespace-pre-wrap break-words text-sm leading-relaxed">{{
                    obs.content
                  }}</div>
                </li>
              </ol>
            </template>

            <!-- 关系 -->
            <template v-else>
              <p v-if="relatedRelations.length === 0" class="text-xs text-muted">
                该实体没有关联关系。
              </p>
              <ul v-else class="space-y-1.5">
                <li
                  v-for="relation in relatedRelations"
                  :key="relation.id"
                  class="flex flex-wrap items-baseline gap-1.5 rounded-md border border-line bg-surface-2 px-3 py-2 text-xs"
                >
                  <span
                    class="mono"
                    :class="relation.from_name === selected.name ? 'text-ink' : 'text-brand'"
                  >
                    {{ relation.from_name }}
                  </span>
                  <span class="text-faint">—{{ relation.relation_type }}→</span>
                  <span
                    class="mono"
                    :class="relation.to_name === selected.name ? 'text-ink' : 'text-brand'"
                  >
                    {{ relation.to_name }}
                  </span>
                  <span class="ml-auto mono text-[10px] text-faint">#{{ relation.id }}</span>
                </li>
              </ul>
            </template>
          </div>
        </template>

        <!-- 未选中：给出全局关系视图，而不是一片空白 -->
        <template v-else>
          <div class="border-b border-line px-4 py-3">
            <h2 class="text-sm font-semibold">关系</h2>
            <p class="mt-1 text-[11px] text-faint">
              选中左侧实体可查看其观测；以下是本项目的全部关系。
            </p>
          </div>
          <div class="min-h-0 flex-1 overflow-y-auto px-4 py-3">
            <p v-if="allRelations.length === 0" class="text-xs text-muted">
              还没有关系。让 AI 客户端写入后即可在此查看。
            </p>
            <ul v-else class="space-y-1.5">
              <li
                v-for="relation in allRelations"
                :key="relation.id"
                class="flex flex-wrap items-baseline gap-1.5 rounded-md border border-line bg-surface-2 px-3 py-2 text-xs"
              >
                <button
                  class="mono text-brand hover:underline"
                  @click="selectedName = relation.from_name"
                >
                  {{ relation.from_name }}
                </button>
                <span class="text-faint">—{{ relation.relation_type }}→</span>
                <button
                  class="mono text-brand hover:underline"
                  @click="selectedName = relation.to_name"
                >
                  {{ relation.to_name }}
                </button>
                <span class="ml-auto mono text-[10px] text-faint">#{{ relation.id }}</span>
              </li>
            </ul>
          </div>
        </template>
      </section>
    </div>
  </div>
</template>