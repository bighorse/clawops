# 熵玑参谋 · 前端对接文档（小程序 + Web）

**版本**：2026-08-06

本文档面向**小程序前端**、**Web 前端（Vue 2）**与**小程序后端**。接口三端共用，Web 端的环境差异集中在 §3.8。所有接口字段均经源码核对，字段名的大小写与下划线**必须逐字一致**，错一个字母登录链路就断。

---

## 一、对接总览

```
微信小程序
   │  ① wx.login() 拿 code
   ▼
小程序前端 ──② POST /auth/wx-login {app_id, code} ──► 熵玑参谋服务
                                                          │
                                     ③ 用 code 换 openid   │
                                                          ▼
                                                  你们的小程序后端
                                    POST /message/wechat/applets/{app_id}/open_id
                                                          │
                                     ④ 返回 {data:{open_id}}
                                                          ▼
                                              服务端建档 + 签发 token
                                                          │
   ◄────────── ⑤ {token, openid, is_new_user, expires_at} ┘
   │
   │  ⑥ 后续所有请求带 Authorization: Bearer <token>
   ▼
POST /chat ──► 熵玑参谋（企业快评 · 企业检索）
```

**分工**：

- **小程序后端**：只需实现 ③ 那个用 `code` 换 `openid` 的接口（详见 §4），此外无需开发。
- **前端（小程序 / Web 各一套）**：登录流程、对话页、**执行过程的实时滚动显示（§3.6，工作量最大的一块）**、简报渲染页（§3.7）、任务列表、错误与超时处理，以及**增长埋点与分享归因（§7）**，都要做。Web 端另见 §3.8（**跨域白名单需提前告知运维，否则一行都跑不通**）。
- **另需提供**：企业详情页的小程序路径模板（§6.1）——企业检索的结果要跳到你们的页面。

**关键概念**：**每个微信用户拥有一份完全独立的参谋实例**——独立的对话记忆、独立的简报产物，用户之间互不可见。首次登录时服务端要为该用户初始化专属实例，比较耗时（见 §3.3）。

---

## 二、接入地址

| 项 | 值 |
|---|---|
| 基地址 | `https://ai.infocts.cn/clawops` |
| 完整示例 | `https://ai.infocts.cn/clawops/chat` |
| 协议 | HTTPS |

下文所有接口路径均相对该基地址，例如 `/chat` 的完整地址是 `https://ai.infocts.cn/clawops/chat`。

**小程序后台需把 `https://ai.infocts.cn` 加入 request 合法域名。**

### ⚠️ 当前状态：接口尚未对公网开放

现在从公网访问会返回 **403**。这是有意为之：在你们的 code2session 接口（§4）就绪之前，服务端处于调试模式，此时任何人都能冒充任意用户，所以入口是关着的。

**开放的前提**：§4 的接口实现并联调通过。届时运维会同步放开访问，你们无需做额外配置。

联调期间如需从外网访问，请联系运维加白名单。

---

## 三、前端对接（小程序与 Web 共用）

### 3.1 登录：`POST /auth/wx-login`

**无需鉴权**（按来源 IP 限流，默认 10 次/分钟，超限返回 429）。

**请求体**（全部 snake_case）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `app_id` | string | **是** | 小程序 appid。生产环境为空会报错 |
| `code` | string | **是** | `wx.login()` 返回的 code。生产环境为空会报错 |
| `display_name` | string | 否 | 昵称，来自 `<input type="nickname">` |
| `avatar_url` | string | 否 | 头像 URL。**服务端原样存储，不做转存**，请自行换成永久链接 |
| `enterprise_profile` | object | 否 | 任意 JSON，会渲染进该用户的 USER.md |
| `mock_openid` | string | 否 | **仅调试用**。生产环境传了会直接返回 400 |

**响应体**（HTTP 200，四个字段恒存在）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `token` | string | **64 位小写十六进制**会话 token |
| `openid` | string | 微信 openid |
| `is_new_user` | bool | 本次是否新建了用户（触发开户） |
| `expires_at` | string | RFC3339 时间，如 `2026-09-04T08:15:30.123456Z` |

**Token 有效期 30 天，不滑动续期**，到期只能重新登录。

