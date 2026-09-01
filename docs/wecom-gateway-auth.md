# 给企微网关的改造说明：认证

**读者**：维护企微网关（调用 ClawOps `/auth/wecom-login` 的那个服务）的人，
以及 ClawOps 自己的维护者——本文的改动**两边各占一半**。

> **本文档大部分是书面交付，不是已完成的改动。** 第 3 节的
> `[[auth_clients]]` 机制在 ClawOps 侧**尚未实现**；第 4 节网关侧的事我也没有
> 权限去做。两边都需要人工落地。
>
> **例外：3.6（过期会话清理）已经实现并测试。**
>
> 第 2 节描述的是**当前代码的真实状态**（已逐行核对）。

---

## 1. 一句话

企微网关现在拿着 ClawOps 的**管理员令牌**。它只需要「给我已认证的企微用户换
一个会话」这一项能力，却拿到了**冒充任意用户、停任意租户、改写全虾群提示词**
的权限。该换成限定权限的令牌。

---

## 2. 当前状态（核对自代码）

### 调用方式

```http
POST /auth/wecom-login
X-Admin-Token: <admin.token>
Content-Type: application/json

{"uin": "zhangsan", "display_name": "张三", "avatar_url": "https://…"}
```

返回：

```json
{"token": "<32+32 hex>", "openid": "uin:zhangsan",
 "is_new_user": true, "expires_at": "2026-09-25T…Z"}
```

### 代码事实

| 事实 | 位置 |
|---|---|
| 用 `AdminGuard` 守门，即校验 `X-Admin-Token`，常数时间比较 | `src/http.rs` `wecom_login` / `AdminGuard` |
| `openid` = `"uin:" + 请求体里的 uin`，**完全采信调用方** | `src/http.rs:373` |
| 首次调用会自动开户：建 Linux 账号、分端口、拉起守护进程 | `provisioner.provision` |
| 会话 30 天，**不滑动续期**，每次调用**新插一行** | `src/sessions.rs` `DEFAULT_TTL_DAYS` |
| `sessions.user_agent` 传的是 `None` | `src/http.rs:400` |
| 限流用的是 `admin_per_ip_per_min`，默认 **60/分钟/IP** | `src/limits.rs`、`AdminGuard` |

### `X-Admin-Token` 实际能做什么

同一个令牌，同样是 `/admin/*` 那一组路由，还能做：

| 路由 | 后果 |
|---|---|
| `POST /admin/issue-token` | 给**任意** openid 签发会话 → **冒充任何小程序用户** |
| `GET /admin/users` | 导出全部租户：openid、手机号、姓名、端口、状态 |
| `POST /admin/stop/:openid` | 停任意租户 |
| `POST /admin/refresh-all-workspaces` | 重启全虾群每一个守护进程 |
| `PUT /admin/breeds/:breed` | 改写某个品种下所有租户的提示词与技能 |
| `DELETE /admin/breeds/:breed` | 删除品种模板 |

**说清楚一点**：这不是品种机制引入的问题。`issue-token` 一条就已经等于全面失
陷，在品种之前就是如此。品种只是让爆炸半径更显眼了。

---

## 3. ClawOps 侧要改的（待实现）

### 3.1 限定权限的客户端令牌

新增配置：

```toml
[[auth_clients]]
name = "wecom-gateway-prod"
# 支持多个，轮换时新旧并存，不用两边同时改
tokens = ["ct_live_xxxxxxxx", "ct_prev_yyyyyyyy"]
scopes = ["wecom_login"]
# 可选：该客户端只能签发 uin:corp1:* 这个命名空间下的会话
uin_prefix = "corp1:"
rate_per_min = 600
```

- `/auth/wecom-login` 改为接受 `X-Client-Token`，匹配到某个带
  `wecom_login` scope 的客户端才放行
- `admin.token` 退回只管 `/admin/*`
- **向后兼容**：没有配置任何 `[[auth_clients]]` 时，`/auth/wecom-login` 仍接受
  `X-Admin-Token`，但启动时打一条 WARN。这样升级二进制不会当场打断网关，切换
  可以分两步走

### 3.2 独立限流，按客户端而不是按 IP

现在共用 `admin_per_ip_per_min = 60`，这个数**两个方向都是错的**：

- **太小**：一个公司早高峰登录，网关一个源 IP 打过来轻松超 60/分钟，用户会
  拿到 429
- **太大**：令牌一旦泄漏，60/分钟足够慢慢把用户表刷出来

而且网关通常在 NAT 或负载均衡后面——**按源 IP 限流对它没有意义**，同一个网关
可能有多个出口 IP，不同网关也可能共用一个。改成按 `name` 计数。

### 3.3 记下是谁签发的

`sessions` 表加 `issued_by` 列（或者至少把 `user_agent` 填上客户端名）。现在网
关签发的会话和攻击者拿令牌签发的会话，在库里**长得一模一样**，事后无从分辨。

### 3.4 `uin_prefix` 校验

