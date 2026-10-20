<script setup lang="ts">
/**
 * 账号管理：列表、新建、改口令、删除。
 *
 * @author yujinping
 * @intent 删除按钮在「只剩一个账号」时直接禁用并给出原因，而不是让用户点下去
 *         再收到 409：服务端拒绝它是正确行为，但一个能点却永远失败的按钮
 *         比一个禁用且带说明的按钮难懂得多。
 *
 * @intent 改口令**不踢掉已有会话**（这是服务端的语义），界面据此提示：
 *         「已登录的设备无需重新登录」。若用户想踢人，正确动作是删除账号，
 *         界面上把这两个动作的差别写清楚，比事后再解释便宜。
 */
import { computed, onMounted, ref } from "vue";
import {
  createUser,
  deleteUser,
  listUsers,
  updatePassword,
  type ConsoleUser,
} from "../lib/api";
import { refreshUserCount, session } from "../lib/session";
import { formatTime } from "../lib/format";
import ConfirmDialog from "../components/ConfirmDialog.vue";
import Toast from "../components/Toast.vue";

const users = ref<ConsoleUser[]>([]);
const loading = ref(true);
const loadError = ref("");

/** 新建表单。 */
const newName = ref("");
const newPassword = ref("");
const creating = ref(false);

/** 改口令表单（`username` 为空表示关闭）。 */
const editing = ref<{ username: string; password: string } | null>(null);
const saving = ref(false);

/** 待删除的账号。 */
const pendingDelete = ref<ConsoleUser | null>(null);

const toast = ref<{ message: string; kind: "ok" | "err" } | null>(null);

/** 只剩一个账号时不允许删除。 */
const lastOne = computed(() => users.value.length <= 1);

function notify(message: string, kind: "ok" | "err" = "ok"): void {
  toast.value = { message, kind };
  window.setTimeout(
    () => {
      toast.value = null;
    },
    kind === "err" ? 5200 : 2600,
  );
}

async function load(): Promise<void> {
  loading.value = true;
  loadError.value = "";
  try {
    users.value = await listUsers();
  } catch (err) {
    loadError.value = err instanceof Error ? err.message : String(err);
    users.value = [];
  } finally {
    loading.value = false;
  }
}

async function submitCreate(): Promise<void> {
  if (creating.value) return;
  creating.value = true;
  try {
    const created = await createUser(newName.value, newPassword.value);
    newName.value = "";
    newPassword.value = "";
    await load();
    await refreshUserCount();
    notify(`账号 ${created.username} 已创建`, "ok");
  } catch (err) {
    notify(err instanceof Error ? err.message : String(err), "err");
  } finally {
    creating.value = false;
  }
}

async function submitPassword(): Promise<void> {
  const form = editing.value;
  if (!form || saving.value) return;
  saving.value = true;
  try {
    await updatePassword(form.username, form.password);
    editing.value = null;
    await load();
    notify(`账号 ${form.username} 的口令已更新（已登录的设备无需重新登录）`, "ok");
  } catch (err) {
    notify(err instanceof Error ? err.message : String(err), "err");
  } finally {
    saving.value = false;
  }
}

