<script setup lang="ts">
/**
 * 登录页。
 *
 * @author yujinping
 * @intent 三个状态必须都能给出**可执行**的提示，而不是一个必然失败的输入框：
 *         1. 正常：渲染表单；
 *         2. 系统里一个账号都没有：说明怎么播种（`memora init --force`）；
 *         3. 服务不可达：说明连不上，别让用户怀疑自己的口令。
 *         这三者在旧的手写页面里混在两条 if 里，切换控制台时很容易丢掉其中一种。
 *
 * @intent 用户名预填上次登录的值（存 localStorage），口令永远不预填：
 *         预填口令需要在某处存明文，而它不是一个「方便」能换的东西。
 */
import { computed, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import BrandMark from "../components/BrandMark.vue";
import { lastUser, session } from "../lib/session";
import { signIn } from "../lib/session";
import { ApiError } from "../lib/api";

const route = useRoute();
const router = useRouter();

const username = ref("");
const password = ref("");
const busy = ref(false);
const errorMessage = ref("");

/** 系统里没有任何账号：登录不可能成功，给修复指引。 */
const noAccounts = computed(() => !session.loginEnabled && session.ready);

onMounted(() => {
  username.value = lastUser() || "admin";
});

async function submit(): Promise<void> {
  if (busy.value) return;
  errorMessage.value = "";
  busy.value = true;
  try {
    await signIn(username.value.trim(), password.value);
    password.value = "";
    const next = typeof route.query.next === "string" ? route.query.next : "/projects";
    await router.replace(next);
  } catch (err) {
    if (err instanceof ApiError) {
      // 401 时统一话术：服务端刻意不区分「用户名错」与「口令错」，
      // 这里也不能自作聪明地补一句「该用户不存在」。
      errorMessage.value =
        err.status === 401 ? "用户名或口令不正确" : err.message;
    } else {
      errorMessage.value = String(err);
    }
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div class="flex min-h-full items-center justify-center px-5 py-10">
    <div class="w-full max-w-sm">
      <div class="mb-7 flex items-center gap-3">
        <BrandMark :size="36" />
        <div>
          <h1 class="text-lg font-semibold leading-tight">Memora</h1>
          <p class="text-xs text-faint">自托管 AI 长期记忆服务</p>
        </div>
      </div>

      <!-- 无账号：不渲染表单 -->
      <div
        v-if="noAccounts"
        class="rounded-lg border border-warn/40 bg-surface p-4 text-sm leading-relaxed text-muted"
      >
        <p class="mb-2 font-medium text-warn">尚未创建任何控制台账号</p>
        <p>首次使用需先在服务器上生成种子账号：</p>
        <pre
          class="mono mt-2 overflow-x-auto rounded-md bg-surface-2 px-3 py-2 text-xs text-ink"
        >memora init --force</pre>
        <p class="mt-2 text-xs">
          它会打印一次随机口令，之后用该账号登录并在此页面创建更多账号。
        </p>
      </div>

      <form
        v-else
        class="rounded-lg border border-line bg-surface p-5 shadow-sm"
        @submit.prevent="submit"
      >
        <div class="mb-4">
          <label for="username" class="mb-1.5 block text-xs text-muted"
            >用户名</label
          >
          <input
            id="username"
            v-model="username"
            type="text"
            autocomplete="username"
            class="w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm text-ink placeholder:text-faint"
            placeholder="admin"
          />
        </div>

        <div class="mb-4">
          <label for="password" class="mb-1.5 block text-xs text-muted"
            >口令</label
          >
          <input
            id="password"
            v-model="password"
            type="password"
            autocomplete="current-password"
            class="w-full rounded-md border border-line bg-surface-2 px-3 py-2 text-sm text-ink placeholder:text-faint"
            placeholder="memora init 输出的口令"
          />
        </div>

        <p v-if="errorMessage" class="mb-3 text-xs text-danger">
          {{ errorMessage }}
        </p>

        <button
          type="submit"
          class="w-full rounded-md bg-brand px-4 py-2 text-sm font-medium text-brand-ink transition-opacity hover:opacity-90 disabled:opacity-50"
          :disabled="busy"
        >
          {{ busy ? "登录中…" : "登录" }}
        </button>
      </form>

      <p class="mt-6 text-center text-[11px] text-faint">
        凭据经 HTTPS 明文口令传输，请仅在可信链路下登录
      </p>
    </div>
  </div>
</template>