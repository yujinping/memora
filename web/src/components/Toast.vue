<script setup lang="ts">
/**
 * 底部提示条。
 *
 * @author yujinping
 * @intent 用 `Teleport` 挂到 body：提示必须浮在所有内容之上，而页面的
 *         `overflow-y-auto` 容器会把它裁在可视区外——这是一个只在长列表里
 *         才会出现的 bug（提示被裁掉，看起来「点了没反应」）。
 */
import { onBeforeUnmount, onMounted, ref } from "vue";

const props = withDefaults(
  defineProps<{
    /** 提示文字 */
    message: string;
    /** 语义：`ok` 绿色、`err` 红色、`info` 中性 */
    kind?: "ok" | "err" | "info";
    /** 自动消失毫秒数；0 表示不自动消失 */
    duration?: number;
  }>(),
  { kind: "info", duration: 2600 },
);

const visible = ref(true);
let timer: number | undefined;

onMounted(() => {
  if (props.duration > 0) {
    timer = window.setTimeout(() => {
      visible.value = false;
    }, props.duration);
  }
});

onBeforeUnmount(() => {
  if (timer !== undefined) window.clearTimeout(timer);
});

/** 边框色随语义变化。 */
const borderClass = {
  ok: "border-ok/50",
  err: "border-danger/60",
  info: "border-line",
}[props.kind];

/** 文字色随语义变化。 */
const textClass = {
  ok: "text-ok",
  err: "text-danger",
  info: "text-ink",
}[props.kind];
</script>

<template>
  <Teleport to="body">
    <Transition name="fade">
      <div
        v-if="visible"
        class="pointer-events-none fixed bottom-6 left-1/2 z-50 -translate-x-1/2 px-4"
      >
        <div
          class="pointer-events-auto max-w-[90vw] rounded-lg border bg-surface px-4 py-2.5 text-sm shadow-lg"
          :class="borderClass"
          role="status"
        >
          <span :class="textClass">{{ message }}</span>
        </div>
      </div>
    </Transition>
  </Teleport>
</template>