将来接第二家企微时，没有这条就没有任何机制阻止 A 网关签发 B 公司用户的会话。
现在只有一个网关，所以是潜在问题；**接第二家的那天就是现实问题**。

### 3.5 启动时拒绝把 admin token 配成客户端令牌

否则整件事白做。

### 3.6 过期会话从来没被清理过 —— ✅ 已修

`sessions::purge_expired()` 从一开始就写了，但**全代码没有任何地方调用它**。
`migrations/0002_sessions.sql` 的注释写着「expired rows are pruned by Reaper
later」，而 Reaper 只处理 `users` 表，不碰 `sessions`。后果是 `sessions` 表
**只增不减**。

**已经挂进 Reaper 的每次 tick**（默认每小时一次），`clawops reap` 也会顺带执
行。停闲置守护进程与清理会话是同一 tick 里的两件独立事情：清理失败只 warn，不
会把整个 tick 变成失败而掩盖掉前者已经成功。

```
$ clawops reap
reaper one-shot: stopped 0 idle user(s), purged 137 expired session(s)
```

这条修掉了**存量堆积**，但 4.5 那条仍然要做：网关每条消息都换一次会话的话，
新行的产生速度会远超每小时一次的清理，而且每一行在 30 天内都还是有效的、**清
不掉**。

---

## 4. 网关侧要改的

### 4.1 换令牌

`X-Admin-Token: <admin token>` → `X-Client-Token: <客户端令牌>`。
向 ClawOps 运维索取一个 `scopes = ["wecom_login"]` 的令牌。

**在 3.1 落地之前**，网关手上仍是 admin token，因此**必须**按生产密钥对待：不
进仓库、不进配置文件明文、不打进日志，只从环境变量或密钥管理注入。

### 4.2 `uin` 必须由网关自己认证得出，不能由调用方传入

ClawOps **完全采信**请求体里的 `uin`，它没有办法验证。所以整条链路的安全边界
就落在网关身上：

- `uin` 只能来自网关自己完成的企微 OAuth 换取（`code` → `gettoken` →
  `getuserinfo` 拿到 `UserId`）
- 网关**不能**对外暴露任何「调用方指定 uin」的接口
- 如果有「以某某身份登录」这类调试路由，**上线前删掉**

这一条是整份文档里最重要的。前面所有令牌工作，防的都是令牌泄漏；这一条防的是
逻辑漏洞，而逻辑漏洞不需要任何令牌泄漏就能被利用。

### 4.3 401 不要重试风暴

令牌错了重试一万次也还是错。401 直接失败并告警，别退避重试。

### 4.4 429 要退避

按 `Retry-After` 响应头退避。ClawOps 限流时返回：

```json
{"error": "rate_limited", "retry_after_secs": 12}
```

并带 `Retry-After` 头。

### 4.5 缓存会话令牌，不要每条消息都换一次

`/auth/wecom-login` **每次调用都会新插一行会话**（不是幂等的，不复用现有会
话）。会话有效期 30 天。

网关应当按 uin 缓存 `token` 直到 `expires_at`，只在缓存缺失或过期时才调用。
每条消息都调一次的话：一次多余的数据库写、一次多余的用户查询，并且直接喂大
3.6 里那张永不清理的表。

### 4.6 首次调用可能很慢

某个 uin 第一次登录会触发**开户**：建 Linux 账号、`loginctl enable-linger`、
分端口、拉起守护进程、等健康检查（最多 20 秒）。

网关这一侧的 HTTP 超时**要能容得下 30 秒**，否则新用户第一次进来必然超时，而
后台其实开户成功了——表现为「第一次点没反应，第二次就好了」。

---

## 5. 切换顺序（顺序不可颠倒）

1. ⬜ ClawOps 实现 3.1–3.5，部署。此时**没有** `[[auth_clients]]` 配置，网关照
   旧用 admin token，一切不变
2. ⬜ 在 `clawops.toml` 里加一条 `[[auth_clients]]`，签发客户端令牌，重启
3. ⬜ 把令牌给网关；网关改成 `X-Client-Token`，灰度、观察
4. ⬜ 确认 ClawOps 日志里 `/auth/wecom-login` 已全部来自客户端令牌
5. ⬜ **最后**才轮换 `admin.token`（网关不再依赖它了，可以放心换）

第 5 步放最后是有原因的：只要网关还在用 admin token，轮换它就会当场打断所有企
微用户登录。

---

## 6. 明确不在本次范围内的

- **`uin` 的密码学证明。** 让 ClawOps 独立验证企微身份，需要网关传企微签名或
  由 ClawOps 自己持有企微凭据。前者要企微侧配合，后者会让 ClawOps 也变成企微
  凭据的持有方。现阶段「网关是可信引入方」这个模型是合理的——前提是 4.2 做到
  位。
- **会话滑动续期。** 现在是签发即定 30 天。够用。
- **mTLS。** 一个限定权限的令牌 + TLS 已经把风险压到位；mTLS 的运维成本在当前
  规模下不划算。