### 3.2 对话：`POST /chat`

**鉴权**：`Authorization: Bearer <token>`（注意 `Bearer` 后**一个空格**，大小写敏感）。
**限流**：按用户 30 次/分钟。

**请求体**：

| 字段 | 类型 | 必填 |
|---|---|---|
| `content` | string | **是** |
| `idempotency_key` | string | 否（透传给下游，服务端自身不去重） |

**响应体**：

| 字段 | 类型 | 说明 |
|---|---|---|
| `response` | string | 助手回复文本 |
| `model` | string \| null | 如 `deepseek-v4-pro`；命中内置路由时为 `clawops-router` |
| `openid` | string | 当前用户 openid |

**⚠️ 超时不匹配（必须处理）**：服务端对下游的超时是 **900 秒**，而 `wx.request` 默认只有 **60 秒**。企业快评这类任务动辄数分钟，**前端必须显式设置 `timeout: 120000` 以上**，并配合 §3.5 的任务列表做长任务感知。

### 3.3 首次登录耗时（体验关键）

`is_new_user = true` 时，服务端正在为这个用户初始化专属的参谋实例。**这个过程可能要 20 秒以上**。

前端处理：`is_new_user = true` 时展示更长的 loading 文案（如"正在为你准备专属参谋…"），不要用普通 loading 让用户以为卡死。

### 3.4 错误处理（易踩点）

**⚠️ 401 的响应体不是 JSON，是纯文本**：

| 状态码 | 响应体 | 含义 | 前端动作 |
|---|---|---|---|
| 401 | `missing bearer token` | 没带 token | 重新登录 |
| 401 | `invalid token` | token 不是本网关签发的 | 重新登录 |
| 401 | `session expired` | token 过期 | 重新登录 |
| 429 | `{"error":"rate_limited","retry_after_secs":N}` | 限流（带 `Retry-After` 头） | 按提示重试 |
| 400 | `{"error":"wechat_login_failed","errcode":N,"errmsg":"..."}` | 登录链路失败，详见 §4.4 | 提示用户重试 |
| 500 | `{"error":"..."}` | 服务端错误 | 提示稍后再试 |

统一错误处理**必须兼容 body 不是 JSON 的情况**，否则 401 时前端会解析崩溃。

另外：**接口本身从不返回 403**。若收到 403，说明请求被网络层的访问控制挡了（见 §2，当前尚未对公网开放）。

### 3.5 其它前端接口

| 接口 | 方法 | 鉴权 | 用途 |
|---|---|---|---|
| `/me/profile` | GET / PUT | Bearer | 读取/更新用户画像。PUT 的字段全部可选，不传=不修改，**无法置空** |
| `/me/chat-history` | GET | Bearer | 历史消息。参数 `before_id`、`limit`（默认 20，上限 100）。返回按 **id 倒序**（新的在前），带 `has_more` / `next_cursor` |
| `/me/sop/tasks` | GET | Bearer | 长任务列表。参数 `status`、`sop_name`、`limit`（默认 50，上限 200）。**只返回最近 30 天，无分页游标** |
| `/me/artifacts` | GET | Bearer | 简报列表（详见 §3.7） |
| `/me/artifacts/{path}` | GET | Bearer | 取简报 Markdown 正文（详见 §3.7） |
| `/events` | GET | Bearer 或 `?token=` | SSE 事件流，推送任务状态变化 |
| `/auth/logout` | POST | 手工读 Bearer | 注销当前 token，**永远返回 200** |
| `/auth/logout-all` | POST | Bearer | 注销该用户全部 token |
| `/health` | GET | 无 | 健康检查 |

**SOP 任务状态**：`pending` / `running` / `done` / `failed` 四值。任务对象字段：`task_id`、`sop_name`、`sop_name_cn`、`enterprise_name`、`status`、`deeplink`、`error`（**注意 JSON 字段名是 `error` 而非 `error_message`**）、`estimated_seconds`、`created_at`、`completed_at`。

**⚠️ `/events` 的鉴权失败返回 500 而不是 401**（已知问题），且**没有心跳帧**，需自行处理断线重连。

---

### 3.6 执行过程实时滚动显示（重点）

