# 熵玑参谋 · 小程序对接文档

**版本**：2026-08-05　**环境**：中伯伦 ECS `47.94.58.57`　**网关**：ClawOps（分支 `tenant/zhongbolun`）

本文档面向三方：**小程序前端**、**小程序后端**、**运维**。所有接口字段均来自源码核对，字段名大小写与下划线**必须逐字一致**，错一个字母登录链路就断。

---

## 一、架构总览

```
微信小程序
   │  ① wx.login() 拿 code
   ▼
小程序前端 ──② POST /clawops/auth/wx-login {app_id, code} ──► ClawOps 网关
                                                                │
                                        ③ 用 code 换 openid     │
                                                                ▼
                                                    中伯伦小程序后端
                                          POST /message/wechat/applets/{app_id}/open_id
                                                                │
                                        ④ 返回 {data:{open_id}} │
                                                                ▼
                                    ClawOps 建租户 + 签发 session token
                                                                │
   ◄──────────────── ⑤ {token, openid, is_new_user, expires_at} ┘
   │
   │  ⑥ 后续所有请求带 Authorization: Bearer <token>
   ▼
POST /clawops/chat ──► ClawOps ──► 该用户专属的 zeroclaw 守护进程（熵玑参谋）
                                              │
                                              ├─ 企业快评 SOP（enterprise-quick-review）
                                              ├─ 企业检索 skill（enterprise-search）
                                              └─ 内部企业库（MySQL sjldk）
```

**关键概念**：ClawOps 是多租户网关。**每个微信用户 = 一个独立的 Linux 账户 + 一个独立的 zeroclaw 守护进程 + 一份独立的工作区和记忆**。用户之间完全隔离。首次登录会触发开户（provision），耗时较长（见 §3.3）。

---

## 二、接入地址与前置条件

| 项 | 值 |
|---|---|
| 域名 | `https://ai.infocts.cn` |
| 路径前缀 | `/clawops/` |
| 完整示例 | `https://ai.infocts.cn/clawops/chat` |
| TLS | Let's Encrypt，有效期至 **2026-10-30**（到期前须续签，否则小程序全部请求失败） |
| 网关监听 | `127.0.0.1:8088`（仅本机，由 nginx 反代） |
| 反代超时 | `proxy_read_timeout 900s`（对话可能长达数分钟） |

### ⚠️ 上线前必做（当前状态：公网关闭）

`/clawops/` 目前配置为 **仅内网可访问**（`deny all`），公网访问返回 **403**。原因：`wx.backend_base_url` 尚未配置，ClawOps 处于 **mock 模式**——该模式下任何人可用 `mock_openid` 字段冒充任意用户。

**开放公网的前提**：小程序后端实现 §4 的 code2session 接口，并把地址填进 `/etc/clawops/clawops.toml` 的 `[wx] backend_base_url`。填好后再移除 nginx 里的 `allow/deny` 段。**顺序不能反。**

另需在微信小程序后台把 `https://ai.infocts.cn` 加入 **request 合法域名**（域名须已备案）。

---

## 三、小程序前端对接

### 3.1 登录：`POST /clawops/auth/wx-login`

**无需鉴权**（按来源 IP 限流，默认 10 次/分钟，超限返回 429）。

**请求体**（全部 snake_case）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `app_id` | string | **是** | 小程序 appid。生产环境为空会报错 |
| `code` | string | **是** | `wx.login()` 返回的 code。生产环境为空会报错 |
| `display_name` | string | 否 | 昵称，来自 `<input type="nickname">` |
| `avatar_url` | string | 否 | 头像 URL。**ClawOps 原样存储，不做转存**，请自行换成永久链接 |
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

### 3.2 对话：`POST /clawops/chat`

**鉴权**：`Authorization: Bearer <token>`（注意 `Bearer` 后**一个空格**，大小写敏感）。
**限流**：按用户 30 次/分钟。

**请求体**：

| 字段 | 类型 | 必填 |
|---|---|---|
| `content` | string | **是** |
| `idempotency_key` | string | 否（透传给下游，ClawOps 自身不去重） |

**响应体**：

| 字段 | 类型 | 说明 |
|---|---|---|
| `response` | string | 助手回复文本 |
| `model` | string \| null | 如 `deepseek-v4-pro`；被网关拦截时为 `clawops-router` |
| `openid` | string | 当前用户 openid |

