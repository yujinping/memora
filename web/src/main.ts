/**
 * 应用入口。
 *
 * @intent 启动顺序是「自检 → 挂载」而不是「挂载 → 自检」：先拿到登录态再渲染，
 *         路由守卫才有答案可用。反过来会让首屏先闪一下登录页/控制台再纠正。
 */

import { createApp } from "vue";
import App from "./App.vue";
import { router } from "./router";
import { refresh } from "./lib/session";
import { setUnauthorizedHandler } from "./lib/api";
import "./style.css";

/** 会话失效时统一回到登录页，并记住原本想去的地方。 */
setUnauthorizedHandler(() => {
  const current = router.currentRoute.value;
  if (current.name === "login") return;
  void router.replace({ name: "login", query: { next: current.fullPath } });
});

async function bootstrap(): Promise<void> {
  await refresh();
  createApp(App).use(router).mount("#app");
}

void bootstrap();