<script setup lang="ts">
/**
 * 一次性 token 展示面板。
 *
 * @author yujinping
 * @intent 这是全流程里**唯一**能让用户拿到明文 token 的时刻：服务端只存
 *         SHA-256，关闭之后无法取回。因此这块面板承担了三件事，缺一不可：
 *         1. 醒目地说清「只显示一次」；
 *         2. 一键复制，并给出可直接粘贴的客户端配置；
 *         3. 明确指出丢失后的补救动作是「重新生成」而不是「找回来」。
 *
 * @intent 复制失败（非 HTTPS 下剪贴板 API 被禁用）时退回「选中文本」而不是
 *         报错：自托管的常见形态正是 `http://localhost:6789`，这不是异常。
 */
import { computed, ref } from "vue";
import { copyText } from "../lib/format";

const props = defineProps<{
  /** 项目标识 */
  projectId: string;
  /** 明文 token */
  token: string;
  /** 是否由「轮换」产生（影响标题措辞） */
  rotated?: boolean;
}>();

const emit = defineEmits<{ (e: "close"): void }>();

const tokenField = ref<HTMLTextAreaElement | null>(null);
const copied = ref(false);
const notice = ref("");

/** MCP 端点地址：与当前页面同源，避免用户手拼出错。 */
const mcpUrl = computed(() => `${location.origin}/mcp`);

/**
 * 可直接粘贴的客户端配置片段。
 *
 * @intent 用 Zed / Claude 等客户端通用的 `context_servers` 形态，并保留
 *         `Authorization` 头的完整写法——这是接入时最容易写错的一行
 *         （少了 `Bearer ` 前缀会得到一个费解的 401）。
 */
const snippet = computed(() =>
  JSON.stringify(
    {
      mcpServers: {
        memora: {
          url: mcpUrl.value,
          headers: { Authorization: `Bearer ${props.token}` },
        },
      },
    },
    null,
    2,
  ),
);

async function copy(value: string, label: string): Promise<void> {
  const ok = await copyText(value);
  if (ok) {
    copied.value = true;
    notice.value = `${label}已复制`;
    window.setTimeout(() => {
      copied.value = false;
      notice.value = "";
    }, 2200);
    return;
  }

  // 剪贴板不可用：选中 token 让用户自己按 Cmd/Ctrl+C。
  const field = tokenField.value;
  if (field) {
    field.focus();
    field.select();
  }
  notice.value = "剪贴板不可用，已选中内容，请按 Cmd/Ctrl+C 复制";
}

defineExpose({ copy });
</script>

<template>
  <section
    class="mb-6 rounded-lg border border-brand/50 bg-surface p-5"
    aria-live="polite"
  >
    <div class="mb-2 flex flex-wrap items-center justify-between gap-2">
      <h2 class="text-sm font-semibold">
        项目
        <span class="mono text-brand">{{ projectId }}</span>
        {{ rotated ? "的 token 已轮换" : "已创建" }}
      </h2>
      <button
        class="text-xs text-faint transition-colors hover:text-ink"
        @click="emit('close')"
      >
        关闭
      </button>
    </div>

    <p class="mb-3 text-xs font-medium text-warn">
      ⚠ 明文 token 仅显示一次。服务端只保存它的 SHA-256 摘要，关闭后无法再取回——
      丢失只能重新生成（旧 token 会立即失效）。
    </p>

    <div class="mb-3 flex flex-wrap items-stretch gap-2">
      <textarea
        ref="tokenField"
        readonly
        rows="2"
        class="mono min-w-0 flex-1 resize-none rounded-md border border-line bg-surface-2 px-3 py-2 text-xs leading-relaxed text-ink"
        :value="token"
        @focus="($event.target as HTMLTextAreaElement).select()"
      ></textarea>
      <div class="flex flex-col gap-2">
        <button
          class="rounded-md bg-brand px-3.5 py-2 text-xs font-medium text-brand-ink transition-opacity hover:opacity-90"
          @click="copy(token, 'token')"
        >
          复制 token
        </button>
        <button
          class="rounded-md border border-line px-3.5 py-2 text-xs text-muted transition-colors hover:text-ink"
          @click="copy(snippet, '配置片段')"
        >
          复制配置
        </button>
      </div>
    </div>

    <p v-if="notice" class="mb-2 text-xs text-ok">{{ notice }}</p>

    <details class="rounded-md border border-line bg-surface-2 px-3 py-2">
      <summary class="cursor-pointer text-xs text-muted">
        客户端配置片段（MCP Streamable HTTP）
      </summary>
      <pre
        class="mono mt-2 overflow-x-auto whitespace-pre-wrap break-all text-[11px] leading-relaxed text-muted"
      >{{ snippet }}</pre>
    </details>
  </section>
</template>