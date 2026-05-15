# SOP 异步任务模式 — 小程序前端对接 (2026-05-15)

> **替代说明**: 本文档替代 [policy-match-frontend-integration.md](policy-match-frontend-integration.md) 中的"长 HTTP 同步等待"模式。**新架构异步化**: HTTP 请求 5-10 秒内返回 task_id,任务在后台跑,前端通过 SSE + 状态查询取结果。
>
> **适用范围**: 所有 SOP — `policy-match` (政策匹配)、即将上线的 `qualification-check` (资质评估) 以及未来扩展的 SOP。统一接口 `POST /sop/{sop_name}/run`。

---

## 1. 为什么改成异步

旧同步模式痛点:
- HTTP 挂 6-10 分钟,小程序进后台 = `wx.request` 被系统挂起 = 用户回来啥都没了
- 退出小程序再进,SOP 状态丢失,只能从头再算
- 一次匹配跑完要 0.2-0.5 元成本,重复触发烧钱

异步模式解决:
- 同企业 7 天内有结果 → 5 秒直出(缓存),不重复跑
- 缓存 miss → 5-10 秒返回 task_id,后台跑;前端用 SSE 监听完成事件,**切后台/重进都能恢复**
- 任务状态写库,服务器知道用户有正在跑的任务,重进时主动 catch up

---

## 2. API 速查

| 接口 | 方法 | 用途 |
|---|---|---|
| `/sop/{sop_name}/run` | POST | 启动 SOP(缓存 hit 直出 / miss 入队) |
| `/sop/task/{task_id}` | GET | 查任务状态 + 结果 |
| `/me/sop/tasks` | GET | 查当前用户进行中/完成的任务列表(重进聊天页用) |
| `/events?token=...` | GET (SSE) | 实时事件流(任务完成信号) |

所有接口都需 `Authorization: Bearer <user-paired-token>`(沿用现有 wx.login → exchange 流程拿到的 token)。

### 2.1 `POST /sop/{sop_name}/run`

启动一个 SOP。**`sop_name` 是 path 参数**,目前支持:
- `policy-match` — 政策匹配
- `qualification-check` — 资质评估(下一期上线)

```
POST https://<clawops-gateway>/sop/policy-match/run
Headers:
  Authorization: Bearer <user-paired-token>
  Content-Type: application/json
Body:
  {
    "enterprise_name": "中拓产业云(北京)科技服务有限公司",  // 可选, 没传走 openid 关联的默认企业
    "force_refresh": false   // 可选, true 时跳过缓存强制重算
  }
```

**响应分两种**:

**Case A: 缓存命中**(7 天内同企业算过) — HTTP 200:
```json
{
  "status": "cached",
  "task_id": "tsk_abc123",          // 历史任务的 id (可再通过 /sop/task/{id} 拿)
  "result": {
    "sop_name": "policy-match",
    "response_text": "已为 **中拓产业云** 匹配 10 条适用政策...TOP 3:...→ [小程序政策匹配页](/pages/recommendation/index?id=100)",
    "deeplink": "/pages/recommendation/index?id=100",
    "qualification_enterprise_id": 100,
    "enterprise_id": 28
  },
  "cached_at": "2026-05-14T16:30:00Z"
}
```
- 前端直接 `displayMessage(result.response_text)`,不需要 loading。
- 拦截 markdown link 跳 `wx.navigateTo({ url: result.deeplink })`。

**Case B: 缓存 miss**(新任务入队) — HTTP 202:
```json
{
  "status": "queued",
  "task_id": "tsk_def456",
  "sop_name": "policy-match",
  "estimated_seconds": 540,   // 预估总耗时(政策匹配 ~9 分钟,资质评估 ~3 分钟)
  "queued_at": "2026-05-15T08:00:00Z"
}
```
- 前端**记住 `task_id`**(本地变量 + 持久化到 wx.setStorage,key 例如 `sop_active_task_<sop_name>`)。
- 切到 long-loading UI(详见 §3)。
- 监听 SSE 等 `agent_end` 事件。

### 2.2 `GET /sop/task/{task_id}`

任意时刻查询任务进度。

```
GET https://<clawops-gateway>/sop/task/tsk_def456
Headers: Authorization: Bearer <user-paired-token>
```

