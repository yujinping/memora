/**
 * 路由表。
 *
 * @intent 用 `createWebHistory` 而非 hash 模式：URL 里出现 `#` 在自托管场景下
 *         没有技术收益（服务端已经能做 SPA 回退），却会让每个被分享的链接
 *         都难看一截。
 *
 * @intent 每个业务路由都带 `meta.needsAuth`，守卫统一读它，而不是在守卫里
 *         枚举路径名——新增页面时忘记加权限判断，是这个模式最常见的漏法，
 *         而「默认需要登录」让漏法变成「默认安全」。
 *
 * @intent `meta.adminOnly` 与 `meta.needsAuth` 同一取向：默认不设，需要时显式
 *         标注。这里无法反过来（默认仅管理员），因为绝大多数页面租户都要用。
 *         即便如此，漏标的后果只是「页面能打开、请求 403」，不会越权——
 *         真正的授权始终在服务端。
 */

import { createRouter, createWebHistory } from "vue-router";
import { isAdmin, session } from "./lib/session";

const routes = [
  {
    path: "/login",
    name: "login",
    component: () => import("./views/LoginView.vue"),
    meta: { public: true, title: "登录" },
  },
  {
    path: "/",
    redirect: "/projects",
  },
  {
    path: "/projects",
    name: "projects",
    component: () => import("./views/ProjectsView.vue"),
    meta: { title: "项目" },
  },
  {
    path: "/projects/:projectId/memory",
    name: "memory",
    component: () => import("./views/MemoryView.vue"),
    // 记忆列表按项目区分，但页面本身是同一个组件（避免重复实现）
    props: true,
    meta: { title: "记忆" },
  },
  {
    path: "/projects/:projectId/handoffs",
    name: "handoffs",
    component: () => import("./views/HandoffsView.vue"),
    props: true,
    meta: { title: "交接" },
  },
  {
    path: "/users",
    name: "users",
    component: () => import("./views/UsersView.vue"),
    // 账号管理仅管理员可用：租户即便手输 URL 也进不去
    meta: { title: "账号", adminOnly: true },
  },
  {
    // 兜底：未知路径回到项目页，而不是留一个空白 404——控制台的链接几乎
    // 都由内部生成，真出现未知路径通常是旧书签，回到首页就是最好的处置。
    path: "/:pathMatch(.*)*",
    redirect: "/projects",
  },
];

export const router = createRouter({
  history: createWebHistory("/admin"),
  routes,
});

/**
 * 登录守卫。
 *
 * @intent 依赖 `session.ready`：应用启动时的自检是异步的，若在它完成之前
 *         就放行/拦截，会得到一次误跳转（先跳到 /login 再跳回来，
 *         表现为地址栏闪两下）。未就绪时先 `refresh()` 等一个答案。
 *
 * @intent 角色判定必须排在 `ready` 之后：`ready` 为假时 `role` 一定是 `null`，
 *         此时按「非管理员」判断会把管理员也挡在账号页外，表现为
 *         「刷新 /admin/users 时闪回项目页」。
 */
router.beforeEach(async (to) => {
  if (!session.ready) {
    await new Promise<void>((resolve) => {
      const tick = () => (session.ready ? resolve() : setTimeout(tick, 16));
      tick();
    });
  }

  document.title = to.meta.title
    ? `${String(to.meta.title)} · Memora`
    : "Memora (忆庐)";

  if (to.meta.public) {
    // 已登录时访问登录页：直接进控制台，避免「登录两次」的困惑。
    return session.authenticated ? { name: "projects" } : true;
  }

  if (!session.authenticated) {
    return { name: "login", query: { next: to.fullPath } };
  }

  if (to.meta.adminOnly && !isAdmin()) {
    // 静默回到项目页：给一个「无权访问」的错误页会把租户引向
    // 「我是不是该有权限」的困惑，而那个页面对他来说本来就不该存在。
    return { name: "projects" };
  }

  return true;
});