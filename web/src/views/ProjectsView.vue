<script setup lang="ts">
/**
 * 项目页：创建项目、生成 token、查看列表、进入记忆 / 交接、轮换与注销。
 *
 * @author yujinping
 * @intent 统计数字按项目并发取（`/stats` 是按项目接口）：项目数量是人工量级，
 *         为此新增一个聚合接口不值得。单个项目的统计失败不影响整表渲染——
 *         后端未注册 / 数据不可读时显示 `—`，而不是让整个表格变成错误提示。
 *
 * @intent 轮换与注销是两个**语义完全不同**的动作，必须在界面上区分清楚：
 *         轮换只换凭据（数据保留），注销销毁数据。把它们放在同一个下拉里
 *         且不加说明，是那种「事后才意识到点错了」的设计。
 *
 * @intent 归属列只在管理员视图下有信息量（普通账号的项目全是自己的），
 *         但仍照常渲染：它让「我现在看到的是自己的还是别人的」这件事
 *         无需推理。管理员另会看到「无主」与「归属失效」两种异常标记——
 *         它们都是升级 / 删号之后只有管理员能处置的遗留数据。
 */
import { computed, onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import {
  createProject,
  deleteProject,
  getStats,
  listProjects,
  listUsers,
  rotateToken,
  type Project,
  type ProjectStats,
  type ProjectToken,
} from "../lib/api";
import { ApiError } from "../lib/api";
import { formatDate, formatRelative } from "../lib/format";
import { isAdmin } from "../lib/session";
import ConfirmDialog from "../components/ConfirmDialog.vue";
import Toast from "../components/Toast.vue";
import TokenPanel from "../components/TokenPanel.vue";

const router = useRouter();

/** 项目列表。 */
const projects = ref<Project[]>([]);
/** 每个项目的统计（按 project_id 索引）；`null` 表示取不到。 */
const stats = ref<Record<string, ProjectStats | null>>({});
/** 是否正在加载列表。 */
const loading = ref(true);
/** 列表加载失败的原因。 */
const loadError = ref("");

/** 现有账号名集合，用于识别「归属指向不存在账号」的项目（仅管理员取）。 */
const knownUsers = ref<Set<string>>(new Set());

/** 新建表单。 */
const newId = ref("");
const newBackend = ref("");
const creating = ref(false);

/** 当前展示的一次性 token（创建或轮换后）。 */
const issued = ref<{ data: ProjectToken; rotated: boolean } | null>(null);

/** 待确认的注销动作。 */
const pendingDelete = ref<Project | null>(null);
/** 待确认的轮换动作。 */
const pendingRotate = ref<Project | null>(null);

/** 底部提示。 */
const toast = ref<{ message: string; kind: "ok" | "err" } | null>(null);

/** 可选存储后端。与 `BackendKind` 一致；空值 = 用服务端默认。 */
const backends = [
  { value: "", label: "使用服务端默认" },
  { value: "sqlite_file", label: "sqlite_file（每项目一个库文件）" },
  { value: "sqlite_single", label: "sqlite_single（所有项目共用一个库文件）" },
];

function notify(message: string, kind: "ok" | "err" = "ok"): void {
  toast.value = { message, kind };
  window.setTimeout(() => {
    toast.value = null;
  }, kind === "err" ? 5200 : 2600);
}

/** 表格行：项目 + 统计。 */
interface Row {
  project: Project;
  stats: ProjectStats | null;
}

const rows = computed<Row[]>(() =>
  projects.value.map((project) => ({
    project,
    stats: stats.value[project.project_id] ?? null,
  })),
);

/**
 * 归属的显示形态。
 *
 * @intent 三种异常都归到同一列而不是新增列：`空` 是迁移中间态，
 *         `失效` 是账号被删后的遗留，两者都只对管理员可见（普通账号根本
 *         拿不到这些项目），处置动作也相同——所以合成一个单元格足够。
 */
function ownerLabel(project: Project): string {
  if (!project.owner) return "无主";
  return project.owner;
}

/** 归属是否已失效（账号已不存在）。 */
function ownerIsStale(project: Project): boolean {
  if (!project.owner || !isAdmin()) return false;
  return !knownUsers.value.has(project.owner);
}

/** 归属是否缺失（迁移前遗留）。 */
function ownerIsMissing(project: Project): boolean {
  return !project.owner;
}

/**
 * 加载列表与统计。
 *
 * @intent 统计用 `Promise.allSettled` 而不是 `Promise.all`：一个项目的
 *         后端未注册不该让整张表的数字全部消失。失败的项留 `null`，
 *         界面上显示 `—`（并带 title 说明原因）。
 *
 * @intent 账号名清单只有管理员能取（`/users` 对租户是 403）。取失败时
 *         只是不标「归属失效」，不影响主流程——那是一个提示性标记，
 *         不该因为辅取失败就让整页报错。
 */
async function load(): Promise<void> {
  loading.value = true;
  loadError.value = "";
  try {
    projects.value = await listProjects();
  } catch (err) {
    loadError.value = err instanceof Error ? err.message : String(err);
    projects.value = [];
    loading.value = false;
    return;
  }

  if (isAdmin()) {
    try {
      knownUsers.value = new Set((await listUsers()).map((u) => u.username));
    } catch {
      knownUsers.value = new Set();
    }
  }

  const settled = await Promise.allSettled(
    projects.value.map((p) => getStats(p.project_id)),
  );
  const next: Record<string, ProjectStats | null> = {};
  settled.forEach((result, index) => {
    const project = projects.value[index];
    if (!project) return;
    next[project.project_id] =
      result.status === "fulfilled" ? result.value : null;
  });
  stats.value = next;
  loading.value = false;
}

async function submitCreate(): Promise<void> {
  if (creating.value) return;
  creating.value = true;
  try {
    const body: { project_id?: string; backend?: string } = {};
    const id = newId.value.trim();
    if (id) body.project_id = id;
    if (newBackend.value) body.backend = newBackend.value;

    const created = await createProject(body);
    issued.value = { data: created, rotated: false };
    newId.value = "";
    await load();
    notify(`项目 ${created.project_id} 已创建`, "ok");
  } catch (err) {
    notify(err instanceof Error ? err.message : String(err), "err");
  } finally {
    creating.value = false;
  }
}

async function confirmRotate(): Promise<void> {
  const target = pendingRotate.value;
  pendingRotate.value = null;
  if (!target) return;
  try {
    const rotated = await rotateToken(target.project_id);
    issued.value = { data: rotated, rotated: true };
    notify(`项目 ${target.project_id} 的 token 已轮换，旧 token 已失效`, "ok");
  } catch (err) {
    notify(err instanceof ApiError ? err.message : String(err), "err");
  }
}

async function confirmDelete(): Promise<void> {
  const target = pendingDelete.value;
  pendingDelete.value = null;
  if (!target) return;
  try {
    const result = await deleteProject(target.project_id);
    if (result.data_removed === false) {
      notify(
        "登记已注销，但物理数据未能删除（后端不可用）——磁盘上可能仍有残留",
        "err",
      );
    } else {
      notify(`项目 ${target.project_id} 已注销`, "ok");
    }
    if (issued.value?.data.project_id === target.project_id) issued.value = null;
    await load();
  } catch (err) {
    notify(err instanceof Error ? err.message : String(err), "err");
  }
}

onMounted(load);
</script>

<template>
  <div>
    <div class="mb-5 flex flex-wrap items-end justify-between gap-3">
      <div>
        <h1 class="text-lg font-semibold">项目</h1>
        <p class="mt-1 text-xs text-muted">
          每个项目独立持有记忆与交接数据，项目之间硬隔离——各自的 token
          只能访问各自的库。
          <span v-if="isAdmin()">
            你是管理员，故这里列出<strong>全部账号</strong>的项目。
          </span>
          <span v-else>
            这里只列出你名下的项目；项目归属由创建者决定，创建后不能转让。
          </span>
        </p>
      </div>
      <button
        class="rounded-md border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink disabled:opacity-50"
        :disabled="loading"
        @click="load"
      >
        {{ loading ? "刷新中…" : "刷新" }}
      </button>
    </div>

    <!-- 一次性 token -->
    <TokenPanel
      v-if="issued"
      :project-id="issued.data.project_id"
      :token="issued.data.token"
      :rotated="issued.rotated"
      @close="issued = null"
    />

    <!-- 新建项目 -->
    <section class="mb-6 rounded-lg border border-line bg-surface p-4">
      <h2 class="mb-3 text-sm font-semibold">新建项目</h2>
      <form class="flex flex-wrap items-end gap-3" @submit.prevent="submitCreate">
        <div class="min-w-56 flex-1">
          <label for="new-id" class="mb-1.5 block text-xs text-muted">
            项目标识（留空自动生成，如
            <span class="mono">p_3f9a1c2b4d5e</span>）
          </label>
          <input
            id="new-id"
            v-model="newId"
            type="text"
            class="mono w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm placeholder:text-faint"
            placeholder="my-project — 仅限字母、数字、下划线、短横线"
          />
        </div>
        <div class="w-64">
          <label for="new-backend" class="mb-1.5 block text-xs text-muted"
            >存储后端</label
          >
          <select
            id="new-backend"
            v-model="newBackend"
            class="w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm"
          >
            <option v-for="b in backends" :key="b.value" :value="b.value">
              {{ b.label }}
            </option>
          </select>
        </div>
        <button
          type="submit"
          class="rounded-md bg-brand px-4 py-2 text-sm font-medium text-brand-ink transition-opacity hover:opacity-90 disabled:opacity-50"
          :disabled="creating"
        >
          {{ creating ? "创建中…" : "创建并生成 token" }}
        </button>
      </form>
    </section>

    <!-- 列表 -->
    <section class="rounded-lg border border-line bg-surface">
      <div class="border-b border-line px-4 py-3">
        <h2 class="text-sm font-semibold">
          项目列表
          <span class="ml-1 text-xs font-normal text-faint">
            {{ projects.length }} 个
          </span>
        </h2>
      </div>

      <p v-if="loadError" class="px-4 py-6 text-sm text-danger">
        加载失败：{{ loadError }}
      </p>

      <p v-else-if="loading" class="px-4 py-6 text-sm text-muted">加载中…</p>

      <p v-else-if="projects.length === 0" class="px-4 py-8 text-center text-sm text-muted">
        <span v-if="isAdmin()">还没有任何项目。用上面的表单创建第一个。</span>
        <span v-else>你还没有项目。用上面的表单创建第一个。</span>
      </p>

      <div v-else class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-line text-xs text-faint">
              <th class="px-4 py-2.5 text-left font-normal">项目</th>
              <th class="px-4 py-2.5 text-left font-normal">归属</th>
              <th class="px-4 py-2.5 text-left font-normal">后端</th>
              <th class="px-4 py-2.5 text-right font-normal">实体</th>
              <th class="px-4 py-2.5 text-right font-normal">关系</th>
              <th class="px-4 py-2.5 text-right font-normal">观测</th>
              <th class="px-4 py-2.5 text-right font-normal">交接</th>
              <th class="px-4 py-2.5 text-left font-normal">创建</th>
              <th class="px-4 py-2.5 text-right font-normal">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="row in rows"
              :key="row.project.project_id"
              class="border-b border-line/60 last:border-0 hover:bg-surface-2/50"
            >
              <td class="mono px-4 py-2.5">{{ row.project.project_id }}</td>
              <td class="px-4 py-2.5">
                <span
                  class="mono text-xs"
                  :class="
                    ownerIsMissing(row.project) || ownerIsStale(row.project)
                      ? 'text-warn'
                      : 'text-ink'
                  "
                  :title="
                    ownerIsMissing(row.project)
                      ? '无主项目：升级前遗留，仅管理员可见'
                      : ownerIsStale(row.project)
                        ? '归属账号已不存在：数据完整保留，仅管理员可见'
                        : `归属账号 ${row.project.owner}`
                  "
                >
                  {{ ownerLabel(row.project) }}
                </span>
                <span
                  v-if="ownerIsStale(row.project)"
                  class="ml-1.5 rounded border border-warn/40 bg-warn/10 px-1.5 py-0.5 text-[10px] text-warn"
                >
                  失效
                </span>
                <span
                  v-else-if="ownerIsMissing(row.project)"
                  class="ml-1.5 rounded border border-warn/40 bg-warn/10 px-1.5 py-0.5 text-[10px] text-warn"
                >
                  遗留
                </span>
              </td>
              <td class="px-4 py-2.5">
                <span
                  class="rounded border border-line bg-surface-2 px-1.5 py-0.5 text-[11px] text-muted"
                >
                  {{ row.project.backend }}
                </span>
              </td>
              <td class="px-4 py-2.5 text-right tabular-nums">
                {{ row.stats?.entities ?? "—" }}
              </td>
              <td class="px-4 py-2.5 text-right tabular-nums">
                {{ row.stats?.relations ?? "—" }}
              </td>
              <td class="px-4 py-2.5 text-right tabular-nums">
                {{ row.stats?.observations ?? "—" }}
              </td>
              <td class="px-4 py-2.5 text-right tabular-nums">
                {{ row.stats?.handoffs ?? "—" }}
              </td>
              <td
                class="px-4 py-2.5 text-xs text-muted"
                :title="formatDate(row.project.created_at)"
              >
                {{ formatRelative(row.project.created_at) }}
              </td>
              <td class="whitespace-nowrap px-4 py-2.5 text-right">
                <button
                  class="px-1.5 text-xs text-brand hover:underline"
                  @click="
                    router.push({
                      name: 'memory',
                      params: { projectId: row.project.project_id },
                    })
                  "
                >
                  记忆
                </button>
                <button
                  class="px-1.5 text-xs text-brand hover:underline"
                  @click="
                    router.push({
                      name: 'handoffs',
                      params: { projectId: row.project.project_id },
                    })
                  "
                >
                  交接
                </button>
                <button
                  class="px-1.5 text-xs text-muted hover:text-ink hover:underline"
                  @click="pendingRotate = row.project"
                >
                  换 token
                </button>
                <button
                  class="px-1.5 text-xs text-muted hover:text-danger hover:underline"
                  @click="pendingDelete = row.project"
                >
                  注销
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <ConfirmDialog
      v-if="pendingRotate"
      title="轮换项目 token"
      confirm-text="轮换"
      @confirm="confirmRotate"
      @cancel="pendingRotate = null"
    >
      <p>
        项目
        <span class="mono text-ink">{{ pendingRotate.project_id }}</span>
        将获得一个新 token，<strong class="text-warn">旧 token 立即失效</strong>。
      </p>
      <p class="mt-2">
        记忆与交接数据不受影响。已接入该 token 的客户端需要更新配置后重连。
      </p>
    </ConfirmDialog>

    <ConfirmDialog
      v-if="pendingDelete"
      title="注销项目"
      confirm-text="确认注销"
      danger
      @confirm="confirmDelete"
      @cancel="pendingDelete = null"
    >
      <p>
        项目
        <span class="mono text-ink">{{ pendingDelete.project_id }}</span>
        的登记信息将被删除。
      </p>
      <p class="mt-2 text-danger">
        该项目的全部记忆（实体、关系、观测）与交接记录将被物理删除，无法恢复。
      </p>
    </ConfirmDialog>

    <Toast
      v-if="toast"
      :message="toast.message"
      :kind="toast.kind"
      :duration="toast.kind === 'err' ? 5200 : 2600"
    />
  </div>
</template>