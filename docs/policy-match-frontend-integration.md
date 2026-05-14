# policy-match 小程序前端对接说明 (2026-05-14)

> **背景**: zeroclaw daemon 端 SOP 已实测跑通完整 6 步 (E2E v27, 后端写库 + 卡片推送都验证)。本文给小程序前端开发讲清楚怎么触发、怎么实时识别 SOP 启动、怎么处理 ~9 分钟的长响应。

---

## 触发方式

小程序在聊天页发送任意触发 policy-match 的消息, 如:

- "帮我匹配下政策"
- "我想看看适用我们公司的政策"
- "政策匹配"
- "看看有什么补贴可以申报"

daemon 端 IDENTITY 第零原则强制识别这类意图 → 调用 `sop_execute('policy-match')`。前端**事先无法用关键词预判**, 因为 IDENTITY 是 LLM 判断, 比正则更松。识别方式见下面 SSE 流。

## 调用 endpoint

小程序需要**两个连接同时存在**:

### 1. `POST /chat` — 发消息 + 等最终回复

```
POST https://<clawops-gateway>/chat
Headers:
  Authorization: Bearer <user-paired-token>
  Content-Type: application/json
Body:
  {
    "message": "帮我匹配下政策",
    "openid": "<wx-openid>"
  }
```

普通 chat 几秒返回, policy-match 触发时挂 6-10 分钟才返回。

### 2. `GET /events` — SSE 事件流, 实时知道 SOP 启动

```
GET https://<clawops-gateway>/events?token=<user-paired-token>
Accept: text/event-stream
```

clawops gateway 按 token resolve 到对应用户的 daemon, 把 zeroclaw `/api/events` byte-stream 透传过来。事件按 SSE 格式 `data: {...}\n\n`。

**关键事件**:

| event.type | event.tool | 含义 |
|-----------|-----------|------|
| `tool_call_start` | `sop_execute` | **SOP 启动** — 前端立刻切 long-loading UI |
| `tool_call_start` | 其他 | 普通工具调用 (memory_recall 等), 忽略 |
| `agent_end` | — | 这一轮 chat 跑完 (最终回复随 /chat HTTP 返回, 不在 SSE 里) |
| `error` | — | daemon 报错 |

`tool_call_start.arguments` 字段是 LLM 调用 tool 时传的参数 JSON 串 (前 300 字符), 含 `name="policy-match"`。多 SOP 时可用 arguments 字段区分 SOP 类型。

## 响应特性 — 重点!

policy-match SOP 完整跑完需要 **6-10 分钟** (qwen3.6-plus + 大 context), 原因:

| 阶段 | 耗时 | 说明 |
|------|------|------|
| step 1 取企业画像 | 5-15s | 后端 enterprise_profile_sync 同步调用 |
| step 1 LLM 写 profile.json | 2-3 分钟 | qwen 写 13KB JSON |
| step 2 取政策清单 | <100ms | 后端 policy_summary 28KB |
| step 3 LLM 匹配 (100 条 → Top 10) | 2-3 分钟 | 大 context 推理 |
| step 4 LLM 条件分析 | 2-3 分钟 | 47 个 condition 评估 |
| step 5 写库 | 1s | POST save_match_result |
| step 6 对话回复 | 5-10s | 总结 Top 3 + URL 植入 |

**HTTP 连接保持 ~9 分钟**是预期行为 (等所有 step 跑完才返回最终回复)。

## 前端实现

### 1. 聊天页 onLoad 时建 SSE 长连接

微信小程序原生没有 `EventSource`, 用 `wx.request` + `enableChunked` (微信基础库 ≥ 2.20.1):

```javascript
let sopActive = false;

const eventsTask = wx.request({
  url: `https://<clawops-gateway>/events?token=${pairedToken}`,
  method: 'GET',
  enableChunked: true,
});

let buffer = '';
eventsTask.onChunkReceived(res => {
  buffer += new TextDecoder().decode(new Uint8Array(res.data));
  // SSE 帧分隔: \n\n
  const frames = buffer.split('\n\n');
  buffer = frames.pop(); // 不完整的尾帧留着等下一片
  for (const frame of frames) {
    const m = frame.match(/^data:\s*(.+)$/m);
    if (!m) continue;
    try {
      const ev = JSON.parse(m[1]);
      if (ev.type === 'tool_call_start' && ev.tool === 'sop_execute') {
        sopActive = true;
        switchToLongLoadingUI();   // 切"政策顾问正在为您匹配...预计 5-10 分钟"
      } else if (ev.type === 'agent_end') {
        sopActive = false;
      } else if (ev.type === 'error') {
        showError(ev.message);
      }
    } catch(e) {}
  }
});

