# ClawOps 小程序对接接口文档

> 状态:Phase 3 已上线 HTTPS,阶段 PoC 接近可联调。剩余[已知限制](#已知限制与上线前必修)。
> **接入地址**:`https://clawops.2048office.com`(通配证书 `*.2048office.com`,TLS 1.3 + HTTP/2)。
> 小程序后台必须把 `https://clawops.2048office.com` 加入"开发设置 → 服务器域名 → request 合法域名"。

## 总览

```
微信小程序 ──HTTPS──▶ ClawOps Gateway ──HTTP(127.0.0.1)──▶ 用户专属 zeroclaw daemon
```

每个登录的用户对应**专属** zeroclaw 进程 + 专属工作区。小程序只跟 ClawOps 交互,**不直接接触** zeroclaw。

## 鉴权模型

1. 小程序 `wx.login()` 拿到 `code`(以及可选 `wx.getPhoneNumber()` 拿到加密手机号)。
2. POST `/auth/wx-login`,ClawOps 调微信 `code2session` 拿到 `openid`。
3. 首次登录的用户由 ClawOps **自动创建** zeroclaw 实例(数秒延迟)。
4. ClawOps 返回 `token`(opaque bearer,30 天有效),**所有后续请求**带:
   ```
   Authorization: Bearer <token>
   ```
5. token 过期 / 失效 → 401,小程序需重新走 `/auth/wx-login`。

> ⚠️ token 是 ClawOps 自己签发的 session token,**不是**微信 session_key。不要传给微信任何接口。

## 接口清单

| 方法 | 路径                | 鉴权              | 说明 |
|------|---------------------|-------------------|------|
| POST | `/auth/wx-login`    | ❌                | 登录 / 首次自动开通(限流 10/min/IP) |
| POST | `/auth/logout`      | ✅ Bearer         | 撤销当前 token(幂等) |
| POST | `/auth/logout-all`  | ✅ Bearer         | 撤销该 openid 全部 token(设备丢失场景) |
| GET  | `/me/profile`       | ✅ Bearer         | 取当前用户的 display_name / phone / 企业画像 |
| PUT  | `/me/profile`       | ✅ Bearer         | 部分更新(只传要改的字段);USER.md 立即重渲染,下条 chat 生效 |
| POST | `/chat`             | ✅ Bearer         | 发送消息,等待整段回复(限流 30/min/用户) |
| GET  | `/events`           | ✅ Bearer 或 `?token=` | 实时事件流(进度条用) |
| GET  | `/health`           | ❌                | 健康检查 |

**429 限流响应**:`{"error":"rate_limited","retry_after_secs":N}` + 标准 `Retry-After: N` 头。客户端建议指数退避。

> 不要在小程序里调用 `/admin/*` 系列 —— 那是运维接口,小程序无权访问。

---

### POST /auth/wx-login

登录或首次自动开通。

**Request**

```json
{
  "code": "081Kq2Fa1xxxx",          // wx.login() 拿到的 code
  "phone": "+8613800138000",        // 可选,getPhoneNumber 解出的手机号
  "display_name": "王小明",          // 可选,用户昵称(用于 USER.md)
  "enterprise_profile": {            // 可选,企业画像(用于定制 USER.md)
    "company_name": "示例科技",
    "industry": "软件开发",
    "stage": "天使轮",
    "team_size": "5-10人",
    "pain_points": ["获客成本高"],
    "goals": ["高新技术企业认定"]
  }
}
```

> `enterprise_profile` 强烈建议小程序在用户首次完善资料时收集并提交。这决定 ClawOps 给该用户渲染的 `USER.md` 内容,直接影响 LLM 回复质量(参考下方"画像生效示例")。

> ⚠️ **生产服务器 `clawops.2048office.com` 已切真微信**(2026-04-28 起)。`mock_openid` 字段会被 **400 DevFieldInProd** 拒绝。前端开发请用微信开发者工具的 `wx.login()` 拿真 code 调试 —— 这跟生产链路完全一致。
>
> 如果未来上线 staging 环境(`wx.appid` 配空的实例),才能用 `mock_openid` 调试。当前没有。

**Response 200**

```json
{
  "token": "77827be8e0554e9292b7fc3044fe0c180cb243f069e54ab0bca19cdf6961e2fb",
  "openid": "ojX3z5Hk...",
  "is_new_user": true,
  "expires_at": "2026-05-25T12:51:48.510826Z"
}
```

- `is_new_user=true` 时,首次开通可能需要 3-8 秒(系统在创建 Linux 用户、启动 zeroclaw 进程、建立工作区)。建议小程序展示"正在为您准备智能助手..."loading 状态。
- 后续登录(`is_new_user=false`)立即返回。

**错误**

| HTTP | 含义 |
|------|------|
| 400  | code 无效或微信验证失败 |
| 500  | ClawOps 内部错误(进程启动失败、磁盘满等) |

---

### POST /chat

同步等待 LLM 完整回复。

**Request Header**

```
Authorization: Bearer <token>
Content-Type: application/json
```

**Request Body**

```json
{
  "content": "帮我看看高新技术企业认定的最新条件",
  "idempotency_key": "msg_xxx"   // 可选,防重发
}
```

**Response 200**

```json
{
  "response": "高新技术企业是指...",
  "model": "qwen3.6-plus",
  "openid": "ojX3z5Hk..."
}
```

**错误**

| HTTP | 含义 |
|------|------|
| 401  | token 缺失/过期/无效 → 重新登录 |
| 500  | 上游 LLM 调用失败、daemon 异常 |

**典型耗时**:5–25 秒(LLM 思考 + 联网搜索)。小程序 `wx.request` 默认超时 60 秒够用,如果用户问题复杂可放宽到 90 秒。

---

### GET /events

订阅 zeroclaw 实时事件流(SSE),用于展示"思考中 / 正在搜索 / 调用工具"等进度。

**鉴权两种方式**(微信小程序不能设 `Authorization` 头时用 query):

```
GET /events?token=<token>     ← 推荐:小程序 wx.request 不支持 SSE,但可走 chunked
GET /events                    ← 带 Authorization: Bearer <token> 头(标准 SSE 客户端)
```

**Response**:`Content-Type: text/event-stream`,持续推送事件块,每块格式:

```
data: {"type":"agent_start","provider":"qwen","model":"qwen3.6-plus","timestamp":"..."}

data: {"type":"llm_request","provider":"qwen","model":"qwen3.6-plus","timestamp":"..."}

data: {"type":"tool_call","tool":"web_search","timestamp":"..."}

data: {"type":"agent_end","duration_ms":12450,"tokens_used":2341,"timestamp":"..."}

```

事件 `type` 包括:`agent_start` / `llm_request` / `tool_call_start` / `tool_call` / `agent_end` / `error`。

**典型用法**:小程序在用户发送消息后,**并发**:
- 走 `/chat` 等最终回复(阻塞型)
- 订阅 `/events` 把进度事件渲染成进度条 / 思考过程

`/events` 在 `/chat` 完成后**不会自动关闭**(它是用户专属事件流,持久订阅);小程序自己负责在合适时机断开(切页面、消息显示完成后)。

---

### GET /health

无需鉴权。返回 `{"status":"ok","version":"0.1.0"}`。用于服务存活探测。

---

## 微信小程序示例代码

### 登录 + token 持久化

```javascript
// app.js or login page
const BASE = 'https://clawops.2048office.com';   // 通配证书,nginx 反代 -> ClawOps

async function login() {
  // 1. 拿 wx.login code
  const { code } = await wx.login();

  // 2. (可选)拿手机号(需要用户点按钮触发)
  // const phoneRes = await wx.getPhoneNumber(...)

  // 3. 调 ClawOps
  const res = await wx.request({
    url: `${BASE}/auth/wx-login`,
    method: 'POST',
    header: { 'Content-Type': 'application/json' },
    data: {
      code,
      display_name: this.userInfo?.nickName,
      // enterprise_profile: ...在用户填资料时再补
    }
  });

  if (res.statusCode !== 200) throw new Error('login failed');
  const { token, openid, is_new_user, expires_at } = res.data;

  // 4. 持久化
  wx.setStorageSync('clawops_token', token);
  wx.setStorageSync('clawops_token_expires_at', expires_at);

  if (is_new_user) {
    wx.showToast({ title: '正在为您准备...', icon: 'loading', duration: 3000 });
  }
  return token;
}

function tokenIsValid() {
  const t = wx.getStorageSync('clawops_token');
  const exp = wx.getStorageSync('clawops_token_expires_at');
  return t && exp && new Date(exp) > new Date();
}
```

### 发送消息 + 处理 401 自动重登

```javascript
async function sendMessage(content) {
  let token = wx.getStorageSync('clawops_token');
  if (!tokenIsValid()) token = await login();

  const res = await wx.request({
    url: `${BASE}/chat`,
    method: 'POST',
    header: {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${token}`
    },
    data: { content },
    timeout: 90000
  });

  if (res.statusCode === 401) {
    token = await login();
    return sendMessage(content);   // 重试一次
  }
  if (res.statusCode !== 200) throw new Error(res.data?.error || 'chat failed');
  return res.data.response;
}
```

### 订阅事件流(微信 chunked 模式)

微信小程序 **不支持** `EventSource`,但支持 `wx.request` 开启 chunked 接收(基础库 ≥ 2.20.2):

```javascript
function subscribeEvents(token, onEvent) {
  const req = wx.request({
    url: `${BASE}/events?token=${encodeURIComponent(token)}`,
    enableChunked: true,
    method: 'GET',
    header: { 'Accept': 'text/event-stream' },
    success: () => console.log('events stream done'),
    fail: (e) => console.error('events fail', e)
  });

  let buffer = '';
  req.onChunkReceived(chunk => {
    // chunk.data 是 ArrayBuffer
    buffer += new TextDecoder('utf-8').decode(new Uint8Array(chunk.data));
    // SSE 块以 \n\n 分隔
    const blocks = buffer.split('\n\n');
    buffer = blocks.pop();   // 最后一段可能不完整,留到下次
    for (const block of blocks) {
      const line = block.split('\n').find(l => l.startsWith('data:'));
      if (!line) continue;
      try {
        const event = JSON.parse(line.slice(5).trim());
        onEvent(event);
      } catch (e) {
        console.warn('bad event:', block);
      }
    }
  });

  return req;   // req.abort() 关闭流
}

