# SOP 异步任务模式 — 小程序前端对接 v3 (2026-05-15)

> **v3 调整说明** (在 v2 基础上):
> - 意图识别从"clawops 正则"升级为 **LLM 分类**(deepseek-v4-flash 1 秒分类,准确率 ~99%)
> - SSE 事件类型从 3 个(`sop_task_created`/`done`/`failed`) 合并为 **1 个 `sop_task`**,前端收到后**重新拉 `/me/sop/tasks` 全量列表**,不维护增量状态
>
> 沿用 v2 的核心流程:"现有 `/chat` 入口 + 服务端识别 + 早返回 + SSE 推任务事件 + 独立任务列表 UI"。前端**不需要新 API 调用代码**,只需:
> - 监听 SSE `sop_task` 事件 → 调用 `GET /me/sop/tasks` 刷新列表
> - 加右上角任务列表组件 + 红点提醒

> **适用范围**: 所有 SOP — `policy-match` (政策匹配)、即将上线的 `qualification-check` (资质评估)、未来扩展 SOP。新增 SOP 时前端**零改动**(后端配置即可)。

---

## 1. 整体流程

```
小程序                    clawops gateway                      daemon
  │  POST /chat            │                                    │
  │  body: {"message":...} │                                    │
  │ ────────────────────►  │ 1. 调 LLM 做意图分类 (~1-2s)        │
  │                        │    deepseek-v4-flash 1 次 call      │
  │                        │    返回 {intent, confidence}        │
  │                        │                                    │
  │                        ├─ intent=normal_chat ─→ 转发 daemon ─►│ 几秒返回普通对话
  │  ◄── 几秒返回 ────────│  ◄────── /api/chat response ──────  │
  │                        │                                    │
  │                        └─ intent=<sop_name>                  │
  │                            │                                │
  │                            ├─ 缓存命中(sop_name + enterprise_id, 7天内)
  │                            │   INSERT sop_tasks status=done  │
  │                            │   推 SSE sop_task event         │
  │                            │   chat 返回:                    │
  │                            │     "已为您找到上次政策匹配结果,│
  │                            │      可在右上角任务列表查看"     │
  │                            │                                │
  │                            └─ 缓存未命中                     │
  │                                INSERT sop_tasks status=running
  │                                推 SSE sop_task event         │
  │                                spawn 后台 task ─────────────►│ daemon 跑 SOP 6 步 (~9 分钟)
  │                                chat 立即返回:                │
  │                                  "已开始政策匹配,预计 9 分钟,│
  │                                   可在右上角任务列表查看"     │
  │  ◄── 几秒返回 ────────│                                    │
  │                        │                                    │
  │  ◄─── SSE sop_task ─── │                              ◄─── /api/chat response (9 分钟后)
  │  前端 → GET /me/sop/tasks                                   │
  │                        │ UPDATE sop_tasks status=done       │
  │                        │ 推 SSE sop_task event              │
  │  ◄─── SSE sop_task ─── │ (前端再次 GET /me/sop/tasks)        │
  │                        │                                    │
  │  用户点右上角 → 已有最新任务列表数据                          │
  │  用户点 done 任务 → wx.navigateTo(task.deeplink)
```

### 关键设计

- **前端 `POST /chat` 调用方式不变**,只是 chat 响应在 SOP 命中场景下变成提示文案(不再挂 9 分钟)
- **意图识别在 clawops 端用 LLM 分类**(deepseek-v4-flash 1-2 秒,准确率 ~99%,边缘 case 走 normal_chat 同步,**不做兜底**)
- **SSE 事件只有 1 种类型 `sop_task`** — 前端不维护增量,收到事件后**直接重新拉 `/me/sop/tasks` 全量**,服务端是 single source of truth
- **任务列表是独立 UI 组件**(右上角入口),不污染聊天历史
- **聊天 history 只存提示文案 + 普通对话**,SOP 的实际结果在任务列表里看
- **缓存命中也走任务流程**(任务瞬时 done,推一次 sop_task event),UX 统一

---

## 2. 后端意图识别 (LLM 分类)

clawops 在 `POST /chat` 入口先调一次 LLM 做意图分类(deepseek-v4-flash 或同档轻量模型),~1-2 秒返回:

