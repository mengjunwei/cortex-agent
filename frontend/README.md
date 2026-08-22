# cortex-agent 前端

cortex-agent 的 Web 前端（Vue 3 + Element Plus + Pinia + vue-router，Vite 构建）。

## 与后端的关系（非前后分离部署）

`npm run build` 的产物输出到项目根的 [`../static/`](../static)，由 Rust 后端（`cortex-agent` 二进制）静态托管——**部署时只需运行后端，不单独起前端服务**。

开发联调时，`npm run dev` 起在 5173 端口，`/api` 请求经 Vite 开发代理转发到后端 `http://127.0.0.1:8095`（见 `vite.config.js` 的 `server.proxy`）。

## 技术栈

- Vue 3.5（`<script setup>` SFC）
- Element Plus（UI 组件库，按需自动导入）
- Pinia（状态管理）
- vue-router（路由）
- 原生 fetch（HTTP：`src/api/index.js` 统一封装 GraphQL 单入口 `POST /api/graphql`、SSE 流式 `runSse` 与少量 REST 例外；未用 axios）
- marked / marked-highlight + highlight.js（Markdown 渲染 + 代码高亮）
- DOMPurify（Markdown/HTML 渲染前 XSS 消毒）
- lucide-vue-next（图标）

## 常用脚本

```bash
npm install       # 安装依赖（当前以 npm 为准——package-lock.json 随 package.json 同步更新；
                  #  目录里的 pnpm-lock.yaml 停留在初始提交，待清理）
npm run dev       # 本地开发（5173，/api 代理到后端 8095）
npm run build     # 构建到 ../static/ 供后端托管
npm run preview   # 本地预览构建产物
```

> 后端启动与配置见项目根 [DEPLOY.md](../DEPLOY.md)，接口契约见 [docs/api.md](../docs/api.md)。
