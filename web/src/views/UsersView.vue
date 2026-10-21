<script setup lang="ts">
/**
 * 账号管理：列表、新建、改角色、改口令、删除（仅管理员可见）。
 *
 * @author yujinping
 * @intent 两档角色，界面必须让它们的差别一眼可见：管理员管全部数据，
 *         普通账号只管自己名下的项目。把「角色」做成一个不解释的徽标，
 *         用户只能靠试错去理解权限范围——那是「为什么他看不到数据」
 *         这类问题的源头。故角色旁直接写出数据范围。
 *
 * @intent 删除与降级的禁用判据是**管理员数量**而不是账号数量：只剩一群
 *         普通账号时，删掉/降级最后一个管理员会让管理面永久锁死。
 *         服务端同样会拒绝（409 `last_admin`），界面提前禁用是为了
 *         不让用户点一个必然失败的按钮，而不是替代服务端判断。
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
  updateRole,
  type ConsoleUser,
  type Role,
} from "../lib/api";
import { refreshRole, refreshUserCount, session } from "../lib/session";
import { formatTime } from "../lib/format";
import ConfirmDialog from "../components/ConfirmDialog.vue";
import Toast from "../components/Toast.vue";

const users = ref<ConsoleUser[]>([]);
const loading = ref(true);
const loadError = ref("");

/** 新建表单。 */
const newName = ref("");
const newPassword = ref("");
const newRole = ref<Role>("user");
const creating = ref(false);

/** 改口令表单（`username` 为空表示关闭）。 */
const editing = ref<{ username: string; password: string } | null>(null);
const saving = ref(false);

/** 待删除的账号。 */
const pendingDelete = ref<ConsoleUser | null>(null);

/** 待确认的角色变更。 */
const pendingRole = ref<{ user: ConsoleUser; role: Role } | null>(null);

const toast = ref<{ message: string; kind: "ok" | "err" } | null>(null);

/** 管理员数量。 */
const adminCount = computed(
  () => users.value.filter((u) => u.role === "admin").length,
);

/** 删除该账号是否会锁死管理面。 */
function isLastAdmin(user: ConsoleUser): boolean {
  return user.role === "admin" && adminCount.value <= 1;
}

/** 该账号在界面上是否可被删除。 */
function canDelete(user: ConsoleUser): boolean {
  return !isLastAdmin(user);
}

/** 删除按钮的提示文案。 */
function deleteHint(user: ConsoleUser): string {
  if (isLastAdmin(user)) {
    return "不能删除唯一的管理员：删掉后将无人能管理账号；先提升另一个账号";
  }
  return "删除该账号并销毁其全部会话（其名下项目会保留，仅管理员可见）";
}

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
    const created = await createUser(
      newName.value,
      newPassword.value,
      newRole.value,
    );
    newName.value = "";
    newPassword.value = "";
    newRole.value = "user";
    await load();
    await refreshUserCount();
    notify(
      `账号 ${created.username} 已创建（${created.role === "admin" ? "管理员" : "普通账号"}）`,
      "ok",
    );
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

/**
 * 提交角色变更。
 *
 * @intent 改的若是自己，必须刷新本地身份：否则界面会停在旧角色上——
 *         降权后「账号」导航还在、点进去满屏 403，是最难自查的一种错位。
 */