响应:
```json
{
  "task_id": "tsk_def456",
  "sop_name": "policy-match",
  "status": "running",          // pending | running | done | failed
  "result": null,               // status=done 时含 result 对象,同 §2.1 Case A 的 result
  "error": null,                // status=failed 时含人类可读错误
  "enterprise_id": 28,
  "qualification_enterprise_id": 100,
  "created_at": "2026-05-15T08:00:00Z",
  "updated_at": "2026-05-15T08:03:14Z"
}
```

**前端调用时机**:
1. SSE 收到 `agent_end` → 立即调一次,看 status 是 done/failed/running
2. 切后台返回(`onShow`)→ 调一次 catch up
3. 重进聊天页 → 先 GET `/me/sop/tasks?status=running`,如有,接着 GET `/sop/task/{id}` 拿状态

### 2.3 `GET /me/sop/tasks`

列当前用户的任务历史,可按 status / sop_name 过滤。

```
GET https://<clawops-gateway>/me/sop/tasks?status=running&limit=10
GET https://<clawops-gateway>/me/sop/tasks?sop_name=policy-match&limit=20
```

响应:
```json
{
  "tasks": [
    {
      "task_id": "tsk_def456",
      "sop_name": "policy-match",
      "status": "running",
      "created_at": "2026-05-15T08:00:00Z",
      "updated_at": "2026-05-15T08:03:14Z"
    }
  ]
}
```

**进聊天页(`onLoad`/`onShow`)时调一次**,如果有 `status=running` 的任务,直接恢复 loading UI + 监听 SSE / 轮询。

### 2.4 `GET /events?token=...` (SSE,沿用)

前端在聊天页 onLoad 时建立 SSE 长连接(用 `wx.request` + `enableChunked: true`),监听这些事件:

| event.type | 用途 |
|---|---|
| `tool_call_start` + `tool="sop_execute"` | LLM 已识别 SOP 意图并启动(可显示"政策匹配中...") |
| `agent_end` | 一次 daemon chat 跑完(可能是 SOP done,也可能是 SOP 中间某步完成的提示) |
| `error` | daemon 端报错 |

**收到 `agent_end` 后**: 调 `GET /sop/task/{active_task_id}` 看真实 status。**SSE 自己不带 result**(避免 SSE payload 暴露大数据,且 task 表是 source of truth)。

> 详细 SSE 接入方法(`wx.request` chunked + 重连)见 [policy-match-frontend-integration.md §4](policy-match-frontend-integration.md) §"前端实现 1. 聊天页 onLoad 时建 SSE 长连接",代码逻辑不变,只是接入语义升级。

---

## 3. 前端状态机

```
[idle]
  │ user 在聊天框发"匹配政策"消息
  ▼
POST /sop/policy-match/run
  │
  ├──── 200 status="cached" ─→ displayMessage(result.response_text) → [idle]
  │
  └──── 202 status="queued" + task_id
       │ wx.setStorage("sop_active_task_policy-match", task_id)
       │ switchToLongLoadingUI()
       ▼
   [waiting]
       │
       ├─ SSE event: agent_end
       │   → GET /sop/task/{task_id}
       │       ├── done    → displayMessage(result.response_text) + wx.removeStorageSync → [idle]
       │       ├── failed  → showError(error) + wx.removeStorageSync → [idle]
       │       └── running → 继续 [waiting]
       │
       ├─ onShow (从后台回到前台)
       │   → GET /sop/task/{task_id} 同上分支处理
       │
       └─ onLoad (重新进聊天页)
           → GET /me/sop/tasks?status=running
              ├── 有 → 取最新 task_id 进入 [waiting]
              └── 无 → [idle]
```

### 关键细节

- **任务 ID 持久化**: 用 `wx.setStorage` 存活跃 task_id,跨页面跳转、关闭重开都能恢复。完成后 `wx.removeStorageSync`。
- **进度提示**: 仍然推荐展示"正在分析企业画像 → 匹配政策 → 评估条件"占位文案(从 SOP 启动到完成的 ~5-6 分钟内每 30-60s 切换一次),不需要真实进度(daemon 不发细粒度进度)。
- **轮询兜底(可选)**: SSE 断网恢复期间,前端可以每 30 秒主动调一次 `/sop/task/{task_id}` 作为兜底,避免 SSE 重连失败时永久卡在 loading。
- **防重复触发**: `wx.getStorage` 检测有活跃 task 时,**禁用**"匹配政策"按钮/输入提交,避免双开。