```
LLM prompt:
  你是政策小助手前置分类器。判断用户意图,从以下分类选一个:
  - policy-match: 用户希望系统帮他匹配政策(政策匹配/补贴申报/扶持优惠 等意图)
  - qualification-check: 用户希望系统评估企业资质(资质评估/高新认定 等意图)
  - normal_chat: 其他对话/咨询/闲聊/打招呼

  仅输出 JSON: {"intent": "<分类名>", "confidence": 0.0-1.0}

用户消息: <message>
```

服务端处理逻辑:
- `intent != "normal_chat"` 且 `confidence ≥ 0.7` → 触发 SOP 异步任务(命中 SOP)
- 否则 → 转发到 daemon `/api/chat` 同步处理(普通对话)

**SOP 支持清单**:

| SOP | 中文名 | 预估耗时 | 触发示例 |
|---|---|---|---|
| `policy-match` | 政策匹配 | 9 分钟 | "帮我匹配下政策"、"看看有什么补贴可以申报" |
| `qualification-check`(待上线) | 资质评估 | 3 分钟 | "帮我评估下资质"、"我能申高新吗" |

**为什么不用正则**:
- LLM 能理解"我们想找些扶持"这种隐含表达,正则会漏
- 准确率 ~99% vs 正则 ~95%
- 新增 SOP 时只动 prompt 不动正则代码,扩展性更好
- 多花 1-2 秒和 ~$0.001 / chat 成本可接受

**漏识别 (~1%)**: LLM 也可能误判 normal_chat → chat 走同步,挂 9 分钟。这种情况罕见,**前端不需要兜底处理**。

> 前端无需在客户端做意图识别 — 服务端识别就够,前端只看 SSE 事件响应。

---

## 3. SSE 事件 schema (统一 `sop_task` 单事件)

接现有 `/events?token=...` 长连接,新增 **1 种事件类型**:

```json
{
  "type": "sop_task",
  "task_id": "tsk_abc123",
  "event": "created"
}
```

**字段说明**:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `type` | string | ✓ | 固定 `"sop_task"` |
| `task_id` | string | ✓ | 触发本次事件的 task ID |
| `event` | string | ✓ | 子事件类型:`"created"` / `"done"` / `"failed"` |

**触发时机**:

| event | 时机 |
|---|---|
| `created` | clawops 识别 SOP 意图 + INSERT 任务后(缓存未命中场景);**缓存命中场景不推 `created`**,直接推 `done`(任务瞬时完成) |
| `done` | daemon SOP 跑完 + clawops 写库后;或缓存命中场景立即推 |
| `failed` | daemon 报错/超时后 |

**前端处理(简化)**:
- 收到 `sop_task` 事件,**不管 event 子类型,统一调用 `GET /me/sop/tasks`** 拉最新列表
- 拉到列表后,**对比本地缓存**找到 `task_id` 对应的任务,根据 status (`running` / `done` / `failed`) 决定 UI 行为:
  - 新任务(本地没有的 task_id) → 红点 +1,显示新任务进入列表
  - 任务 done → 红点保持,UI 显示"已完成"
  - 任务 failed → 红点保持,UI 显示错误文案

**为什么用统一事件**:
- 前端不维护增量状态,服务端列表是 single source of truth
- 不怕事件丢失/乱序
- SSE payload 极简,后端少传字段
- 多 task 并发场景天然支持(收到任意事件就刷新整个列表)

**`event` 字段的可选用途**:
- 直接根据 `event=created` 显示 toast "已开始政策匹配"
- `event=done` 显示 toast "政策匹配已完成,点击查看"
- `event=failed` 显示 toast "政策匹配失败"
- (如果前端不想做这种细分提示,可以完全忽略 `event` 字段,只看 task_id 触发刷新)

### 3.1 现有事件(可保留监听,但本流程不依赖)

`/events` 仍推送 daemon 内部事件:`tool_call_start` / `tool_call` / `agent_start` / `agent_end` / `llm_request` 等。**前端可以忽略这些**,只看 `sop_task` 事件就够了。

---

## 4. `GET /me/sop/tasks`

用户点右上角任务列表时调,拉 30 天内任务。

```
GET https://<clawops-gateway>/me/sop/tasks?limit=50
Headers: Authorization: Bearer <user-paired-token>
```

可选 query:
- `status=running|done|failed` — 按状态过滤
- `sop_name=policy-match` — 按 SOP 类型过滤
- `limit=50` — 默认 50,最大 200

响应:

```json
{
  "tasks": [
    {
      "task_id": "tsk_abc123",
      "sop_name": "policy-match",
      "sop_name_cn": "政策匹配",
      "enterprise_name": "中拓产业云(北京)科技服务有限公司",
      "status": "done",
      "deeplink": "/pages/recommendation/index?id=100",    // status=done 时存在
      "error": null,                                        // status=failed 时存在
      "estimated_seconds": 540,                            // status=running 时展示用
      "created_at": "2026-05-15T08:00:00Z",
      "completed_at": "2026-05-15T08:09:00Z"               // done/failed 时存在,running 时 null
    }
  ],
  "total": 23,    // 30 天内任务总数
  "has_more": false
}
```

**保留期限**: **30 天**,超过自动从 `/me/sop/tasks` 过滤掉(后端 reaper 不立即删除数据,只是不返回)。

---

## 5. `POST /chat` 行为说明

前端调用方式**不变**:

```
POST https://<clawops-gateway>/chat
Headers:
  Authorization: Bearer <user-paired-token>
  Content-Type: application/json
Body:
  { "message": "帮我匹配下政策", "openid": "..." }
```

响应内容根据场景变化(`response` 字段):

| 场景 | 响应文案 | HTTP 耗时 |
|---|---|---|
| 普通对话(未命中关键词) | LLM 自然回复 | 几秒(daemon 同步) |
| SOP 缓存命中 | "已为您找到上次**政策匹配**结果,可在右上角任务列表查看" | < 200ms |
| SOP 新任务入队 | "已开始**政策匹配**,预计 9 分钟,可在右上角任务列表查看" | 1-2s(写库 + spawn) |
| 漏识别 SOP(罕见) | LLM 自然回复或同步等 9 分钟 | 几秒到 9 分钟 |

前端**不需要解析 response 内容**——SSE 的 `sop_task_*` 事件才是 single source of truth。`response` 文案只是用户在聊天里能直接看到的提示。

---

## 6. 前端集成

### 6.1 SSE 监听代码示例(简化版)

```javascript
let sseTask = null;

function connectSSE(pairedToken) {
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
        if (ev.type === 'sop_task') {
          // 统一处理:任何 sop_task 事件都触发刷新列表
          refreshTasks(ev.task_id, ev.event);
        }
      } catch(e) {}
    }
  });
}

async function refreshTasks(triggeredTaskId, eventSubtype) {
  const res = await wxRequest({
    url: `${API}/me/sop/tasks?limit=50`,
    header: { 'Authorization': `Bearer ${pairedToken}` },
  });

  // 找到本次事件触发的 task,根据 status 决定 UI 行为
  const triggered = res.tasks.find(t => t.task_id === triggeredTaskId);
  const wasInLocal = taskStore.has(triggeredTaskId);

  // 用服务端最新数据覆盖整个本地列表 (single source of truth)
  taskStore.replaceAll(res.tasks);

  // UI 反馈(可选,根据 event 子类型给不同 toast)
  if (triggered) {
    if (eventSubtype === 'created' && !wasInLocal) {
      // 新任务进入
      updateRedDot();
      wx.showToast({ title: `已开始${triggered.sop_name_cn}`, icon: 'none' });
    } else if (eventSubtype === 'done') {
      updateRedDot();
      wx.showToast({ title: `${triggered.sop_name_cn}已完成,点击右上角查看`, icon: 'success' });
    } else if (eventSubtype === 'failed') {
      wx.showToast({ title: `${triggered.sop_name_cn}失败: ${triggered.error}`, icon: 'error' });
    }
  }
}
```

### 6.2 进入页面时拉历史任务

```javascript
async function loadTasks() {
  const res = await wxRequest({
    url: `${API}/me/sop/tasks?limit=50`,
    header: { 'Authorization': `Bearer ${pairedToken}` },
  });
  taskStore.replaceAll(res.tasks);
  updateRedDot();
}
```

### 6.3 右上角任务列表 UI 建议

#### 红点提醒

- 有 `status=running` 的任务 → 红点常亮(进行中提示)
- 有 `status=done` 但用户未查看的任务 → 红点常亮(新结果提示)
- 用户打开任务列表 → 标记所有 done 任务为"已查看"(本地状态,可用 wx.setStorageSync 存 viewed_task_ids 数组)→ 红点消失(如果没 running)

#### 任务列表展示(抽屉/弹窗)