对话往往要跑几分钟，期间必须让用户看见"它在干什么"，否则体感就是卡死。做法是订阅 `GET /events` 事件流，把每一步工具调用滚动渲染出来。

#### 3.6.1 事件类型（实测抓包，非推测）

事件以标准 SSE 帧下发，每帧一行 `data: {json}`，帧间空行分隔：

```
data: {"type":"llm_request","provider":"custom:https://api.deepseek.com","model":"deepseek-v4-pro","timestamp":"..."}

data: {"type":"tool_call_start","tool":"shell","arguments":"{\"command\":\"date\"}","timestamp":"..."}

data: {"type":"tool_call","tool":"shell","duration_ms":2,"success":true,"timestamp":"..."}
```

完整事件类型：

| `type` | 关键字段 | 含义 | 建议展示 | 实测 |
|---|---|---|---|---|
| `llm_request` | `provider`、`model` | 开始请求模型 | "思考中…" | ✅ 22 次 |
| `tool_call_start` | `tool`、`arguments?` | **开始执行工具** | "▶ 正在执行 `<tool>`" + 参数摘要 | ✅ 28 次 |
| `tool_call` | `tool`、`duration_ms`、`success` | **工具执行完毕** | 把上面那条改成 "✓ / ✗ `<tool>` (Nms)" | ✅ 28 次 |
| `sop_task` | `task_id`、`event` | 长任务状态：`created` → `running` → `done` | 更新任务列表 / 进度条 | ✅ 3 次 |
| `agent_start` / `agent_end` | `provider`、`model`、`duration_ms` | 智能体起止 | — | 未出现 |
| `error` | `component?`、`message` | 出错 | 红字提示 | 未出现 |

以上"实测"列来自一次完整的企业快评（28 次工具调用、耗时约 5 分钟）的真实抓包。

**⚠️ 不要依赖 `sop_result`**：客户端类型定义里存在该事件，但**实测整轮长任务中一次都没有出现**。长任务的完成信号请以 **`sop_task` 的 `event: "done"`** 为准，收到后再调 `/me/sop/tasks` 取任务详情。

**`arguments` 是一个 JSON 字符串**（不是对象），需二次 `JSON.parse` 才能取到如 `{"command":"date"}`。展示时建议截断，不要整段糊到屏幕上。

**实测会出现的工具名**（用于做中文映射）：`web_search_tool`、`web_fetch`、`file_read`、`file_write`、`glob_search`、`shell`、`sop_execute`、`sop_advance`。

#### 3.6.2 小程序怎么连（与 Web 不同，关键差异）

Web 端用 `fetch` + `ReadableStream` 手动解析——因为 `EventSource` **加不了 `Authorization` 头**。

**小程序既没有 `EventSource` 也没有 `fetch` 流式**，只能用 `wx.request` 的分块模式：

```js
const task = wx.request({
  url: 'https://ai.infocts.cn/clawops/events',
  header: { Authorization: `Bearer ${token}` },
  enableChunked: true,          // 必须：开启分块接收
  timeout: 0,                   // 长连接不设超时
  success() {}, fail() {},
})

let buf = ''
const decoder = (ab) => {
  // 小程序无 TextDecoder，需自行把 ArrayBuffer 转 UTF-8 字符串
  return decodeUTF8(new Uint8Array(ab))
}

task.onChunkReceived((res) => {
  buf += decoder(res.data)
  let idx
  while ((idx = buf.indexOf('\n\n')) >= 0) {   // SSE 帧以空行分隔
    const frame = buf.slice(0, idx)
    buf = buf.slice(idx + 2)
    for (const line of frame.split('\n')) {
      if (!line.startsWith('data:')) continue
      const payload = line.slice(5).trim()
      if (!payload) continue
      try { handleEvent(JSON.parse(payload)) } catch (_) { /* 非 JSON 帧忽略 */ }
    }
  }
})
```

三个必须注意的点：
1. **`enableChunked: true` 不能少**，否则收不到流。
2. **小程序没有 `TextDecoder`**，`res.data` 是 `ArrayBuffer`，要自己实现 UTF-8 解码。中文被切在两个 chunk 之间时会乱码，解码器必须能处理**半个字符**（按字节缓冲，不足一个完整字符就留到下一块）。
3. **`/events` 若鉴权失败返回的是 500**，不是 401，重连逻辑不要只判 401。