// 页面 onHide 时不必关 (微信会自动 freeze); onShow 时检测断了重连
```

### 2. 发消息 — POST /chat 设大 timeout

```javascript
wx.request({
  url: 'https://<clawops-gateway>/chat',
  method: 'POST',
  timeout: 900000,   // 15 min, 留余量
  data: { message: userInput, openid: wxOpenid },
  success(res) {
    dismissLoading();
    displayMessage(res.data.response);
    // 最终回复里含小程序深链 [小程序政策匹配页](/pages/recommendation/index?id=<id>)
    // 前端拦截 markdown 链接 click → wx.navigateTo
  },
  fail(err) {
    if (err.errMsg.includes('timeout')) {
      // 超过 15 min — 极少见, 展示 fallback
    }
  }
});
```

### 3. URL 植入与跳转

最终 LLM 回复格式示例:

```
已为 **<公司名>** 匹配 10 条适用政策...
TOP 3 优先级:
(1) ...
(2) ...
(3) ...
→ [小程序政策匹配页](/pages/recommendation/index?id=16)
```

前端在 markdown 渲染时拦截 `/pages/recommendation/index?id=<id>` 形式的链接 → `wx.navigateTo({ url: '/pages/recommendation/index?id=16' })`。

### 4. Long-loading UI 建议 (避免用户以为卡死)

收到 `tool_call_start{tool:"sop_execute"}` 后:

- 立即展示"政策顾问正在为您匹配...(预计 5-10 分钟)" 占位 bubble
- bubble 内带跳动动画 + 进度提示("正在分析企业画像 → 检索政策库 → 智能匹配 → 条件评估")
- 每 30-60s 切换进度文案 (模拟, daemon 不发细粒度进度事件)
- POST /chat success 回调到达 → bubble 替换为真实 LLM 回复

## 测试用例

让产品/测试同学按以下流程跑通端到端:

1. 在小程序新建一个测试账号 (没用过 policy-match 的)
2. 进聊天页 → 前端自动建 SSE `/events` 连接 (打开 devtools 看 chunked response)
3. 发"帮我匹配下政策"
4. **预期** SSE 在 10-30 秒内推 `tool_call_start{tool:"sop_execute"}` → 前端切 long-loading UI
5. **预期** loading 5-10 分钟 → POST `/chat` success → 收到文本回复
6. 文本回复格式应像 `已为 **<公司名>** 匹配 10 条适用政策...TOP 3 优先级: (1)... (2)... (3)... → [小程序政策匹配页](/pages/recommendation/index?id=<id>)`
7. 点击链接 → 跳转到 `/pages/recommendation/index?id=<id>` 看完整 10 条 + 条件分析

## 已知约束 (不需要前端处理)

- daemon 每次 chat 起一个 fresh isolated tokio runtime (~10-30ms 开销), 从前端看不见
- 大 context 的 chat() 内部用 reqwest + http1_only + pool_max_idle=0, 稳定走 dashscope qwen3.6-plus
- 后端 fields 过滤已上线 (profile 73→13KB, policy 67→28KB), 否则 SOP 会 hang
- SSE 是 per-daemon 广播, daemon 是 per-user 进程, 所以不会跨用户串流

## SSE 边界场景

- **小程序进后台**: 微信 freeze 小程序进程, SSE 连接断。回前台 onShow 时前端需要检测并重连 `/events`。如果切后台期间 SOP 启动事件已经过去, 回前台时拿不到 (但 POST /chat 的最终回复仍然能拿到, 前提是 wx.request 没被系统挂起)。
- **网络抖动**: SSE 用 byte pass-through, 上游 zeroclaw `/api/events` 的 keepalive 帧 (axum 默认每 15s 一次) 帮助探活。

## 故障排查

| 现象 | 排查方向 |
|------|---------|
| 小程序长 loading 9 分钟后超时, 文本未到 | 看 daemon `/tmp/claw-<uid>.log`, SOP 是否 sop_advance 完整 6 次 |
| SSE 收不到 `sop_execute` 事件 | curl `https://<gateway>/events?token=...` 看 SSE 流是否通; 看 daemon log 是否记录了 ToolCallStart |
| 文本回复到了但 enterprise_id 错 | 看 step 1 返回的 enterprise_id 是否非 0 (企业未注册场景) |
| 用户 openid 没绑企业 | 后端 enterprise_profile_sync 会 Failed, daemon SOP `sop_advance status=Failed` 终止 |

## 相关代码 (clawops 仓库)

- SOP 定义: [templates/workspace/sops/policy-match/SOP.md.hbs](../templates/workspace/sops/policy-match/SOP.md.hbs)
- IDENTITY 触发规则: [templates/workspace/IDENTITY.md.hbs](../templates/workspace/IDENTITY.md.hbs)
- clawops SSE 代理: [src/http.rs:144](../src/http.rs#L144)  (GET /events → zeroclaw /api/events)
- 后端契约: [policy-match-api-contract.md](policy-match-api-contract.md) + addendum-fields.md + addendum-profile-fields.md
- daemon 死锁修复历史: [policy-match-zeroclaw-deadlock-investigation.md](policy-match-zeroclaw-deadlock-investigation.md)
- zeroclaw SSE 实现 (含 ToolCallStart arguments 透传, v28): [zeroclaw/src/gateway/sse.rs](../../zeroclaw/src/gateway/sse.rs)
