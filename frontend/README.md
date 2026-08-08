# cortex-agent 前端

cortex-agent 的 Web 前端（Vue 3 + Element Plus + Pinia + vue-router，Vite 构建）。

## 与后端的关系（非前后分离部署）

`pnpm build` 的产物输出到项目根的 [`../static/`](../static)，由 Rust 后端（`cortex-agent` 二进制）静态托管——**部署时只需运行后端，不单独起前端服务**。

开发联调时，`pnpm dev` 起在 5173 端口，`/api` 请求经 Vite 开发代理转发到后端 `http://127.0.0.1:8095`（见 `vite.config.js` 的 `server.proxy`）。

## 技术栈

- Vue 3.5（`<script setup>` SFC）
- Element Plus（UI 组件库，按需自动导入）
- Pinia（状态管理）
- vue-router（路由）
- axios（HTTP，对接后端 GraphQL 单入口 + SSE）
- marked / marked-highlight + highlight.js（Markdown 渲染 + 代码高亮）

## 常用脚本

```bash
pnpm install      # 安装依赖
pnpm dev          # 本地开发（5173，/api 代理到后端 8095）
pnpm build        # 构建到 ../static/ 供后端托管
pnpm preview      # 本地预览构建产物
```

> 后端启动与配置见项目根 [DEPLOY.md](../DEPLOY.md)，接口契约见 [docs/api.md](../docs/api.md)。