#### 3.6.3 滚动渲染的正确姿势（避免刷屏）

维护一个 `liveSteps` 数组，**原地更新而不是不断追加新消息**：

```js
function handleEvent(e) {
  if (!this.data.busy) return          // 只在等待回复期间处理

  if (e.type === 'tool_call_start') {
    // 推入一条"执行中"
    liveSteps.push({ tool: e.tool, detail: e.arguments, running: true })

  } else if (e.type === 'tool_call') {
    // 从后往前找到同名且仍在 running 的那条，回填结果
    for (let i = liveSteps.length - 1; i >= 0; i--) {
      if (liveSteps[i].running && liveSteps[i].tool === e.tool) {
        liveSteps[i] = { ...liveSteps[i], running: false,
                         durationMs: e.duration_ms, success: e.success }
        break
      }
    }
  }
  this.setData({ liveSteps })
}
```

**为什么要"从后往前找同名 running 的那条"**：同一个工具可能被连续调用多次，先进先出会把结果配错行。

**长任务进度**：`sop_task` 状态变化时，**用同一条消息原地刷新**（记住它的消息 id 后 patch 内容），不要每次状态变化都追加一行，否则用户会看到满屏重复的进度条。

**收到 `response` 后清空 `liveSteps`**：`POST /chat` 返回最终回复时，把执行过程折叠或清掉，只留最终答案（可保留一个"查看执行过程"的折叠入口）。

#### 3.6.4 长任务完成后怎么拿结果

`sop_task` 的 `event: "done"` 到达后：

1. 调 `GET /me/artifacts` 拿简报列表（**已按修改时间倒序，第一条就是刚生成的**）
2. 用其中的 `path` 调 `GET /me/artifacts/{path}` 取 Markdown 正文
3. 在简报页渲染

`GET /me/sop/tasks` 只返回任务元数据（状态、企业名、耗时），**不含正文**——正文走 artifacts 接口。

---

### 3.7 简报接口（产物交付）

**产物只有一份：Markdown 简报。** 没有图片、没有下载链接、没有附件——小程序用专门的页面渲染这份 Markdown。

#### `GET /me/artifacts`

鉴权 Bearer。列出当前用户的全部简报，**按修改时间倒序**：

```json
{
  "artifacts": [
    {
      "path": "briefs/cambricon/brief_2026-08-05.md",
      "name": "brief_2026-08-05.md",
      "size": 7431,
      "modified_at": "2026-08-05T09:09:46.778208740Z"
    }
  ]
}
```

没有任何简报时返回 `{"artifacts": []}`（**不是错误**），前端可直接渲染空态。

`path` 字段**原样**回传给下面的接口即可。

#### `GET /me/artifacts/{path}`

鉴权 Bearer。返回单份简报的 Markdown 正文：

```json
{
  "path": "briefs/cambricon/brief_2026-08-05.md",
  "name": "brief_2026-08-05.md",
  "content": "# 中科寒武纪科技股份有限公司 — 企业快评\n\n**简报日期**：...",
  "size": 7431,
  "modified_at": "2026-08-05T09:09:46.778208740Z"
}
```

`content` 是**原始 Markdown**，网关不做任何转换。简报的固定结构是：一级标题（`<公司全称> — 企业快评`）、元信息行、一句话画像、五个板块（基本面 / 科创属性 / 融资历史 / 竞品对比 / 风险点）、快速总结、末尾免责声明。含 Markdown 表格，渲染组件需支持 GFM 表格。

**错误处理**：文件不存在、路径非法、越界访问、超出大小上限——**一律返回 404，且响应体完全相同**：

```json
{"error": "artifact not found"}
```

刻意不区分：错误文案里不回显你传的路径，否则可以拿它探测服务器上某个文件存不存在。所以前端**无法**从错误信息判断具体原因，统一按"这份简报取不到"处理即可。

**服务端硬约束**（前端需知晓，触发时同样是 404）：

