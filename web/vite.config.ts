import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import tailwindcss from "@tailwindcss/vite";

/**
 * 构建配置。
 *
 * @intent `base: "/"` 与产物目录 `../src/console_dist`：后端把 `console_dist/`
 *         整个目录内嵌，并在 `/assets/*` 上直接吐字节，因此产物路径必须与
 *         服务端的路由前缀一致。改这里就要同步改 `src/console.rs` 的 `asset`。
 *
 * @intent 不产出 sourcemap：产物会进二进制并随每次发布分发给用户，
 *         sourcemap 只会把源码与体积一起带出去，而自托管场景没有
 *         「线上排障看 map」的需求（真要看，本地 `pnpm dev` 即可）。
 *
 * @intent `emptyOutDir` 必须开着且目录在 `root` 之外：`src/console_dist` 由
 *         `build.rs` 扫描，残留的旧指纹文件会被一起打进二进制——表现为
 *         「二进制比预期大，且里面有两份 js」。
 */
export default defineConfig({
  plugins: [vue(), tailwindcss()],
  base: "/",
  build: {
    outDir: "../src/console_dist",
    emptyOutDir: true,
    sourcemap: false,
    // 单个资源内联会把体积藏进 HTML，而 HTML 不可缓存；控制台的体积很小，
    // 保持所有资源独立成文件对缓存策略更友好。
    assetsInlineLimit: 0,
    rollupOptions: {
      output: {
        // 固定文件名而非指纹：产物只有一 js 一 css，指纹带来的缓存收益
        // 换不来「每次构建都改变文件名」的 diff 噪声。
        entryFileNames: "assets/[name].js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/[name][extname]",
      },
    },
  },
  server: {
    // 开发期把 API 与 MCP 打到本机 memora，前端热更新时才连得到真数据。
    proxy: {
      "/api": "http://127.0.0.1:6789",
      "/mcp": "http://127.0.0.1:6789",
    },
  },
});