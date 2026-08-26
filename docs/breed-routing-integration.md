# 按来源分流品种（breed routing）开发文档

**读者**：企微机器人后端团队、小程序后端团队。
**目标**：让不同来源进来的用户，自动落到**不同品种的龙虾**上，而不是全部拿
`default_breed`。

> **状态说明**：ClawOps 的品种机制（`users.breed`、模板推送、按品种重渲染）
> **已上线**；本文描述的**分流**部分**尚未实现**，是待开发的契约。哪边做什么、
> 各自的工作量在第 2 节说清楚了。

---

## 1. 现状：分流点在哪、为什么现在没分

一个租户属于哪个品种，记在 `users.breed` 列上。目前只有三条路能写它：

| 入口 | 现在写什么 | 代码位置 |
|---|---|---|
| `POST /auth/wx-login`（小程序） | `breed: None` → `default_breed` | `src/http.rs` `wx_login` |
| `POST /auth/wecom-login`（企微） | `breed: None` → `default_breed` | `src/http.rs` `wecom_login` |
| `POST /admin/provision`（管理员开户） | 可选字段 `breed` | 已支持 |

也就是说：**自助注册的用户一律拿默认品种**。要分流，就得让前两条路知道
「这个人该进哪个品种」。

---

## 2. ⚠️ 两端的信任模型不同，方案必须不同

这是本文最重要的一节。**不要给两个接口加同一个字段。**

| | `/auth/wx-login` | `/auth/wecom-login` |
|---|---|---|
| 谁在调 | **用户手机上的小程序**（不可信） | **你们的企微机器人后端**（服务器到服务器） |
| 鉴权 | 无，仅按来源 IP 限流 | **`X-Admin-Token`**（`AdminGuard`） |
| 能不能让调用方直接指定品种 | **绝对不行** | **可以** |

`/auth/wx-login` 是**公网、设备侧**接口。如果加一个 `breed` 字段由客户端填，
任何人改一下请求体就能把自己开到别的品种上——那等于让用户自选人格、自选技能、
自选数据权限。**品种必须由服务端决定。**

`/auth/wecom-login` 是**带管理员令牌的服务器间**接口，调用方已经是可信的，让它
直接说明品种是安全且最简单的。

---

## 3. 小程序端：**你们不需要改任何代码**

好消息：`app_id` **已经**随每次登录传上来了。

```
POST /auth/wx-login
{"app_id": "wxABC123...", "code": "...", "display_name": "...", "avatar_url": "..."}
```

ClawOps 拿到 `app_id` 后转发给你们后端换 openid：

```
POST {backend_base_url}/message/wechat/applets/{app_id}/open_id
body: {"code": "...", "client": "clawops"}
resp: {"data": {"open_id": "..."}}
```

所以「这个人来自哪个小程序」这个信息**在分流点上已经具备**，只是没被用来选品种。

### 方案 A（推荐）：ClawOps 侧配一张映射表

**小程序端零改动，你们后端也零改动。** 只需运维在 `clawops.toml` 里加：

```toml
[provisioner.breed_routes]
# 小程序 app_id → 品种名
"wxABC123..." = "shangji"
"wxDEF456..." = "yiliao"
# 没列到的 app_id 一律走 default_breed，不报错
```

注意这**不是白名单**：未列出的 `app_id` 仍然能正常登录，只是拿默认品种。
ClawOps 现在的设计是不维护 appid 白名单（由你们后端 403 拒掉未配置的
appid），这条规矩不变。

**代价**：新增一个小程序时，运维要改一次 ClawOps 配置。小程序数量少、变动不
频繁时这是最省事的做法。

### 方案 B（小程序变多时再换）：后端在换 openid 时一并返回品种

你们后端在 `/message/wechat/applets/{app_id}/open_id` 的响应里多返回一个字段：

```json
{"data": {"open_id": "oABC...", "breed": "shangji"}}
```

ClawOps 读到就用它，读不到就用 `default_breed`（**向后兼容，字段可以先不加**）。

**好处**：品种归属和小程序配置在你们系统里是同一份数据，新增小程序不用动
ClawOps。**代价**：你们后端要加字段，且要和 ClawOps 约定品种名。

> 两个方案**不冲突**，ClawOps 会先看后端返回的 `breed`，没有再查映射表，还没有
> 才用 `default_breed`。可以先上 A，将来平滑切到 B。

---

## 4. 企微端：**必须改，因为现在连"你是哪个企业"都没传**

现状的请求体**只有三个字段**：

```
POST /auth/wecom-login          （需 X-Admin-Token）
{"uin": "7881303049925005", "display_name": "张三", "avatar_url": "https://..."}
```