| 约束 | 值 | 说明 |
|---|---|---|
| 单份大小上限 | **4 MB** | 简报约 7 KB，正常远不会触及。整份 Markdown 会进 JSON，故有此上限 |
| 目录递归深度 | 8 层 | 简报固定在 `briefs/<slug>/x.md`（2 层），仅列表接口受此限 |
| 列表条目上限 | 500 条 | 超出部分不返回，列表无分页游标 |
| 扩展名 | 仅 `.md` | 其它一律 404 |

**典型量级**：一份简报约 7 KB，可一次性读入内存渲染。

---

### 3.8 Web 端（Vue 2）对接差异

**接口完全相同**——地址、字段、鉴权、错误码都不用改。以下只列 Web 与小程序的环境差异。

#### 3.8.1 ⚠️ 先给运维你们的域名（否则一行都跑不通）

浏览器有同源策略，跨站调用前会先发一个 `OPTIONS` 预检。服务端**按白名单**放行，**不在白名单里的来源会被浏览器直接拦截**，请求根本到不了业务代码。

当前已放行：

| 来源 | 用途 |
|---|---|
| `https://ai.infocts.cn` | 与接口同域部署时 |
| `http://localhost:8080` | Vue CLI 默认 dev server |
| `http://localhost:5173` | Vite 默认 dev server |

**你们的生产域名和其它开发端口，需要提前告知运维加进白名单。** 端口不同也算不同来源（`localhost:8081` 与 `8080` 是两个来源）。

也可以在开发期用 Vue CLI 的 devServer 代理绕开跨域：

```js
// vue.config.js
module.exports = {
  devServer: {
    proxy: {
      '/clawops': {
        target: 'https://ai.infocts.cn',
        changeOrigin: true,
      },
    },
  },
}
```

代理只解决开发期。**生产环境若与接口不同域，仍然需要白名单。**

#### 3.8.2 SSE：不能用 EventSource

浏览器原生 `EventSource` **无法自定义请求头**，而我们的鉴权是 `Authorization: Bearer`，所以只能用 `fetch` + `ReadableStream` 手动解析。这一点和小程序不同（小程序是没有流式 fetch，只能用 `enableChunked`）。

```js
// services/events.js
const listeners = new Set()
let controller = null

export function onGatewayEvent(fn) {
  listeners.add(fn)
  ensureStream()
  return () => listeners.delete(fn)
}

async function ensureStream() {
  if (controller) return
  while (listeners.size > 0) {
    try {
      controller = new AbortController()
      const res = await fetch('https://ai.infocts.cn/clawops/events', {
        headers: { Authorization: `Bearer ${getToken()}` },
        signal: controller.signal,
      })
      if (!res.ok || !res.body) throw new Error(`SSE ${res.status}`)

      const reader = res.body.getReader()
      const decoder = new TextDecoder()   // 浏览器有，小程序没有
      let buf = ''
      for (;;) {
        const { done, value } = await reader.read()
        if (done) break
        buf += decoder.decode(value, { stream: true })
        let idx
        while ((idx = buf.indexOf('\n\n')) >= 0) {   // SSE 帧以空行分隔
          const frame = buf.slice(0, idx)
          buf = buf.slice(idx + 2)
          for (const line of frame.split('\n')) {
            if (!line.startsWith('data:')) continue
            const payload = line.slice(5).trim()
            if (!payload) continue
            try {
              const e = JSON.parse(payload)
              listeners.forEach((fn) => fn(e))
            } catch (_) { /* 非 JSON 帧忽略 */ }
          }
        }
      }
    } catch (_) {
      // 断开或失败，退避后重连
    }
    controller = null
    await new Promise((r) => setTimeout(r, 3000))
  }
}
```

`decoder.decode(value, { stream: true })` 的 `stream: true` **不能省**——它会把切在两个 chunk 之间的半个汉字留到下一块，否则中文会乱码。

#### 3.8.3 滚动显示：Vue 2 的响应式坑

事件处理逻辑与 §3.6.3 完全一致（`liveSteps` 数组、从后往前回填），但 Vue 2 有个必须注意的点：

