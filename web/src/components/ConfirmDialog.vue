<script setup lang="ts">
/**
 * 危险动作的确认对话框。
 *
 * @author yujinping
 * @intent 不用 `window.confirm`：它无法定制文案结构（注销项目要列出「会连带
 *         删除什么」），且在所有浏览器里样式都与控制台割裂。更重要的是，
 *         原生 confirm 的按钮顺序由浏览器决定，而「取消」必须永远是
 *         视觉上的第一选择。
 *
 * @intent `danger` 为真时把确认按钮标成危险色并**默认聚焦取消**：
 *         误触 Enter 时应当是「什么都没发生」。
 */
import { nextTick, onMounted, ref } from "vue";

withDefaults(
  defineProps<{
    /** 标题 */
    title: string;
    /** 确认按钮文案 */
    confirmText?: string;
    /** 是否是不可逆的破坏性动作 */
    danger?: boolean;
  }>(),
  { confirmText: "确认", danger: false },
);

const emit = defineEmits<{
  (e: "confirm"): void;
  (e: "cancel"): void;
}>();

const cancelButton = ref<HTMLButtonElement | null>(null);

onMounted(() => {
  void nextTick(() => cancelButton.value?.focus());
});

/** Esc 关闭：与系统对话框一致，不需要用户去找取消按钮。 */
function onKeydown(event: KeyboardEvent): void {
  if (event.key === "Escape") emit("cancel");
}
</script>

<template>
  <Teleport to="body">
    <div
      class="fixed inset-0 z-40 flex items-center justify-center bg-black/55 px-4"
      @keydown="onKeydown"
      @click.self="emit('cancel')"
    >
      <div
        class="w-full max-w-md rounded-xl border border-line bg-surface p-5 shadow-2xl"
        role="dialog"
        aria-modal="true"
      >
        <h2 class="mb-3 text-base font-semibold">{{ title }}</h2>
        <div class="mb-5 text-sm leading-relaxed text-muted">
          <slot />
        </div>
        <div class="flex justify-end gap-2">
          <button
            ref="cancelButton"
            class="rounded-md border border-line px-3.5 py-2 text-sm text-muted transition-colors hover:text-ink"
            @click="emit('cancel')"
          >
            取消
          </button>
          <button
            class="rounded-md px-3.5 py-2 text-sm font-medium transition-opacity hover:opacity-90"
            :class="
              danger
                ? 'bg-danger text-white'
                : 'bg-brand text-brand-ink'
            "
            @click="emit('confirm')"
          >
            {{ confirmText }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>