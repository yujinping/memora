<script setup lang="ts">
/**
 * Memora 图标（内联 SVG）。
 *
 * @author yujinping
 * @intent 内联而不用 `<img src="/favicon.svg">`：后者会让 **Vite 把该 URL 当成
 *         模块去解析**，而 `/favicon.svg` 是后端在运行时提供的路由、磁盘上并不
 *         存在同名文件，构建直接失败（`Rollup failed to resolve import`）。
 *         内联后图标随 js 一起进入二进制，既少一次请求，也不会在首屏出现
 *         一段「图标还没加载出来」的空白。
 *
 * @intent 颜色用 `currentColor` 之外的字面量：这是品牌标识，不该随主题变色。
 *         圆角方底 + 白色 M，与后端 `console::icon` 的 favicon 保持同一形状。
 */
withDefaults(defineProps<{ size?: number }>(), { size: 28 });
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 32 32"
    fill="none"
    xmlns="http://www.w3.org/2000/svg"
    aria-hidden="true"
    class="shrink-0"
  >
    <rect width="32" height="32" rx="8" fill="#4c8dff" />
    <path d="M9 22V10h3l4 6 4-6h3v12h-3v-7l-4 6-4-6v7z" fill="#fff" />
  </svg>
</template>