```js
// ❌ Vue 2 检测不到按索引赋值
this.liveSteps[i] = { ...this.liveSteps[i], running: false }

// ✅ 用 $set 或整体替换
this.$set(this.liveSteps, i, { ...this.liveSteps[i], running: false })
// 或
this.liveSteps = this.liveSteps.map((s, idx) => (idx === i ? {...s, running: false} : s))
```

Vue 2 的响应式基于 `Object.defineProperty`，**数组按下标赋值不会触发视图更新**。这个坑在滚动显示里必踩——工具执行完了但界面还停在"执行中"。

#### 3.8.4 其它差异一览

| 项 | 小程序 | Web（Vue 2） |
|---|---|---|
| token 存储 | `wx.setStorageSync` | `localStorage`（注意 XSS 风险，别渲染未转义的用户内容） |
| 登录 | `wx.login()` 拿 code | **没有 code**，见下 |
| 请求超时 | `wx.request` 的 `timeout` | `fetch` **默认不超时**，需 `AbortController` + `setTimeout` |
| 长任务 | 同左 | 同左，`/chat` 建议至少给 300 秒 |
| Markdown 渲染 | 小程序 Markdown 组件 | `marked` + `DOMPurify`（**必须消毒**，简报含外部来源内容） |
| 分享 | `onShareAppMessage` | 复制链接 / 网页分享 API，埋点见 §7 |

**登录方式**：`wx.login()` 是小程序专有的。Web 端没有 code，需要另一条登录路径（微信网页授权、扫码登录，或你们已有的账号体系）。**这条需要和运维单独确认**——当前服务端只实现了小程序 code 换 openid 与企微两条路径。

#### 3.8.5 Web 端的埋点调整

§7 的埋点清单在 Web 端同样适用，但两处要改：

- `share_performed` — Web 没有 `onShareAppMessage`，改为在"复制链接"或"分享"按钮的点击回调里上报
- 归因参数 `from` — 改为放在 URL query（`?from=xxx`），首屏从 `location.search` 读出后存进 `localStorage`，其余逻辑与 §7.2 一致

---

## 四、小程序后端必须实现的接口

这是 **唯一** 需要中伯伦后端开发的接口。服务端**不持有小程序的 appid/secret**，也不直接调用微信的 `jscode2session`，而是把 code 转发给你们的后端去换。

### 4.1 接口契约

```
POST {backend_base_url}/message/wechat/applets/{app_id}/open_id
```

- 这个基地址由你们提供给运维配置，尾部斜杠会被自动去除
- `{app_id}` 直接拼进 URL 路径（**服务端不做 URL encode**）
- Content-Type: `application/json`
- **不带任何认证头**（无 Authorization、无 admin token）——请自行用网络层或 IP 白名单保护
- 超时：**30 秒**

### 4.2 请求体（服务端发给你们）

```json
{
  "code": "<wx.login 返回的 code>",
  "client": "clawops"
}
```

只有这两个字段。`client` 恒为字面量 `"clawops"`。**app_id 不在 body 里，只在 URL 路径上。**

### 4.3 响应体（你们返回给服务端）

```json
{
  "data": {
    "open_id": "oXXXXXXXXXXXXXXXXXXXX"
  }
}
```

**三个致命细节**：
1. 字段名是 **`open_id`（带下划线）**，不是 `openid`
2. 它嵌在 **`data` 对象里**，不是顶层
3. 必须返回**合法 JSON**，且 HTTP 状态为 2xx

失败时可返回非 2xx，并在顶层放 `message` 字段说明原因，服务端会把它透传给前端。

你们内部的实现就是拿 `code` + appid + secret 调微信官方 `jscode2session`，把拿到的 openid 按上面格式包一层返回。

### 4.4 错误码对照（便于联调）

服务端会把后端的失败包装成 HTTP 400 + 结构化错误：

| errcode | errmsg | 触发条件 |
|---|---|---|
| `-10001` | `empty code (call wx.login first)` | 前端没传 code |
| `-10002` | `backend returned empty open_id` | 你们返回的 `data.open_id` 为空或缺失 |
| `-10003` | `missing app_id` | 前端没传 app_id |
| `-10004` | `backend returned non-JSON body: ...` | 你们返回的不是合法 JSON |
| `403` / `500` 等 | 你们的 `message` 字段 | 你们返回了非 2xx，errcode 即你们的 HTTP 状态码 |