```
┌───────────────────────────────┐
│  任务列表                ✕    │
├───────────────────────────────┤
│ 🟢 政策匹配 — 已完成          │
│    中拓产业云(北京)...        │
│    2026-05-15 16:09           │
│    [查看结果]  ← 点击跳 deeplink
├───────────────────────────────┤
│ ⏳ 政策匹配 — 进行中(剩 4 分钟) │
│    华为技术                    │
│    2026-05-15 17:00           │
│    (点击 → toast"还在跑")     │
├───────────────────────────────┤
│ ❌ 资质评估 — 失败             │
│    某公司                      │
│    服务繁忙,请稍后重试         │
│    2026-05-14 10:00           │
└───────────────────────────────┘
```

字段:
- 状态 icon + sop_name_cn + 状态文字
- 企业名(辅助识别)
- 时间戳(created_at,done 时显示 completed_at)
- done 时:"查看结果"按钮(`wx.navigateTo({ url: task.deeplink })`)
- running 时:点击显示"还在跑"toast(可选展示剩余预估时间)
- failed 时:展示 error 文案,点击 toast"请重试"

#### 进度条(可选优化)

running 状态可以根据 `created_at + estimated_seconds` 算预计完成时间 → 显示倒计时(纯前端计算,不依赖后端推进度)。daemon 实际可能比 eta 快或慢,但用户感知更好。

---

## 7. 扩展到新 SOP

后端添加新 SOP(如 `qualification-check`)只需:

1. 在 `templates/workspace/sops/qualification-check/SOP.md.hbs` 创建 SOP 模板
2. clawops.toml 加 SOP 元数据:
   ```toml
   [sop_metadata."qualification-check"]
   display_name_cn = "资质评估"
   estimated_seconds = 180
   keyword_regex = "资质|评估|认证|高新|专精特新"
   ```
3. clawops 启动时 load metadata,识别用户消息 + 推 SSE 时填 `sop_name_cn`

**前端零改动** — SSE 推过来的 `sop_name_cn` 字段直接展示,`deeplink` 字段直接 `wx.navigateTo`。
新增的 sop_name 在 `taskStore` 自动适配。

---

## 8. 与现有 `/chat` 的兼容性

- 当前生产仍跑同步 9 分钟模式(`/chat` 阻塞等 SOP 跑完)
- 异步模式上线后,**前端按本文档接入**(主要工作:加 SSE 处理 + 任务列表 UI),老的 `/chat` 同步等待逻辑可以删除
- 普通对话(非 SOP)仍走 `/chat`,行为不变(几秒返回)
- 历史对话(已完成的 SOP)不在新的 `sop_tasks` 表里,但**没必要回填** — 用户重发"匹配政策"触发新任务即可(7 天缓存生效,实际不会重跑)

---

## 9. 错误处理

| 现象 | 原因 | 前端处理 |
|---|---|---|
| 任务一直 `running` 超过 15 分钟 | daemon hang | 不主动处理,等服务端 reaper 标记 failed → 推 sop_task_failed |
| 漏识别 SOP `POST /chat` 同步挂 9 分钟 | clawops keyword 没匹配 | 同当前生产,接受;用户体验降级 |
| SSE 断连 | 网络/微信冻结 | onShow 重连 + 调 `GET /me/sop/tasks` catch up |
| `GET /me/sop/tasks` 5xx | clawops 异常 | 重试 3 次,失败展示"任务列表加载失败" |

---

## 10. 上线时间表

| 阶段 | 工作 | 工期 |
|---|---|---|
| 1. 后端实现 | clawops 加 sop_tasks 表 + `/chat` 识别分流 + 后台 spawn + `/me/sop/tasks` endpoint + SSE 事件 generator | 1.5 人天 |
| 2. 前端实现 | SSE 监听 + 任务列表组件 + 红点 + 跳转 | 1-2 人天 |
| 3. 联调 | 端到端测试 | 0.5 天 |
| 4. 灰度 | 切 1 个测试号,观察 1 天 | 1 天 |
| 5. 全量 | 11 个生产号切换 | 半天 |

**总计 ~5 个工作日**。

---

## 相关文档

- [policy-match-frontend-integration.md](policy-match-frontend-integration.md) — 旧同步模式(SSE 接入代码可复用)
- [policy-match-api-contract.md](policy-match-api-contract.md) — 后端政策匹配下游 API 契约
- 后端实现 plan(待补): `async-sop-backend-implementation.md`