async function confirmRole(): Promise<void> {
  const target = pendingRole.value;
  pendingRole.value = null;
  if (!target) return;
  try {
    const result = await updateRole(target.user.username, target.role);
    await load();
    const effect =
      result.sessions_updated > 0
        ? `，已即时生效于其 ${result.sessions_updated} 个在线会话（无需重新登录）`
        : "，对方重新登录时生效";
    notify(
      `账号 ${result.username} 已改为${
        result.role === "admin" ? "管理员" : "普通账号"
      }${effect}`,
      "ok",
    );
    if (result.username === session.user) {
      await refreshRole();
    }
  } catch (err) {
    notify(err instanceof Error ? err.message : String(err), "err");
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
          管理员可管理全部项目与全部账号；普通账号只能管理自己名下的项目，
          并且只能修改自己的口令。配置文件中的
          <span class="mono">admin_user / admin_password_hash</span>
          只在首次启动时用于播种第一个管理员。
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
        <div class="w-44">
          <label for="new-role" class="mb-1.5 block text-xs text-muted">角色</label>
          <select
            id="new-role"
            v-model="newRole"
            class="w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm"
          >
            <option value="user">普通账号 · 仅本人数据</option>
            <option value="admin">管理员 · 全部数据</option>
          </select>
        </div>
        <button
          type="submit"
          class="rounded-md bg-brand px-4 py-2 text-sm font-medium text-brand-ink transition-opacity hover:opacity-90 disabled:opacity-50"
          :disabled="creating"
        >
          {{ creating ? "创建中…" : "创建账号" }}
        </button>
      </form>
      <p v-if="newRole === 'admin'" class="mt-3 text-[11px] text-warn">
        管理员能查看并删除<strong>全部</strong>项目、记忆与账号。仅在确实需要
        第二个人拥有全部权限时才创建管理员。
      </p>
    </section>

    <!-- 列表 -->
    <section class="rounded-lg border border-line bg-surface">
      <div class="border-b border-line px-4 py-3">
        <h2 class="text-sm font-semibold">
          账号列表
          <span class="ml-1 text-xs font-normal text-faint">
            {{ users.length }} 个 · 管理员 {{ adminCount }} 个
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
              <th class="px-4 py-2.5 text-left font-normal">角色</th>
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
              <td class="px-4 py-2.5">
                <span
                  class="rounded border px-1.5 py-0.5 text-[11px]"
                  :class="
                    user.role === 'admin'
                      ? 'border-brand/40 bg-brand/10 text-brand'
                      : 'border-line bg-surface-2 text-muted'
                  "
                >
                  {{ user.role === "admin" ? "管理员" : "普通账号" }}
                </span>
                <span class="ml-2 text-[11px] text-faint">
                  {{ user.role === "admin" ? "全部数据" : "仅本人数据" }}
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
                    isLastAdmin(user) && user.role === 'admin'
                      ? 'cursor-not-allowed text-faint'
                      : 'text-muted hover:text-ink hover:underline'
                  "
                  :disabled="isLastAdmin(user)"
                  :title="
                    isLastAdmin(user)
                      ? '不能降级唯一的管理员：降级后将无人能管理账号'
                      : user.role === 'admin'
                        ? '改为普通账号：只能看到自己名下的项目'
                        : '提升为管理员：可查看并管理全部数据'
                  "
                  @click="
                    !isLastAdmin(user) &&
                      (pendingRole = {
                        user,
                        role: user.role === 'admin' ? 'user' : 'admin',
                      })
                  "
                >
                  {{ user.role === "admin" ? "降为普通" : "提升为管理员" }}
                </button>
                <button
                  class="px-1.5 text-xs transition-colors"
                  :class="
                    canDelete(user)
                      ? 'text-muted hover:text-danger hover:underline'
                      : 'cursor-not-allowed text-faint'
                  "
                  :disabled="!canDelete(user)"
                  :title="deleteHint(user)"
                  @click="canDelete(user) && (pendingDelete = user)"
                >
                  删除
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <p
        v-if="adminCount <= 1 && !loading && !loadError"
        class="border-t border-line px-4 py-2 text-[11px] text-faint"
      >
        当前只有一个管理员，故它既不能删除也不能降级：那会让账号管理永久无人可用。
        先提升另一个账号，再来处置它。
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
      v-if="pendingRole"
      :title="pendingRole.role === 'admin' ? '提升为管理员' : '降为普通账号'"
      :confirm-text="pendingRole.role === 'admin' ? '确认提升' : '确认降级'"
      :danger="pendingRole.role === 'user'"
      @confirm="confirmRole"
      @cancel="pendingRole = null"
    >
      <p>
        账号 <span class="mono text-ink">{{ pendingRole.user.username }}</span>
        将改为
        <strong>{{ pendingRole.role === "admin" ? "管理员" : "普通账号" }}</strong
        >。
      </p>
      <p v-if="pendingRole.role === 'admin'" class="mt-2 text-warn">
        管理员将能查看并删除全部项目、记忆与全部账号。
      </p>
      <p v-else class="mt-2">
        降级后它只能看到自己名下的项目，立刻失去账号管理权限。
      </p>
      <p class="mt-2 text-xs text-muted">其在线会话的角色会立即刷新，无需重新登录。</p>
    </ConfirmDialog>

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
      <p class="mt-2">记忆与项目数据不受影响；该账号名下的项目会保留，仅管理员可见。</p>
    </ConfirmDialog>

    <Toast
      v-if="toast"
      :message="toast.message"
      :kind="toast.kind"
      :duration="toast.kind === 'err' ? 5200 : 2600"
    />
  </div>
</template>