**注意**：后端返回的 403 会被降级成 HTTP **400**（errcode 里保留 403）。服务端对登录接口永远不返回 401/403。

---

## 五、企微接口（保留，待后续实现）

已实现并保留，当前不启用。**这是服务端到服务端接口，小程序不应直接调用。**

### `POST /auth/wecom-login`

- **鉴权：`X-Admin-Token`**（管理员令牌，由运维保管，切勿下发到客户端）
- 请求体：`uin`（**必填**）、`display_name`（可选）、`avatar_url`（可选）
- 响应体：与 `/auth/wx-login` 完全相同
- openid 会被合成为 **`uin:<uin>`** 形式
- **每次调用都签发新 token，旧 token 不失效**

**后续接企微时的正确姿势**：企微服务端为每个用户先调此接口拿 token，用该 token 作 Bearer 调 `/chat`，遇 401 就重新登录。**不要缓存来路不明的 token，也不要自己生成 token**——服务端只认自己签发的那一份。

---

## 六、能力说明：熵玑参谋能做什么

对外只有**两项能力**：

| 类型 | 名称 | 说明 |
|---|---|---|
| SOP | `enterprise-quick-review` | **企业快评**：公司名消歧 → 缓存检查 → 五板块多源检索 → Markdown 简报 + ≤800 字口头汇报 |
| Skill | `enterprise-search` | **企业检索**：按行业 / 地域 / 融资阶段等条件筛选企业 |

**触发方式**：用户自然语言即可。
- 企业快评："查一下 XX 公司""XX 这家公司怎么样""帮我看看 XX""评估 XX"
- 企业检索："搜索企业""帮我找""按行业筛选""有哪些…的公司"

企业快评是**长任务**（实测约 5 分钟、28 次工具调用），进度通过 §3.6 的事件流感知。

**产物**：一份 Markdown 简报，经 §3.7 的接口读取。对话里同时会有一段 ≤800 字的口头汇报（一句话画像 + 核心亮点 + 主要风险）。

---

### 6.1 ⚠️ 需要你们提供：企业详情页路径模板

**企业检索**会返回一批匹配的企业，每条都应该能点进你们的**企业详情页**。这个页面路径只有你们知道，所以需要提供一个模板给运维配置。

**格式**：一个小程序页面路径，用 `{name}` 占位企业全称。例如

```
/pages/enterprise/detail?name={name}
```

**两个必须知道的约束**：

1. **只能按企业名称跳转，没有企业 ID。** 企业库的表里没有主键，检索结果只有企业全称。所以占位符只有 `{name}`，你们的详情页需要支持**按名称查询**。
2. **企业名会做 URL 编码后填入。** 工商全称里常见中文、括号、`&`，不编码会截断链接。你们的页面接收后需要解码。例如"百维互联科技发展(北京)有限公司"会变成 `%E7%99%BE%E7%BB%B4...%E5%85%AC%E5%8F%B8`。

**配置前的表现**：模板未配置时，检索结果**不带链接**（只有企业名和推荐理由），不会输出坏链接或占位符。所以这一项**不阻塞联调**，可以后补。

把模板发给运维即可，无需你们做其它改动。

## 七、增长埋点（前端实现）

产品方案第五章的增长引擎，靠一个循环跑起来：**用户看到有价值的简报 → 转发给同行 → 同行打开并开通 → 又产出新简报**。这个循环转不转，由一个数衡量：

```
K 因子 = 分享率 × 打开率 × 激活率
       = 平均每个活跃用户带来多少新用户
```

K ≥ 1 就意味着流量自我延续、不再依赖投放。**要算出这个数，必须有埋点**——下面每个事件都对应 K 因子公式里的一环，缺一环就算不出来。

### 7.1 必埋事件

