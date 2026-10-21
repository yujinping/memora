<script setup lang="ts">
/**
 * 控制台外壳：左侧导航 + 顶栏。
 *
 * @author yujinping
 * @intent 导航项写死在数组里而不是从路由表派生：控制台的页面不是「所有路由」，
 *         而是「所有**可导航**的路由」（登录页与详情页都不进来）。从路由表
 *         派生需要给每个路由加 `meta.nav` 之类的标记，反而更绕。
 *
 * @intent 顶栏显示当前用户名与角色，退出按钮放在最右：这是唯一一个
 *         「点错了会有副作用」的动作，放在视觉动线终点可以降低误触。
 *
 * @intent **导航按角色裁剪**：普通账号看不到「账号」页。这只是界面收敛，
 *         真正的拦截在路由守卫（防手输 URL）与服务端（403）。三层里任何
 *         一层被绕过，剩下的仍然是有效的。
 */
import { computed, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import BrandMark from "./BrandMark.vue";
import { isAdmin, refreshUserCount, session, signOut } from "../lib/session";

const route = useRoute();
const router = useRouter();

/** 导航项；`adminOnly` 的项对普通账号不渲染。 */
const nav = [
  { name: "projects", label: "项目", path: "/projects", adminOnly: false },
  { name: "users", label: "账号", path: "/users", adminOnly: true },
];

/** 当前身份可见的导航项。 */
const visibleNav = computed(() => nav.filter((item) => !item.adminOnly || isAdmin()));

/** 角色标签。 */
const roleLabel = computed(() => (isAdmin() ? "管理员" : "普通账号"));

/** 当前是否落在某个导航项（含其子页面）。 */
function isActive(item: { path: string }): boolean {
  return route.path === item.path || route.path.startsWith(item.path + "/");
}

const signingOut = ref(false);

async function doSignOut(): Promise<void> {
  signingOut.value = true;
  try {
    await signOut();
    await router.replace({ name: "login" });
  } finally {
    signingOut.value = false;
  }
}

onMounted(() => {
  // 顶栏的账号数可能是上次渲染时的旧值（另一个标签页建了账号），
  // 进入控制台时刷新一次即可，不必轮询。
  void refreshUserCount();
});
</script>

<template>
  <div class="flex h-full min-h-0">
    <!-- 侧栏：窄屏隐藏，靠顶栏的链接兜底 -->
    <aside
      class="hidden w-52 shrink-0 flex-col border-r border-line bg-surface px-3 py-5 md:flex"
    >
      <RouterLink to="/projects" class="mb-6 flex items-center gap-2 px-2">
        <BrandMark :size="28" />
        <span class="text-sm font-semibold tracking-wide">Memora</span>
        <span class="text-[11px] text-faint">忆庐</span>
      </RouterLink>

      <nav class="flex flex-col gap-0.5">
        <RouterLink
          v-for="item in visibleNav"
          :key="item.name"
          :to="item.path"
          class="rounded-md px-3 py-2 text-sm transition-colors"
          :class="
            isActive(item)
              ? 'bg-surface-2 font-medium text-ink'
              : 'text-muted hover:bg-surface-2 hover:text-ink'
          "
        >
          {{ item.label }}
        </RouterLink>
      </nav>

      <div class="mt-auto px-2 pt-6 text-[11px] leading-relaxed text-faint">
        <p>{{ roleLabel }} · 数据范围{{ isAdmin() ? "为全部账号" : "仅限本人" }}</p>
        <p class="mt-1">自托管 AI 长期记忆服务</p>
        <p class="mt-1">数据只在本机</p>
      </div>
    </aside>

    <div class="flex min-w-0 flex-1 flex-col">
      <header
        class="flex flex-wrap items-center justify-between gap-3 border-b border-line bg-surface/80 px-5 py-3 backdrop-blur"
      >
        <div class="flex items-center gap-3 md:hidden">
          <RouterLink to="/projects" class="flex items-center gap-2">
            <BrandMark :size="24" />
            <span class="text-sm font-semibold">Memora</span>
          </RouterLink>
          <RouterLink
            v-for="item in visibleNav"
            :key="item.name"
            :to="item.path"
            class="text-xs"
            :class="isActive(item) ? 'text-ink' : 'text-muted'"
          >
            {{ item.label }}
          </RouterLink>
        </div>

        <div class="hidden text-xs text-faint md:block">
          {{ isAdmin() ? `账号 ${session.userCount} 个` : "普通账号" }}
        </div>

        <div class="ml-auto flex items-center gap-3">
          <span class="text-xs text-muted">
            已登录
            <span class="mono ml-1 text-ink">{{ session.user ?? "—" }}</span>
            <span
              class="ml-1.5 rounded border px-1.5 py-0.5 text-[10px]"
              :class="
                isAdmin()
                  ? 'border-brand/40 bg-brand/10 text-brand'
                  : 'border-line bg-surface-2 text-muted'
              "
            >
              {{ roleLabel }}
            </span>
          </span>
          <button
            class="rounded-md border border-line px-3 py-1.5 text-xs text-muted transition-colors hover:border-danger/60 hover:text-danger disabled:opacity-50"
            :disabled="signingOut"
            @click="doSignOut"
          >
            {{ signingOut ? "退出中…" : "退出" }}
          </button>
        </div>
      </header>

      <main class="min-h-0 flex-1 overflow-y-auto px-5 py-6">
        <div class="mx-auto max-w-6xl">
          <slot />
        </div>
      </main>
    </div>
  </div>
</template>