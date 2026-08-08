"""
Document Loader MCP — Streamable HTTP 网关

把 stdio 版 awslabs.document-loader-mcp-server 暴露为 **标准 MCP Streamable HTTP 协议**
（单端点 POST /mcp + Mcp-Session-Id header），完全兼容 RMCP 的 streamable_http_client。

== 为什么重写（旧版的三个致命问题）==
1. 旧版用 GET /mcp/stream（建连拿 sid）+ POST /mcp 双端点模式，是非标准协议；
   RMCP 按标准协议直接 POST /mcp 握手 → 命中旧版 "No active SSE stream" 的 400。
2. 旧版要求 sid query 参数，标准协议没有 → 422 missing sid。
3. 旧版单 stdio 后端服务所有会话、靠 rpc_id 路由：stdio MCP server 是单会话有状态的，
   并发 initialize 会互相串。

== 新版做法 ==
- 单端点 POST /mcp：接收 JSON-RPC，转发到 stdio 后端，原样返回 JSON-RPC 响应，
  响应头带 Mcp-Session-Id。
- 每个 Mcp-Session 独立 spawn 一个 awslabs stdio 后端进程 → 会话隔离、状态正确。
- GET /mcp：SSE 保活通道（RMCP 可能建立用于接收服务端推送；本网关仅心跳）。
- DELETE /mcp：关闭会话并回收后端进程。

运行：python doc_mcp.py   （监听 0.0.0.0:10919，端点 /mcp）
依赖：fastapi uvicorn sse-starlette（与旧版一致，无需新增）
"""
import asyncio
import json
import uuid
from typing import Dict, Optional

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse
from sse_starlette.sse import EventSourceResponse
import uvicorn

# stdio MCP 后端启动命令
MCP_CMD = ["uvx", "awslabs.document-loader-mcp-server@latest"]
# uvx 首次拉取较慢，给足超时
REQUEST_TIMEOUT = 180

app = FastAPI(title="Document Loader MCP (Streamable HTTP Gateway)")


class McpSession:
    """一个 HTTP 会话对应一个独立的 stdio 后端进程，保证会话隔离、状态正确。"""

    def __init__(self, sid: str):
        self.sid = sid
        self.proc: Optional[asyncio.subprocess.Process] = None
        self.lock = asyncio.Lock()          # 串行化对后端 stdin 的写入（stdio 是单连接）
        self.pending: Dict = {}             # rpc_id -> asyncio.Future（响应路由）
        self._stdout_task: Optional[asyncio.Task] = None
        self._stderr_task: Optional[asyncio.Task] = None

    async def start(self) -> None:
        self.proc = await asyncio.create_subprocess_exec(
            *MCP_CMD,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        self._stdout_task = asyncio.create_task(self._read_stdout())
        self._stderr_task = asyncio.create_task(self._drain_stderr())

    async def _read_stdout(self) -> None:
        assert self.proc and self.proc.stdout
        while True:
            try:
                line = await self.proc.stdout.readline()
            except Exception:
                break
            if not line:
                break
            try:
                msg = json.loads(line.decode(errors="replace"))
            except Exception:
                continue
            if not isinstance(msg, dict):
                continue
            rpc_id = msg.get("id")
            if rpc_id is None:
                continue  # 通知类（无 id）：本网关按请求-响应工作，不下发
            fut = self.pending.pop(rpc_id, None)
            if fut and not fut.done():
                fut.set_result(msg)

    async def _drain_stderr(self) -> None:
        """消费后端 stderr，避免缓冲区写满阻塞子进程。"""
        assert self.proc and self.proc.stderr
        while True:
            try:
                line = await self.proc.stderr.readline()
            except Exception:
                break
            if not line:
                break
            print(f"[backend:{self.sid[:8]}] {line.decode(errors='replace').rstrip()}")

    async def request(self, payload: dict) -> dict:
        assert self.proc and self.proc.stdin
        rpc_id = payload.get("id")
        fut = None
        if rpc_id is not None:
            fut = asyncio.get_event_loop().create_future()
            self.pending[rpc_id] = fut
        async with self.lock:
            self.proc.stdin.write((json.dumps(payload, ensure_ascii=False) + "\n").encode())
            await self.proc.stdin.drain()
        if fut is None:
            # notification（无 id）：无响应
            return {}
        return await asyncio.wait_for(fut, timeout=REQUEST_TIMEOUT)

    async def stop(self) -> None:
        for t in (self._stdout_task, self._stderr_task):
            if t:
                t.cancel()
        if self.proc and self.proc.returncode is None:
            try:
                self.proc.terminate()
                await asyncio.wait_for(self.proc.wait(), timeout=5)
            except Exception:
                try:
                    self.proc.kill()
                except Exception:
                    pass


sessions: Dict[str, McpSession] = {}
sessions_lock = asyncio.Lock()


async def _get_or_create_session(session_id: Optional[str]):
    """首次请求（无 session header）创建新会话；后续复用。返回 (session, sid)。"""
    async with sessions_lock:
        if session_id and session_id in sessions:
            return sessions[session_id], session_id
        new_id = str(uuid.uuid4())
        s = McpSession(new_id)
        await s.start()
        sessions[new_id] = s
        return s, new_id


@app.post("/mcp")
async def mcp_post(request: Request):
    try:
        payload = await request.json()
    except Exception:
        return JSONResponse(
            {"jsonrpc": "2.0", "error": {"code": -32700, "message": "Parse error"}},
            status_code=400,
        )

    session_id = request.headers.get("mcp-session-id")
    try:
        s, sid = await _get_or_create_session(session_id)
    except Exception as e:
        return JSONResponse(
            {"jsonrpc": "2.0", "error": {"code": -32603, "message": f"Backend start failed: {e}"}},
            status_code=503,
        )

    try:
        response = await s.request(payload)
    except asyncio.TimeoutError:
        return JSONResponse(
            {"jsonrpc": "2.0", "id": payload.get("id"),
             "error": {"code": -32000, "message": "Backend timeout"}},
            headers={"mcp-session-id": sid},
            status_code=504,
        )
    except Exception as e:
        return JSONResponse(
            {"jsonrpc": "2.0", "id": payload.get("id"),
             "error": {"code": -32603, "message": str(e)}},
            headers={"mcp-session-id": sid},
            status_code=502,
        )

    # 标准 Streamable HTTP 响应：JSON-RPC body + Mcp-Session-Id header
    return JSONResponse(response, headers={"mcp-session-id": sid})


@app.get("/mcp")
async def mcp_get(request: Request):
    """SSE 保活通道：RMCP 可能建立用于接收服务端推送；本网关按请求-响应工作，仅心跳。"""
    async def gen():
        try:
            while True:
                await asyncio.sleep(15)
                yield {"event": "ping", "data": ""}
        except asyncio.CancelledError:
            return
    return EventSourceResponse(gen())


@app.delete("/mcp")
async def mcp_delete(request: Request):
    session_id = request.headers.get("mcp-session-id")
    async with sessions_lock:
        s = sessions.pop(session_id, None)
    if s:
        await s.stop()
    return JSONResponse({"status": "closed"})


@app.on_event("shutdown")
async def shutdown():
    async with sessions_lock:
        items = list(sessions.items())
        sessions.clear()
    for _, s in items:
        await s.stop()


if __name__ == "__main__":
    uvicorn.run(app, host="0.0.0.0", port=10919, log_level="info")