**⚠️ 超时不匹配（必须处理）**：ClawOps 对下游的超时是 **900 秒**，而 `wx.request` 默认只有 **60 秒**。企业快评这类任务动辄数分钟，**前端必须显式设置 `timeout: 120000` 以上**，并配合 §3.5 的任务列表做长任务感知。

### 3.3 首次登录耗时（体验关键）

`is_new_user = true` 时，服务端正在做：建 Linux 账户 → 渲染工作区 → 分配端口 → 拉起守护进程 → 健康检查。**这个过程可能要 20 秒以上**。

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

另外：**ClawOps 从不返回 403**。若收到 403，那是 nginx 的访问控制（见 §2 公网未开放）。

### 3.5 其它前端接口

| 接口 | 方法 | 鉴权 | 用途 |
|---|---|---|---|
| `/clawops/me/profile` | GET / PUT | Bearer | 读取/更新用户画像。PUT 的字段全部可选，不传=不修改，**无法置空** |
| `/clawops/me/chat-history` | GET | Bearer | 历史消息。参数 `before_id`、`limit`（默认 20，上限 100）。返回按 **id 倒序**（新的在前），带 `has_more` / `next_cursor` |
| `/clawops/me/sop/tasks` | GET | Bearer | 长任务列表。参数 `status`、`sop_name`、`limit`（默认 50，上限 200）。**只返回最近 30 天，无分页游标** |
| `/clawops/events` | GET | Bearer 或 `?token=` | SSE 事件流，推送任务状态变化 |
| `/clawops/auth/logout` | POST | 手工读 Bearer | 注销当前 token，**永远返回 200** |
| `/clawops/auth/logout-all` | POST | Bearer | 注销该用户全部 token |
| `/clawops/health` | GET | 无 | 健康检查 |

**SOP 任务状态**：`pending` / `running` / `done` / `failed` 四值。任务对象字段：`task_id`、`sop_name`、`sop_name_cn`、`enterprise_name`、`status`、`deeplink`、`error`（**注意 JSON 字段名是 `error` 而非 `error_message`**）、`estimated_seconds`、`created_at`、`completed_at`。

**⚠️ `/events` 的鉴权失败返回 500 而不是 401**（已知问题），且**没有心跳帧**，需自行处理断线重连。

---

## 四、小程序后端必须实现的接口

这是 **唯一** 需要中伯伦后端开发的接口。ClawOps **不持有小程序的 appid/secret**，也不直接调用微信的 `jscode2session`，而是把 code 转发给你们的后端去换。

### 4.1 接口契约

```
POST {backend_base_url}/message/wechat/applets/{app_id}/open_id
```

- `backend_base_url` 由运维填进 ClawOps 配置，尾部斜杠会被自动去除
- `{app_id}` 直接拼进 URL 路径（**ClawOps 未做 URL encode**）
- Content-Type: `application/json`
- **不带任何认证头**（无 Authorization、无 admin token）——请自行用网络层或 IP 白名单保护
- 超时：**30 秒**

### 4.2 请求体（ClawOps 发给你们）

```json
{
  "code": "<wx.login 返回的 code>",
  "client": "clawops"
}
```

只有这两个字段。`client` 恒为字面量 `"clawops"`。**app_id 不在 body 里，只在 URL 路径上。**

### 4.3 响应体（你们返回给 ClawOps）

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

失败时可返回非 2xx，并在顶层放 `message` 字段说明原因，ClawOps 会把它透传给前端。

你们内部的实现就是拿 `code` + appid + secret 调微信官方 `jscode2session`，把拿到的 openid 按上面格式包一层返回。

### 4.4 错误码对照（便于联调）

ClawOps 会把后端的失败包装成 HTTP 400 + 结构化错误：

| errcode | errmsg | 触发条件 |
|---|---|---|
| `-10001` | `empty code (call wx.login first)` | 前端没传 code |
| `-10002` | `backend returned empty open_id` | 你们返回的 `data.open_id` 为空或缺失 |
| `-10003` | `missing app_id` | 前端没传 app_id |
| `-10004` | `backend returned non-JSON body: ...` | 你们返回的不是合法 JSON |
| `403` / `500` 等 | 你们的 `message` 字段 | 你们返回了非 2xx，errcode 即你们的 HTTP 状态码 |

**注意**：后端返回的 403 会被降级成 ClawOps 的 HTTP **400**（errcode 里保留 403）。ClawOps 自身对登录接口永远不返回 401/403。

---

## 五、企微接口（保留，待后续实现）