async function confirmDelete(): Promise<void> {
  const target = pendingDelete.value;
  pendingDelete.value = null;
  if (!target) return;
  try {
    const result = await deleteUser(target.username);
    await load();
    await refreshUserCount();
    const revoked =
      result.sessions_revoked > 0
        ? `，已销毁其 ${result.sessions_revoked} 个会话`
        : "";
    notify(`账号 ${target.username} 已删除${revoked}`, "ok");
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
        <h1 class="text-lg font-semibold">账号</h1>
        <p class="mt-1 text-xs text-muted">
          能登录控制台的账号都可管理项目与记忆（当前没有角色区分）。
          配置文件中的
          <span class="mono">admin_user / admin_password_hash</span>
          只在首次启动时用于播种第一个账号。
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

    <!-- 新建账号 -->
    <section class="mb-6 rounded-lg border border-line bg-surface p-4">
      <h2 class="mb-3 text-sm font-semibold">新建账号</h2>
      <form class="flex flex-wrap items-end gap-3" @submit.prevent="submitCreate">
        <div class="min-w-44 flex-1">
          <label for="new-user" class="mb-1.5 block text-xs text-muted">用户名</label>
          <input
            id="new-user"
            v-model="newName"
            type="text"
            autocomplete="off"
            class="w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm placeholder:text-faint"
            placeholder="不得包含空白字符"
          />
        </div>
        <div class="min-w-44 flex-1">
          <label for="new-pass" class="mb-1.5 block text-xs text-muted">
            口令（至少 8 位）
          </label>
          <input
            id="new-pass"
            v-model="newPassword"
            type="password"
            autocomplete="new-password"
            class="w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm placeholder:text-faint"
            placeholder="创建后请转交给使用者"
          />
        </div>
        <button
          type="submit"
          class="rounded-md bg-brand px-4 py-2 text-sm font-medium text-brand-ink transition-opacity hover:opacity-90 disabled:opacity-50"
          :disabled="creating"
        >
          {{ creating ? "创建中…" : "创建账号" }}
        </button>
      </form>
    </section>

    <!-- 列表 -->
    <section class="rounded-lg border border-line bg-surface">
      <div class="border-b border-line px-4 py-3">
        <h2 class="text-sm font-semibold">
          账号列表
          <span class="ml-1 text-xs font-normal text-faint">
            {{ users.length }} 个
          </span>
        </h2>
      </div>

      <p v-if="loadError" class="px-4 py-6 text-sm text-danger">
        加载失败：{{ loadError }}
      </p>
      <p v-else-if="loading" class="px-4 py-6 text-sm text-muted">加载中…</p>

      <div v-else class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-line text-xs text-faint">
              <th class="px-4 py-2.5 text-left font-normal">用户名</th>
              <th class="px-4 py-2.5 text-left font-normal">创建时间</th>
              <th class="px-4 py-2.5 text-left font-normal">口令更新</th>
              <th class="px-4 py-2.5 text-right font-normal">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="user in users"
              :key="user.username"
              class="border-b border-line/60 last:border-0 hover:bg-surface-2/50"
            >
              <td class="px-4 py-2.5">
                <span class="mono">{{ user.username }}</span>
                <span
                  v-if="user.username === session.user"
                  class="ml-2 rounded border border-brand/40 bg-brand/10 px-1.5 py-0.5 text-[10px] text-brand"
                >
                  当前登录
                </span>
              </td>
              <td class="px-4 py-2.5 text-xs text-muted">
                {{ formatTime(user.created_at) }}
              </td>
              <td class="px-4 py-2.5 text-xs text-muted">
                {{ formatTime(user.updated_at) }}
              </td>
              <td class="whitespace-nowrap px-4 py-2.5 text-right">
                <button
                  class="px-1.5 text-xs text-muted hover:text-ink hover:underline"
                  @click="editing = { username: user.username, password: '' }"
                >
                  改口令
                </button>
                <button
                  class="px-1.5 text-xs transition-colors"
                  :class="
                    lastOne
                      ? 'cursor-not-allowed text-faint'
                      : 'text-muted hover:text-danger hover:underline'
                  "
                  :disabled="lastOne"
                  :title="
                    lastOne
                      ? '不能删除唯一的账号：删掉后将无法再登录控制台'
                      : '删除该账号并销毁其全部会话'
                  "
                  @click="!lastOne && (pendingDelete = user)"
                >
                  删除
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <p v-if="lastOne && !loading" class="border-t border-line px-4 py-2 text-[11px] text-faint">
        只有一账号时删除被禁用：删掉它会让控制台永久无法登录。
      </p>
    </section>

    <!-- 改口令 -->
    <Teleport to="body">
      <div
        v-if="editing"
        class="fixed inset-0 z-40 flex items-center justify-center bg-black/55 px-4"
        @click.self="editing = null"
      >
        <form
          class="w-full max-w-sm rounded-xl border border-line bg-surface p-5 shadow-2xl"
          @submit.prevent="submitPassword"
        >
          <h2 class="mb-1 text-base font-semibold">修改口令</h2>
          <p class="mb-4 text-xs text-muted">
            账号 <span class="mono text-ink">{{ editing.username }}</span> ·
            已登录的设备无需重新登录
          </p>
          <label for="edit-pass" class="mb-1.5 block text-xs text-muted">
            新口令（至少 8 位）
          </label>
          <input
            id="edit-pass"
            v-model="editing.password"
            type="password"
            autocomplete="new-password"
            class="mb-4 w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm"
          />
          <div class="flex justify-end gap-2">
            <button
              type="button"
              class="rounded-md border border-line px-3.5 py-2 text-sm text-muted hover:text-ink"
              @click="editing = null"
            >
              取消
            </button>
            <button
              type="submit"
              class="rounded-md bg-brand px-3.5 py-2 text-sm font-medium text-brand-ink disabled:opacity-50"
              :disabled="saving || editing.password.length === 0"
            >
              {{ saving ? "保存中…" : "保存" }}
            </button>
          </div>
        </form>
      </div>
    </Teleport>

    <ConfirmDialog
      v-if="pendingDelete"
      title="删除账号"
      confirm-text="确认删除"
      danger
      @confirm="confirmDelete"
      @cancel="pendingDelete = null"
    >
      <p>
        账号 <span class="mono text-ink">{{ pendingDelete.username }}</span>
        将被删除，其全部已登录会话立即失效。
      </p>
      <p class="mt-2">记忆与项目数据不受影响。</p>
    </ConfirmDialog>

    <Toast
      v-if="toast"
      :message="toast.message"
      :kind="toast.kind"
      :duration="toast.kind === 'err' ? 5200 : 2600"
    />
  </div>
</template>