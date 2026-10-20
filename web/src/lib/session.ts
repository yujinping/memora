/**
 * 登录态。
 *
 * @author yujinping
 * @intent 只用一个 `reactive` 对象而不是引入 Pinia：控制台的状态总量是
 *         「当前是谁」加几个页面级列表，引入状态库会让「状态从哪来」
 *         多一层间接，而收益为零。等真的出现跨页共享的复杂状态再说。
 *
 * @intent **不持久化任何凭据**：会话 id 在 HttpOnly Cookie 里，脚本读不到也
 *         不该读。localStorage 里只留「上次登录的用户名」，仅用于登录框预填
 *         ——它不是凭据，泄漏它不构成任何风险。
 */

import { reactive, readonly } from "vue";
import * as api from "./api";

/** 登录态。 */
interface SessionState {
  /** 是否已通过鉴权（由 `/session` 自检结果决定，不由前端猜） */
  authenticated: boolean;
  /** 当前用户名 */
  user: string | null;
  /** 系统中是否存在可登录账号 */
  loginEnabled: boolean;
  /** 账号总数 */
  userCount: number;
  /** 是否已完成首次自检（未完成时不应渲染登录框，否则会闪一下） */
  ready: boolean;
}

const state = reactive<SessionState>({
  authenticated: false,
  user: null,
  loginEnabled: true,
  userCount: 0,
  ready: false,
});

/** 只读的登录态（组件不能直接改，必须经下面的动作）。 */
export const session = readonly(state);

/** 上次登录的用户名（仅用于预填）。 */
const LAST_USER_KEY = "memora.console.lastUser";

/** 读出上次登录的用户名。 */
export function lastUser(): string {
  try {
    return localStorage.getItem(LAST_USER_KEY) ?? "";
  } catch {
    // 隐私模式下 localStorage 可能抛错（Safari），预填失败不该影响登录。
    return "";
  }
}

/** 记录本次登录的用户名。 */
function rememberUser(user: string): void {
  try {
    localStorage.setItem(LAST_USER_KEY, user);
  } catch {
    /* 无法持久化只是少一次预填，不是错误 */
  }
}

/**
 * 自检：问服务端「我现在是谁」。
 *
 * @intent 这是页面启动的**唯一**真相源：不读 Cookie（读不到）、不猜
 *         （会闪）、不信本地标记（会被伪造）。401 时不报错，只把状态
 *         置为未登录——自检本身失败是正常的未登录状态。
 */
export async function refresh(): Promise<void> {
  try {
    const status = await api.getSession();
    state.authenticated = status.authenticated;
    state.user = status.user;
    state.loginEnabled = status.console_login_enabled;
    state.userCount = status.users;
  } catch {
    // 服务不可达时保守地按未登录处理：给用户看登录框，让他在那里
    // 看到「无法连接服务」的具体错误，比停在白屏上好。
    state.authenticated = false;
    state.user = null;
    state.loginEnabled = true;
    state.userCount = 0;
  } finally {
    state.ready = true;
  }
}

/** 登录。 */
export async function signIn(username: string, password: string): Promise<void> {
  const result = await api.login(username, password);
  state.authenticated = true;
  state.user = result.user;
  rememberUser(result.user);
  // 账号数可能刚变化（首次登录后有人建了新账号），顺手刷新一次。
  void refreshUserCount();
}

/** 登出。 */
export async function signOut(): Promise<void> {
  try {
    await api.logout();
  } catch {
    // 登出失败也要把本地状态清掉：服务端会话可能已过期，重登一次即可收敛。
  }
  state.authenticated = false;
  state.user = null;
}

/** 仅刷新账号数（登录成功后调用，避免整轮自检）。 */
async function refreshUserCount(): Promise<void> {
  try {
    const status = await api.getSession();
    state.userCount = status.users;
    state.loginEnabled = status.console_login_enabled;
  } catch {
    /* 计数只是展示信息，取不到就沿用旧值 */
  }
}

/** 供账号管理页在增删账号后同步顶部计数。 */
export { refreshUserCount };