/**
 * 管理接口客户端。
 *
 * @author yujinping
 * @intent 所有取数集中在这里，而不是散在各组件里 `fetch`：控制台有 8 个以上
 *         调用点，散着写会让「401 时该做什么」出现七八种略有差异的实现，
 *         而其中一两种迟早会漏掉跳转登录页——表现为「点了没反应」。
 *
 * @intent 失败时抛出带服务端 `detail` 的错误：本项目所有 4xx 响应都刻意带上
 *         可读原因（见 `reply` 模块），丢掉它只留「请求失败」会让控制台
 *         比 curl 更难用。
 */

/** 管理接口前缀。与 `routes::create_app` 的 nest 路径一致。 */
const API = "/api/v1";

/** 服务端错误体。 */
interface ErrorBody {
  error?: string;
  detail?: string;
}

/** 调用失败时抛出的错误。 */
export class ApiError extends Error {
  /** HTTP 状态码（网络层失败时为 0） */
  readonly status: number;
  /** 服务端给出的错误代号（如 `weak_password`），无则为空串 */
  readonly code: string;

  constructor(message: string, status: number, code: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }
}

/** 会话失效时执行的回调，由 `main.ts` 注册（跳转登录页）。 */
let onUnauthorized: (() => void) | null = null;

/**
 * 注册 401 处理器。
 *
 * @intent 用回调而不是在客户端里 `location.assign`：路由跳转归 router 管，
 *         客户端直接改 location 会绕开 router 的状态，导致登录后回到
 *         一个 router 认为「你在首页」的界面。
 */
export function setUnauthorizedHandler(handler: () => void): void {
  onUnauthorized = handler;
}

/** 发起一次 JSON 请求。 */
async function request<T>(
  method: string,
  path: string,
  body?: unknown,
): Promise<T> {
  const init: RequestInit = {
    method,
    // 会话凭据走 Cookie，必须显式带上；同源请求默认也会带，但写出来
    // 才能在跨子域部署时保持正确。
    credentials: "same-origin",
    headers: {},
  };

  if (body !== undefined) {
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(body);
  }

  let response: Response;
  try {
    response = await fetch(API + path, init);
  } catch (err) {
    // 网络层失败（服务未启动、连接被重置）：这不是 401，不该触发跳登录页。
    throw new ApiError(
      `无法连接服务：${err instanceof Error ? err.message : String(err)}`,
      0,
      "network_error",
    );
  }

  // 204 / 空体：登录登出等接口没有响应体，不能对空串调 JSON.parse。
  const text = await response.text();
  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      // 非 JSON 响应（例如反代返回的 HTML 错误页）保留原文，便于定位部署问题。
      payload = text;
    }
  }

  if (!response.ok) {
    if (response.status === 401) {
      onUnauthorized?.();
    }
    const errorBody = (payload ?? {}) as ErrorBody;
    const detail =
      errorBody.detail ||
      errorBody.error ||
      (typeof payload === "string" ? payload : "") ||
      `HTTP ${response.status}`;
    throw new ApiError(detail, response.status, errorBody.error ?? "");
  }

  return payload as T;
}

// ---------------------------------------------------------------- 类型

/** 角色标识。 */
export type Role = "admin" | "user";

/** 会话自检结果。 */
export interface SessionStatus {
  console_login_enabled: boolean;
  authenticated: boolean;
  user: string | null;
  /**
   * 当前身份的角色；未登录为 `null`。
   *
   * @intent 角色**必须**由服务端给出，前端不得从用户名或账号数推算：
   *         控制台的导航（是否显示「账号」页）依赖它，一旦前端自行判断，
   *         就会出现「页面显示可点、后端 403」的错位。注意它只影响呈现，
   *         真正的授权始终在服务端。
   */
  role: Role | null;
  users: number;
}

/** 登录结果。 */
export interface LoginResult {
  user: string;
  role: Role;
  expires_in_secs: number;
}

/** 项目登记项。 */
export interface Project {
  project_id: string;
  db_path: string;
  backend: string;
  created_at: number;
  /**
   * 归属账号；空串 = 无主（迁移前遗留，仅管理员可见）。
   *
   * @intent 管理员会把「归属指向不存在账号」的项目（例如账号已被删除）
   *         在界面上标为失效，故这个字段是自由字符串而不是枚举。
   */
  owner: string;
}

/** 项目用量统计。 */
export interface ProjectStats {
  project_id: string;
  backend: string;
  entities: number;
  relations: number;
  observations: number;
  handoffs: number;
}

/** 观测。 */
export interface Observation {
  id: number;
  content: string;
  created_at: number;
  source: string;
  entity_name: string;
}

/** 实体（含观测）。 */
export interface Entity {
  name: string;
  entity_type: string;
  created_at: number;
  source: string;
  observations: Observation[];
}

/** 关系。 */
export interface Relation {
  id: number;
  from_name: string;
  to_name: string;
  relation_type: string;
  created_at: number;
  source: string;
}

/** 全图快照。 */
export interface Graph {
  project_id: string;
  entities: Entity[];
  relations: Relation[];
}

/** 新建项目的请求体。 */
export interface CreateProjectBody {
  project_id?: string;
  backend?: string;
}

