/**
 * 时间与文本格式化。
 *
 * @author yujinping
 * @intent 服务端一律返回 Unix 秒（存储层就是 Unix 秒，转换只做一次，
 *         避免「有些字段是秒、有些是毫秒」的经典混乱）。展示层的格式化
 *         集中在这里，使全站的时间写法一致。
 */

/**
 * Unix 秒 → `yyyy-MM-dd HH:mm` 本地时间。
 *
 * @intent `0` 与负数一律显示 `—` 而不是 1970 年：它是「时间未知 / 未设置」
 *         的哨兵值（例如从未接手的 `acked_at`），把它渲染成 1970 会让人以为
 *         有数据，而实际是「没有」。
 */
export function formatTime(seconds: number | null | undefined): string {
  if (!seconds || seconds <= 0) return "—";
  const date = new Date(seconds * 1000);
  if (Number.isNaN(date.getTime())) return "—";

  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}`
  );
}

/** 仅日期：`yyyy-MM-dd`。 */
export function formatDate(seconds: number | null | undefined): string {
  const full = formatTime(seconds);
  return full === "—" ? full : full.slice(0, 10);
}

/**
 * 相对时间（「3 分钟前」）。
 *
 * @intent 列表里用它、详情里用绝对时间：列表需要「新旧感」一眼可辨，
 *         详情需要精确到分钟以便对照日志。两处混用会让其中一处总是别扭。
 */
export function formatRelative(seconds: number | null | undefined): string {
  if (!seconds || seconds <= 0) return "—";
  const delta = Math.floor(Date.now() / 1000) - seconds;
  if (delta < 60) return "刚刚";
  if (delta < 3600) return `${Math.floor(delta / 60)} 分钟前`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} 小时前`;
  if (delta < 86400 * 30) return `${Math.floor(delta / 86400)} 天前`;
  return formatDate(seconds);
}

/** 截断文本并补省略号。 */
export function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return text.slice(0, max).trimEnd() + "…";
}

/**
 * 复制文本到剪贴板。
 *
 * @intent 返回 `boolean` 而不是抛错：调用方只想决定「提示复制成功还是
 *         提示手动选中」。剪贴板 API 在非 HTTPS 下会被浏览器禁用，
 *         而自托管的常见形态正是 `http://localhost:6789`——这不是异常，
 *         是常态，故按「两种正常路径」处理。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}