---

## 4. 完整示例代码 (微信小程序)

```javascript
// chat-page.js
const API = 'https://<clawops-gateway>';
let pairedToken = '';
let activeTask = null;
let sseTask = null;

Page({
  data: { loading: false, messages: [] },

  onLoad() {
    pairedToken = wx.getStorageSync('paired_token');
    this.recoverActiveTask();
    this.connectSSE();
  },

  onShow() {
    // 从后台回来时检查活跃任务
    this.checkActiveTaskStatus();
  },

  onUnload() {
    if (sseTask) sseTask.abort();
  },

  // ────── 1. 启动 SOP ──────
  async sendMessage(userInput) {
    const sopName = this.detectSopFromInput(userInput);
    if (!sopName) {
      // 普通 chat,走 /chat (本文档外)
      return this.sendNormalChat(userInput);
    }

    if (this.hasActiveTask(sopName)) {
      wx.showToast({ title: '匹配进行中,请稍候', icon: 'none' });
      return;
    }

    this.setData({ loading: true });
    wx.request({
      url: `${API}/sop/${sopName}/run`,
      method: 'POST',
      header: { 'Authorization': `Bearer ${pairedToken}` },
      data: { enterprise_name: this.data.enterpriseName },
      timeout: 30000,   // 5-10 秒,加余量
      success: (res) => {
        if (res.statusCode === 200 && res.data.status === 'cached') {
          // 缓存命中,直接展示
          this.displayResult(res.data.result);
          this.setData({ loading: false });
        } else if (res.statusCode === 202 && res.data.status === 'queued') {
          // 入队,进 waiting
          activeTask = { task_id: res.data.task_id, sop_name: sopName };
          wx.setStorageSync(`sop_active_task_${sopName}`, res.data.task_id);
          this.showLongLoadingBubble(res.data.estimated_seconds);
        }
      },
      fail: (err) => {
        this.setData({ loading: false });
        wx.showToast({ title: '请求失败,请重试', icon: 'error' });
      }
    });
  },

  // ────── 2. SSE 监听任务完成 ──────
  connectSSE() {
    sseTask = wx.request({
      url: `${API}/events?token=${pairedToken}`,
      method: 'GET',
      enableChunked: true,
    });
    let buffer = '';
    sseTask.onChunkReceived(res => {
      buffer += new TextDecoder().decode(new Uint8Array(res.data));
      const frames = buffer.split('\n\n');
      buffer = frames.pop();
      for (const frame of frames) {
        const m = frame.match(/^data:\s*(.+)$/m);
        if (!m) continue;
        try {
          const ev = JSON.parse(m[1]);
          if (ev.type === 'agent_end' && activeTask) {
            this.checkActiveTaskStatus();   // 一次 daemon chat 结束就 catch up
          }
        } catch(e) {}
      }
    });
  },

  // ────── 3. catch up 任务状态 ──────
  checkActiveTaskStatus() {
    if (!activeTask) return;
    wx.request({
      url: `${API}/sop/task/${activeTask.task_id}`,
      header: { 'Authorization': `Bearer ${pairedToken}` },
      success: (res) => {
        const task = res.data;
        if (task.status === 'done') {
          this.displayResult(task.result);
          this.clearActiveTask();
        } else if (task.status === 'failed') {
          wx.showToast({ title: task.error || '匹配失败', icon: 'error' });
          this.clearActiveTask();
        }
        // running: 不动,继续 waiting
      }
    });
  },

  // ────── 4. 进页面时恢复活跃任务 ──────
  async recoverActiveTask() {
    wx.request({
      url: `${API}/me/sop/tasks?status=running&limit=1`,
      header: { 'Authorization': `Bearer ${pairedToken}` },
      success: (res) => {
        const tasks = res.data.tasks;
        if (tasks && tasks.length > 0) {
          activeTask = { task_id: tasks[0].task_id, sop_name: tasks[0].sop_name };
          this.showLongLoadingBubble();
        }
      }
    });
  },

  // ────── helpers ──────
  detectSopFromInput(input) {
    if (/政策|匹配|补贴|申报/.test(input)) return 'policy-match';
    if (/资质|评估|认证|高新|专精特新/.test(input)) return 'qualification-check';
    return null;
  },

  hasActiveTask(sopName) {
    return !!wx.getStorageSync(`sop_active_task_${sopName}`);
  },

  clearActiveTask() {
    if (activeTask) wx.removeStorageSync(`sop_active_task_${activeTask.sop_name}`);
    activeTask = null;
    this.setData({ loading: false });
  },

  displayResult(result) {
    this.appendMessage({ role: 'assistant', text: result.response_text });
    // 链接拦截:用户点 result.deeplink → wx.navigateTo
  },

  showLongLoadingBubble(estimatedSeconds = 540) {
    this.appendMessage({
      role: 'assistant',
      text: `政策顾问正在为您匹配...(预计 ${Math.ceil(estimatedSeconds / 60)} 分钟)`,
      loading: true,
    });
  },
});
```