/** 新建 / 轮换 token 的结果（两者形状一致）。 */
export interface ProjectToken {
  project_id: string;
  token: string;
  backend: string;
  /** 归属账号（新建时即创建者；管理员代建时是管理员自己） */
  owner: string;
}

/** 交接摘要（列表用）。 */
export interface HandoffSummary {
  task_id: string;
  revision: number;
  goal: string;
  state_excerpt: string;
  client: string;
  session_id: string;
  created_at: number;
  acked: boolean;
  acked_by: string;
  next_step_count: number;
}

/** 交接完整记录（详情用）。 */
export interface Handoff {
  task_id: string;
  revision: number;
  goal: string;
  state: string;
  next_steps: string[];
  files: string[];
  decisions: string[];
  open_questions: string[];
  client: string;
  session_id: string;
  created_at: number;
  acked_at: number | null;
  acked_by: string;
}

/** 控制台账号。 */
export interface ConsoleUser {
  username: string;
  /** 角色：决定数据范围。管理员管全部，普通账号只管自己名下的项目 */
  role: Role;
  created_at: number;
  updated_at: number;
}

// ---------------------------------------------------------------- 接口

/** 会话自检（公开）。 */
export function getSession(): Promise<SessionStatus> {
  return request("GET", "/session");
}

/** 登录。 */
export function login(username: string, password: string): Promise<LoginResult> {
  return request("POST", "/session", { username, password });
}

/** 登出（幂等：未登录也返回 204）。 */
export function logout(): Promise<void> {
  return request("DELETE", "/session");
}

/** 项目列表。 */
export function listProjects(): Promise<Project[]> {
  return request("GET", "/projects");
}

/** 新建项目。 */
export function createProject(body: CreateProjectBody): Promise<ProjectToken> {
  return request("POST", "/projects", body);
}

/** 注销项目（物理删除数据）。 */
export function deleteProject(
  projectId: string,
): Promise<{ project_id: string; data_removed: boolean }> {
  return request("DELETE", `/projects/${encodeURIComponent(projectId)}`);
}

/** 轮换项目 token。 */
export function rotateToken(projectId: string): Promise<ProjectToken> {
  return request("POST", `/projects/${encodeURIComponent(projectId)}/token`);
}

/** 项目用量统计。 */
export function getStats(projectId: string): Promise<ProjectStats> {
  return request("GET", `/projects/${encodeURIComponent(projectId)}/stats`);
}

/** 项目全图（记忆浏览）。 */
export function getGraph(projectId: string): Promise<Graph> {
  return request("GET", `/projects/${encodeURIComponent(projectId)}/graph`);
}

/**
 * 交接列表。
 *
 * @intent `unacked` 参数默认 `true`（只列待接手）：这是控制台打开交接页时
 *         最常想看的集合。看历史需要显式切换——两者用同一个接口，
 *         避免「列表接口」和「历史接口」两套分页语义。
 */
export function listHandoffs(
  projectId: string,
  options: { unacked?: boolean; limit?: number; prefix?: string } = {},
): Promise<{ handoffs: HandoffSummary[] }> {
  const params = new URLSearchParams();
  if (options.unacked === false) params.set("unacked", "0");
  if (options.limit !== undefined) params.set("limit", String(options.limit));
  if (options.prefix) params.set("prefix", options.prefix);
  const query = params.toString();
  return request(
    "GET",
    `/projects/${encodeURIComponent(projectId)}/handoffs${query ? `?${query}` : ""}`,
  );
}

/** 交接详情（取最新修订）。 */
export function getHandoff(projectId: string, taskId: string): Promise<Handoff> {
  return request(
    "GET",
    `/projects/${encodeURIComponent(projectId)}/handoffs/${encodeURIComponent(taskId)}`,
  );
}

/** 账号列表。 */
export function listUsers(): Promise<ConsoleUser[]> {
  return request("GET", "/users");
}

/**
 * 新建账号。
 *
 * @intent `role` 省略时由服务端落在普通账号上（最小权限缺省）。控制台总是
 *         显式传值，但把它做成可选参数是为了让「建普通账号」这条高频路径
 *         不必写 `"user"`——高频动作的写法越短，越不会有人图省事去传 admin。
 */
export function createUser(
  username: string,
  password: string,
  role: Role = "user",
): Promise<ConsoleUser> {
  return request("POST", "/users", { username, password, role });
}

/** 删除账号。 */
export function deleteUser(
  username: string,
): Promise<{ username: string; sessions_revoked: number }> {
  return request("DELETE", `/users/${encodeURIComponent(username)}`);
}

/** 修改账号口令。 */
export function updatePassword(
  username: string,
  password: string,
): Promise<{ username: string }> {
  return request("PUT", `/users/${encodeURIComponent(username)}`, { password });
}

/**
 * 修改账号角色。
 *
 * @intent 服务端会顺手刷新该账号**在线会话**的角色快照，故 `sessions_updated`
 *         是「即时生效了多少个会话」而非「改了几行库」。界面据此告知
 *         「对方无需重新登录即已生效」。
 */
export function updateRole(
  username: string,
  role: Role,
): Promise<{ username: string; role: Role; sessions_updated: number }> {
  return request("PUT", `/users/${encodeURIComponent(username)}/role`, { role });
}