**没有 corpid，没有 agentid，没有任何能区分企业/应用的字段。** 所以企微侧不是
「把已有信息用起来」，而是**要新增字段**。

### 方案 C（推荐）：直接携带品种

因为这个接口是带管理员令牌的服务器间调用，调用方可信，最简单直接：

```json
POST /auth/wecom-login
{
  "uin": "7881303049925005",
  "display_name": "张三",
  "avatar_url": "https://...",
  "breed": "shangji"          ← 新增，可选
}
```

- **可选字段**：不传 = `default_breed`，与现在行为完全一致，**老调用方不用改**
- 传了未知品种 → ClawOps 返回错误（不会把租户开到一个渲染不出东西的品种上）
- **只在新建用户时生效**。已存在的用户不会因为这里传了别的品种就被改掉——
  改已有租户的品种走第 6 节的接口，那是个会重启对方 daemon 的动作，不该由一次
  登录顺带触发

### 方案 D（备选）：传企业标识，由 ClawOps 映射

如果你们更希望品种归属由 ClawOps 统一管：

```json
{"uin": "...", "corp_id": "wwABC123...", "agent_id": "1000002"}
```

ClawOps 侧配：

```toml
[provisioner.breed_routes_wecom]
"wwABC123..." = "shangji"
"wwABC123.../1000002" = "yiliao"   # 同企业不同应用可分流
```

**建议选 C**：企微机器人后端本来就知道这次会话属于哪条业务线，直接说出来比让
ClawOps 再猜一次简单，也少维护一张表。

---

## 5. ClawOps 侧要做的（我方）

供你们对照，不需要你们实现：

1. `WxLoginReq` 分流：读后端返回的 `data.breed` → 查 `breed_routes[app_id]` →
   `default_breed`，把结果填进 `NewUser.breed`
2. `WecomLoginReq` 新增可选 `breed` 字段，同样填进 `NewUser.breed`
3. 新增 `[provisioner.breed_routes]` 配置
4. 品种合法性校验**已经有了**：`Provisioner::provision` 在消耗 linux uid 和写
   DB 行之前会先解析品种，未知品种直接失败（`src/provisioner.rs`）

---

## 6. 存量租户怎么迁移

分流只影响**新注册**用户。已经开好的租户要换品种，用：

```bash
curl -X PUT -H "X-Admin-Token: $TOKEN" -H 'Content-Type: application/json' \
  -d '{"breed":"shangji"}' \
  "$CLAWOPS_URL/admin/users/<openid>/breed"
```

返回 `{"openid":…, "breed":"shangji", "previous_breed":"default"}`。

**这个动作会立刻重渲染该租户的工作区并重启他的 daemon**，旧品种独有的技能和
SOP 会真的从工作区里消失。批量迁移请分批做，别一次几十个。

服务器上也可以用 CLI：`clawops set-breed --openid <openid> --breed <name>`。

---

## 7. 验收清单

分流上线后，按这个顺序验：

| # | 动作 | 期望 |
|---|---|---|
| 1 | 用已配置分流的小程序登录一个**全新** openid | `users.breed` = 映射的品种 |
| 2 | 用**未配置**的 app_id 登录新用户 | 能登录，`breed` = `default`，**不报错** |
| 3 | 企微用新 `uin` + `breed` 调一次 | 该租户拿到指定品种 |
| 4 | 企微**不传** `breed` 调一次 | 拿 `default_breed`（老行为不变） |
| 5 | 企微传一个不存在的品种 | 返回错误，**且没有新建 Linux 用户** |
| 6 | 用**已存在**的用户重新登录，带上不同的 `breed` | 品种**不变**（登录不改已有租户） |
| 7 | `GET /admin/breeds` | 各品种 `tenants` 数与预期一致 |

第 2 条和第 6 条最容易被漏掉，但正是它们保证分流上线不会伤到现有用户。

---

## 8. 坑

- **品种名要先在 ClawOps 上存在**。品种是先推模板、再有租户，不是反过来。分流
  配置里写一个还没推过的品种名，新用户注册会直接失败。上线顺序：先推品种 →
  再配分流。
- **不要用分流去做权限控制**。品种决定的是「这只龙虾是什么样」（人格、技能、
  SOP），不是「这个人能看什么数据」。数据权限仍然由各租户独立的工作区和后端
  接口鉴权保证。
- **小程序端不要自己传 `breed`**，前面说过原因。如果将来看到 `/auth/wx-login`
  的请求体里出现 `breed` 字段，那是个安全问题，不是新功能。
- **一个 openid 只属于一个品种**。同一个人用两个小程序进来，如果 openid 相同，
  后进的那次**不会**改变他的品种（见验收第 6 条）。真要区分，得让这两个来源
  产生不同的 openid。
