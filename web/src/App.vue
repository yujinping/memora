<script setup lang="ts">
/**
 * 应用外壳。
 *
 * @intent 登录页不套布局（它需要全屏居中），其余页面共用左侧导航。
 *         用 `route.meta.public` 判断而不是路径字符串比较：新增公开页面时
 *         只需在路由上标记，不必回来改这里。
 */
import { computed } from "vue";
import { useRoute } from "vue-router";
import AppShell from "./components/AppShell.vue";

const route = useRoute();
const bare = computed(() => route.meta.public === true);
</script>

<template>
  <RouterView v-if="bare" />
  <AppShell v-else>
    <RouterView />
  </AppShell>
</template>