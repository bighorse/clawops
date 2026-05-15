# SOP 异步任务模式 — 小程序前端对接 v2 (2026-05-15)

> **v2 重写说明**: 采用"现有 `/chat` 入口 + 服务端识别 + 早返回 + SSE 推任务事件 + 独立任务列表 UI"流程。前端**不需要新 API 调用代码**,只需:
> - 监听新增的 SSE 事件 (`sop_task_created` / `sop_task_done` / `sop_task_failed`)
> - 加一个右上角任务列表组件 + 红点提醒
> - `GET /me/sop/tasks` 拉列表数据
>
> 替代 v1 的 `POST /sop/{sop_name}/run` 设计 (已废弃)。

> **适用范围**: 所有 SOP — `policy-match` (政策匹配)、即将上线的 `qualification-check` (资质评估)、未来扩展 SOP。新增 SOP 时前端**零改动**(后端配置即可)。

---

## 1. 整体流程

```
小程序                    clawops gateway                      daemon
  │  POST /chat            │                                    │
  │  body: {"message":...} │                                    │
  │ ────────────────────►  │ 1. keyword regex 识别 sop_name      │
  │                        │                                    │
  │                        ├─ 不命中 ─→ 转发 daemon /api/chat ──►│ 几秒返回普通对话
  │  ◄── 几秒返回 ────────│  ◄────── /api/chat response ──────  │
  │                        │                                    │
  │                        └─ 命中 sop_name(如 policy-match)    │
  │                            │                                │
  │                            ├─ 缓存命中(sop_name + enterprise_id, 7天内)
  │                            │   INSERT sop_tasks status=done  │
  │                            │   推 SSE sop_task_created + sop_task_done
  │                            │   chat 返回:                    │
  │                            │     "已为您找到上次政策匹配结果,│
  │                            │      可在右上角任务列表查看"     │
  │                            │                                │
  │                            └─ 缓存未命中                     │
  │                                INSERT sop_tasks status=running
  │                                推 SSE sop_task_created       │
  │                                spawn 后台 task ─────────────►│ daemon 跑 SOP 6 步 (~9 分钟)
  │                                chat 立即返回:                │
  │                                  "已开始政策匹配,预计 9 分钟,│
  │                                   可在右上角任务列表查看"     │
  │  ◄── 5-10 秒返回 ─────│                                    │
  │                        │                                    │
  │  ◄─── SSE sop_task_created ──── (前端右上角红点 +1)         │
  │                        │                              ◄─── /api/chat response (9 分钟后)
  │                        │ UPDATE sop_tasks status=done +     │
  │                        │ result_json + deeplink             │
  │  ◄─── SSE sop_task_done ─────── (前端右上角红点继续亮 + 列表 status=done)
  │                        │                                    │
  │  用户点右上角 → GET /me/sop/tasks                          │
  │                        │ SELECT * FROM sop_tasks            │
  │                        │ WHERE openid=? AND created_at > 30d ago
  │  ◄── 任务列表 ────────│                                    │
  │                        │                                    │
  │  用户点 done 任务 → wx.navigateTo(task.deeplink)
```

### 关键设计

- **前端 `POST /chat` 调用方式不变**,只是 chat 响应在 SOP 命中场景下变成提示文案(不再挂 9 分钟)
- **意图识别在 clawops 端用正则**(命中率 ~95%,漏识别时 chat 同步等 daemon 跑完——和当前生产一致,**不做兜底**)
- **任务列表是独立 UI 组件**(右上角入口),不污染聊天历史
- **聊天 history 只存提示文案 + 普通对话**,SOP 的实际结果在任务列表里看
- **缓存命中也走任务流程**(任务瞬时 done),UX 统一

---

## 2. 后端识别的关键词

clawops 用以下正则匹配 user message,命中对应 SOP:

| SOP | 正则(JS 等价) | 触发示例 |
|---|---|---|
| `policy-match` | `/政策\|匹配\|补贴\|申报\|适用/` | "帮我匹配下政策"、"看看有什么补贴"、"我能申报什么" |
| `qualification-check` | `/资质\|评估\|认证\|高新\|专精特新/`(待上线) | "帮我评估下资质"、"我能申高新吗" |

**漏识别风险**: 用户说话方式特殊(如"我们企业属于科技创新型,想找点扶持") 可能不命中 → chat 走同步,挂 9 分钟。这种情况罕见,**前端不需要兜底处理**(用户接受这种边缘 case)。

> 前端无需在客户端做关键词匹配 — 服务端识别就够,前端只看 SSE 事件响应。

---

## 3. SSE 事件 schema

接现有 `/events?token=...` 长连接,新增 3 个事件类型:

### 3.1 `sop_task_created`

任务创建时立即推送(缓存命中和未命中都推):

```json
{
  "type": "sop_task_created",
  "task_id": "tsk_abc123",
  "sop_name": "policy-match",
  "sop_name_cn": "政策匹配",
  "enterprise_name": "中拓产业云(北京)科技服务有限公司",
  "status": "running",                  // running / done(缓存命中时)
  "estimated_seconds": 540,             // 预计时长,running 时有效
  "created_at": "2026-05-15T08:00:00Z"
}
```

前端应:
- 右上角红点 +1
- 任务列表(如果当前展开)插入一条记录

### 3.2 `sop_task_done`

任务完成(daemon SOP 跑完 + 后端写完库):

```json
{
  "type": "sop_task_done",
  "task_id": "tsk_abc123",
  "sop_name": "policy-match",
  "sop_name_cn": "政策匹配",
  "status": "done",
  "deeplink": "/pages/recommendation/index?id=100",
  "completed_at": "2026-05-15T08:09:00Z"
}
```

前端应:
- 任务列表对应条目 status 改 `done`
- 红点保持(直到用户打开列表查看)

### 3.3 `sop_task_failed`

任务失败(daemon 报错/超时):

```json
{
  "type": "sop_task_failed",
  "task_id": "tsk_abc123",
  "sop_name": "policy-match",
  "sop_name_cn": "政策匹配",
  "status": "failed",
  "error": "服务繁忙,请稍后重试",      // 人类可读
  "completed_at": "2026-05-15T08:05:00Z"
}
```

前端应:
- 任务列表对应条目改 `failed` + 显示 error 文案
- 红点保持

### 3.4 现有事件(可保留监听,但本流程不依赖)

`/events` 仍推送 daemon 内部事件:`tool_call_start` / `tool_call` / `agent_start` / `agent_end` / `llm_request` 等。**前端可以忽略这些**,只看 sop_task_* 三种就够了。

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

### 6.1 SSE 监听代码示例

```javascript
const SOP_NAME_CN = {
  'policy-match': '政策匹配',
  'qualification-check': '资质评估',
  // 新 SOP 上线时加一行,但实际后端会推 sop_name_cn 字段,这里只作 fallback
};

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
        switch (ev.type) {
          case 'sop_task_created':
            taskStore.add({
              task_id: ev.task_id,
              sop_name: ev.sop_name,
              sop_name_cn: ev.sop_name_cn,
              enterprise_name: ev.enterprise_name,
              status: ev.status,
              estimated_seconds: ev.estimated_seconds,
              created_at: ev.created_at,
            });
            updateRedDot(); // 红点 +1
            break;

          case 'sop_task_done':
            taskStore.update(ev.task_id, {
              status: 'done',
              deeplink: ev.deeplink,
              completed_at: ev.completed_at,
            });
            // 红点保持亮,用户点开列表后才清除
            break;

          case 'sop_task_failed':
            taskStore.update(ev.task_id, {
              status: 'failed',
              error: ev.error,
              completed_at: ev.completed_at,
            });
            break;
        }
      } catch(e) {}
    }
  });
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