---

## 5. 错误处理

| 现象 | 原因 | 前端处理 |
|---|---|---|
| `POST /sop/X/run` 返回 5xx | clawops 服务异常 | 提示"服务暂时不可用",30 秒后允许重试 |
| `GET /sop/task/{id}` 一直返回 `running` 超过 15 分钟 | daemon hang | 提示"任务超时,请重试" + 调 `DELETE /sop/task/{id}` (后端实现) 主动取消 |
| `status=failed` 错误为 "All providers/models failed" | LLM 服务侧抖动 | 提示"服务繁忙,请稍后重试" |
| `status=failed` 错误含 "enterprise_name" | 企业未注册 | 提示"未找到企业,请补全企业资料" + 引导小程序企业页 |
| SSE 连接断 | 网络/微信冻结 | onShow 重连 + 主动调 `/sop/task/{id}` catch up |

---

## 6. 扩展到新 SOP

未来添加资质评估、规模评估、补贴申请等 SOP,前端 **无需改 API 调用代码**,只需:

1. **在 `detectSopFromInput()` 加关键词分支** — 识别用户意图触发哪个 sop_name
2. **(可选) 加 sop_name → estimated_seconds 映射** — 不同 SOP 预估时长不同(policy-match 9 分钟,qualification-check 3 分钟)
3. **(可选) 加 sop_name → result 渲染逻辑** — 各 SOP 的 result schema 大致一致(都含 response_text + deeplink),如有特殊字段需要前端额外渲染时再细分

`POST /sop/{sop_name}/run` + `GET /sop/task/{id}` 完全通用,后端按 sop_name 路由到对应 daemon SOP。

---

## 7. 后端实现状态 + 上线时间

**当前状态**: 设计中。生产仍跑同步模式(`POST /chat` 9 分钟挂),按 [policy-match-frontend-integration.md](policy-match-frontend-integration.md) 旧版接入。

**预计上线**: 后端 1-1.5 人天工作量(clawops 加 `sop_tasks` 表 + 异步任务 spawn + 缓存逻辑)。

**联调路径**:
1. 后端先上 `POST /sop/policy-match/run` + `GET /sop/task/{id}`(MVP, 仅 policy-match)
2. 前端在测试环境完成接入 + 状态机
3. 切生产
4. 资质 SOP 上线时,前端只加意图关键词,接口零改动

---

## 8. 兼容性说明

- 切到异步模式后,**老的同步 `POST /chat` 接口保留**(不立即废弃)— 普通对话(非 SOP)仍走 `/chat`,只有触发 SOP 的消息走新 `/sop/X/run` endpoint
- 后端 `detectSopFromInput()` 等价的服务端意图识别**也保留**(为了 `/chat` 用户直接发"匹配政策"也能识别) — 但建议前端**主动**用关键词预判走 `/sop/X/run`,理由:
  1. 走 `/sop/X/run` 才能享受缓存
  2. 走 `/chat` 触发的 SOP 还是同步等 9 分钟(因为 daemon 端逻辑没变)
  3. 前端关键词宽松匹配(宁可多识别),命中率比 LLM-based 服务端识别还高

---

## 相关文档

- [policy-match-frontend-integration.md](policy-match-frontend-integration.md) — 旧同步模式(SSE 接入代码可复用)
- [policy-match-api-contract.md](policy-match-api-contract.md) — 后端政策匹配接口契约(daemon 调的下游 API,不在本文档范围)
- 后端架构设计(待补): `async-sop-architecture.md`