| 事件名 | 触发时机 | 必带参数 | 对应指标 |
|---|---|---|---|
| `report_generated` | 一份简报生成完毕（收到 `sop_task` 的 `done`） | `task_id` | 分享率的**分母** |
| `share_performed` | `onShareAppMessage` 被触发（用户确实转发了） | `task_id`、`share_id` | 分享率的**分子** |
| `launch_from_share` | 小程序启动且 query 里带 `from` 参数 | `from`、`scene` | 打开率的**分子** |
| `first_query_submitted` | 新用户提交第一条查询 | — | 激活漏斗 |
| `first_answer_received` | 新用户拿到第一条完整回复 | `duration_ms` | **首答完成率**（目标 ≥70%） |
| `session_start` | 每次进入小程序 | `openid`、`is_new_user` | **7 日留存**（目标：首月 ≥40%，长尾 ≥25%） |

`is_new_user` 直接取登录接口返回的同名字段（§3.1），不用自己判断。

### 7.2 归因链路：K 因子的命门

上面六个事件各自都好埋，但**只有把"谁带来谁"串起来，K 因子才算得出来**。做法是在分享路径里带上分享者标识：

**分享时**——在 `onShareAppMessage` 返回的 `path` 里附上分享者标识：

```js
onShareAppMessage() {
  return {
    title: `${companyName} 企业快评`,
    path: `/pages/report/index?task=${taskId}&from=${myShareCode}`,
  }
}
```

**被分享者打开时**——从启动参数里取出来，随后续埋点一起上报：

```js
onLaunch(options) {
  const from = options.query?.from
  if (from) {
    track('launch_from_share', { from, scene: options.scene })
    wx.setStorageSync('referrer', from)   // 留到注册完成时一起归因
  }
}
```

**两个务必注意的点**：

1. **别把 openid 直接放进分享链接。** openid 是用户身份标识，出现在可被任意转发的链接里等于公开。请自行生成一个不可逆的短码（如 openid 的哈希取前 8 位）作为 `myShareCode`，并在自己这边维护短码↔用户的映射。
2. **归因要延迟到注册完成时再记。** 被分享者点开后可能先逛一圈才登录，`from` 要先存进 storage，等 `is_new_user = true` 的登录成功时再落库，否则会把老用户也算成"带来的新用户"。

### 7.3 数据存哪

**这些埋点数据全部存在你们自己这边**（微信自定义分析，或你们自己的埋点系统），**熵玑参谋服务端不参与**——它只负责 AI 能力，不碰增长分析。K 因子在你们的数据后台算即可。

例外：如果后续要做"分享者奖励"（方案里提到的「你带来的同行已开通，已自动为你延长 X 天」），那需要服务端知道归因关系，届时再单独加接口。**当前阶段不需要。**

### 7.4 合规红线（方案第五章明确要求）

微信对「利诱分享」有硬性规定，以下三条请在实现时守住：

- **不设「分享才能解锁」的门槛**——任何功能都不能以分享为前置条件
- **分享者奖励只做事后弱提示**，不能在分享前承诺回报
- **被分享者的体验权益放在落地页**，作为落地页权益是合规的

### 7.5 建议的分享入口位置

- **简报页底部**：最自然的位置，用户读完有观点的内容时最想转发
- **对话里简报生成后**：附一个「转发给同行」的轻量入口
- 转发卡片的标题建议带公司名（如「寒武纪 企业快评」），比通用标题的打开率高

---

## 八、当前状态

| 项 | 状态 |
|---|---|
| 服务端 | ✅ 运行中 |
| 对话 `/chat` | ✅ 已验证 |
| 企业快评 · 企业检索 | ✅ 已上线 |
| 简报接口 `/me/artifacts` | ✅ 已上线 |
| 事件流 `/events` | ✅ 已验证 |
| **你们的 code2session 接口** | ⬜ **待开发**（§4），唯一的阻塞项 |
| 企业详情页路径模板 | ⬜ 待你们提供（§6.1），不阻塞联调 |
| 增长埋点与分享归因 | ⬜ 待前端实现（§7），上线前需就绪 |
| Web 端跨域白名单 | ✅ 已支持；生产域名待你们提供后加入（§3.8.1） |
| Web 端登录方式 | ⬜ 待定：服务端目前只有小程序 code 与企微两条路径（§3.8.4） |
| 公网开放 | ⬜ 待上一项就绪后由运维放开 |
| 企微接入 | ⬜ 接口已就绪，待后续实现 |

联调有问题随时找运维——尤其是遇到 403（说明还没给你们放开访问）。