已实现并保留，当前不启用。**这是服务端到服务端接口，小程序不应直接调用。**

### `POST /clawops/auth/wecom-login`

- **鉴权：`X-Admin-Token`**（管理员令牌，由运维保管，切勿下发到客户端）
- 请求体：`uin`（**必填**）、`display_name`（可选）、`avatar_url`（可选）
- 响应体：与 `/auth/wx-login` 完全相同
- openid 会被合成为 **`uin:<uin>`** 形式，网关内部据此区分企微用户与小程序用户
- **每次调用都签发新 token，旧 token 不失效**

**后续接企微时的正确姿势**：企微服务端为每个用户先调此接口拿 token，用该 token 作 Bearer 调 `/chat`，遇 401 就重新登录。**不要缓存来路不明的 token，也不要自己生成 token**——网关只认自己签发的那一份。

---

## 六、能力说明：熵玑参谋能做什么

每个租户的工作区包含：

| 类型 | 名称 | 说明 |
|---|---|---|
| SOP | `enterprise-quick-review` | **企业快评**：公司名消歧 → 缓存检查 → 查内部企业库 → 五板块多源检索 → 简报 + 分享卡片 + ≤800 字汇报 |
| Skill | `enterprise-search` | 企业检索：按行业/地域/融资阶段筛选企业 |

**触发方式**：用户自然语言即可，如"查一下 XX 公司""XX 这家公司怎么样""帮我看看 XX"。SOP 会自动启动，属于长任务，进度通过 `/me/sop/tasks` 或 `/events` 感知。

**产物路径**（在该用户工作区内）：
- 简报：`briefs/<company_slug>/brief_<YYYY-MM-DD>.md`
- 分享卡片：`briefs/cards/<company_slug>_<YYYY-MM-DD>.png`（750×1000 竖版）

**模型**：DeepSeek `deepseek-v4-pro`（官方 API），推理模型。

---

## 七、运维备忘

| 项 | 位置/值 |
|---|---|
| 配置文件 | `/etc/clawops/clawops.toml`（权限 600） |
| 模板 | `/etc/clawops/templates/workspace/` |
| 数据库 | `/var/lib/clawops/data/clawops.db` |
| 源码 | `/opt/clawops`，分支 **`tenant/zhongbolun`** |
| 服务 | `systemctl {status,restart} clawops` |
| 租户工作区 | `/home/claw-NNN/.zeroclaw/workspace/` |
| 租户端口段 | **43000–50000**（避开存量 41101/41102/42618/42619） |
| 共享环境变量 | `/etc/clawops/zeroclaw.env`（MySQL 凭据，经 systemd 注入所有租户） |
| nginx 配置 | `/etc/nginx/sites-enabled/ai`（**注意：这份才生效，`sites-available/ai` 是另一份且已漂移**） |
| nginx 备份 | `/root/nginx-backups/`（**切勿把备份放进 `sites-enabled/`**，通配符会把它当配置加载） |

**改模板后下发到已有租户**：
```
curl -X POST http://127.0.0.1:8088/admin/refresh-all-workspaces -H "X-Admin-Token: <token>"
```

**⚠️ 分支纪律**：本实例走 `tenant/zhongbolun`。老服务器（2048）的 deploy.sh 拉的是 `main`，**切勿把熵玑参谋的模板合进 main**，否则会冲掉 2048 的租户模板。

**⚠️ 密钥不可跨实例复制**：zeroclaw 的 `api_key` 若以 `enc2:` 开头即为密文，只能被生成它的那份 `.secret_key` 解开。给 ClawOps 配置时**必须填明文**，否则租户守护进程启动即失败，且报错会误导为 `zeroclaw not reachable ... after 20000ms`。

---

## 八、当前状态与待办

| 项 | 状态 |
|---|---|
| ClawOps 网关 | ✅ 运行中（`127.0.0.1:8088`） |
| 熵玑参谋模板 | ✅ 已上线，零怀柔/2048 残留 |
| DeepSeek 模型 | ✅ `deepseek-v4-pro` 已验证可用 |
| 企业快评 SOP + 检索 skill | ✅ 已下发 |
| 内部企业库（MySQL） | ✅ 凭据已注入租户 |
| 端到端对话 | ✅ 已验证 |
| **小程序后端 code2session** | ⬜ **待中伯伦开发**（§4） |
| **公网开放** | ⬜ 待后端就绪后移除 nginx 的 `deny all` |
| 企微接入 | ⬜ 接口已就绪，待实现 |