// 用法
const stream = subscribeEvents(token, ev => {
  switch (ev.type) {
    case 'agent_start': /* 显示"思考中" */ break;
    case 'tool_call':    /* 显示"调用工具:" + ev.tool */ break;
    case 'agent_end':    /* 收尾 */ break;
  }
});
// 用户离开页面时:stream.abort();
```

> **注意**:`enableChunked` 需要小程序基础库 ≥ 2.20.2,且后端必须 `Transfer-Encoding: chunked`。ClawOps 用的是 axum 默认 chunked 输出,符合要求。

---

## 画像生效示例

同样问"你服务的是哪家公司?",两个用户回复完全不同:

| 用户 | enterprise_profile | LLM 实际回复 |
|------|--------------------|------|
| A | `{company_name: "测试科技", industry: "软件开发"}` | "我服务的是**测试科技**,主要在软件开发领域协助财税、政策匹配..." |
| B | `{company_name: "医药健康有限公司", goals: ["创新药申报","GMP认证"]}` | "我服务的企业是**医药健康有限公司**,近期重点方向是创新药申报与 GMP 认证..." |

这是因为 ClawOps 在 `wx-login` 创建用户时把 `enterprise_profile` 渲染进了该用户的 `USER.md`(系统提示)。小程序在用户首次完善资料时**应当**调用一个用户资料更新接口写回 ClawOps —— **目前 v0.1.0 还没暴露此接口**(用户首次注册时的 profile 是固化的,后续修改要走运维)。Phase 3 会补 `PUT /me/profile`。

---

## 已知限制与上线前必修

| 项 | 状态 | 影响 |
|----|------|------|
| HTTPS | ✅ 已上(nginx 反代 + 通配证书) | 证书 2026-06-08 到期需续签;长期建议切 Let's Encrypt 自动续 |
| `/admin/*` 鉴权 | ✅ 已加 `X-Admin-Token`(Phase 3.1) | 小程序无需关心,运维侧用 |
| Reaper 90 天清理 | ✅ 已上(Phase 3.2) | 用户 90 天无活跃自动停 daemon,工作区文件保留;再次访问自动唤醒 |
| 真实微信 `code2session` | ✅ 已切(2026-04-28) | 生产 `wx.appid` + `secret` 已配置;`mock_openid` 字段被 400 拒 |
| `PUT /me/profile` 修改企业画像 | ✅ 已上(Phase 3.5 #3) | 改后下条 /chat 即生效,无需重启 |
| Token 撤销接口 | ✅ 已上(`/auth/logout` + `/auth/logout-all`) | 用户登出立即失效,不必等 30 天过期 |
| 多端登录 token 互斥 | ⏳ 未实现 | 用户多端登录会拿不同 token,互不影响(可用 logout-all 一次清掉) |
| Rate limit | ✅ 已上(governor) | wx-login 10/分/IP,chat 30/分/用户,admin 60/分/IP;429 + Retry-After |

---

## 联系 / 调试

- ClawOps 健康检查:`curl https://clawops.2048office.com/health`
- 后端版本:见 `/health` 返回的 `version` 字段
- TLS 链路:HTTP/2 + TLSv1.3,通配证书 `*.2048office.com` 由 TrustAsia 颁发
- 后端联调请联系运维(目前是 Mario)开 admin 接口排查
