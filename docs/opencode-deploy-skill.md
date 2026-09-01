# 给 OpenCode 的改造说明：龙虾部署技能

**读者**：维护 `https://oc.2048office.com/` 上那套 OpenCode / LoopClaw 的人。
**目标**：在 OpenCode 里调好一只 ZeroClaw 之后，**一条命令**把它的配置、技能、
SOP、脚本推进 ClawOps 虾群，并让该品种的租户立刻用上。

> **本文档是书面交付，不是已完成的改动。** `https://oc.2048office.com/` 在
> 我这边的出网策略里被拒（CONNECT 返回 403），SSH 也不通，所以我没有看过现
> 有部署技能的实现，也没有改它。下面写的是**接口契约和必须满足的行为**，落
> 地需要人工做一次。

ClawOps 这一侧**已经实现并测试完毕**，分支
`claude/zeroclaw-config-sync-5vydaj`。契约细节见
[`breed-sync.md`](breed-sync.md)。

---

## 1. 先理解「品种」

ClawOps 现在按**品种(breed)**区分不同的龙虾。一个品种 = 一棵 handlebars 模板
树；`users.breed` 决定某个租户从哪一棵渲染。一台 ClawOps 可以同时养多种，互不
干扰。

OpenCode 里开发的那只龙虾，对应的就是**一个品种**。部署技能要做的事，就是把
这棵树推过去。

---

## 2. OpenCode 项目里的目录约定

```
<project>/
├── breed/                      ← 推的就是这个目录，别的都不推
│   ├── config.toml.hbs         必需。缺了直接被服务端拒绝
│   ├── IDENTITY.md.hbs
│   ├── SOUL.md.hbs
│   ├── USER.md.hbs
│   ├── skills/<name>/SKILL.md.hbs
│   ├── sops/<name>/SOP.toml.hbs
│   ├── sops/<name>/SOP.md.hbs
│   └── scripts/…               原样拷贝，不做模板渲染
├── .clawops                    ← 推给谁、叫什么名字
└── (技能实现) push-breed.sh    ← 从 clawops 仓库拷过来
```

### 为什么是 `.hbs`

ClawOps 给**每个租户**单独渲染一份工作区：企业名、用户名、端口、
paired_token、模型、API key 都是逐租户不同的。所以推过去的必须是模板，不是某
一次渲染的结果。

**但绝大多数文件根本不需要改。** 技能和 SOP 基本不含插值——现有的
`SKILL.md` 直接 `mv SKILL.md SKILL.md.hbs`，内容一个字不动即可。真正需要占位
符的只有三个文件：

| 文件 | 需要什么 |
|---|---|
| `config.toml.hbs` | `{{llm.default_model}}`、`{{llm.api_key}}`、`{{port}}`、`{{paired_token}}`、`{{http_allowed_domains_toml}}` |
| `IDENTITY.md.hbs` | `{{display_name}}`、`{{enterprise.company_name}}` |
| `USER.md.hbs` | 同上 |

这三个可以直接抄 clawops 仓库 `templates/workspace/` 里现成的那份，改文案即可。
可用变量的完整清单在 `Provisioner::build_ctx`（`src/provisioner.rs`）。

### `.clawops`

shell 可直接 source 的格式：

```sh
CLAWOPS_BREED=shangji
CLAWOPS_URL=https://ai.example.com/clawops
# token 不进仓库
```

---

## 3. 技能的实现

把现有「部署」技能的实现整个换成：

```bash
set -a; . ./.clawops; set +a
: "${CLAWOPS_ADMIN_TOKEN:?未设置：请在 OpenCode 的密钥管理里配好}"
./push-breed.sh --breed "$CLAWOPS_BREED" --dir ./breed
```

`push-breed.sh` 在 clawops 仓库的 `scripts/` 下，**整个拷过来**即可。它只依赖
`bash / curl / tar / sha256sum / find`，没有别的依赖。

它替你做了五件事：

1. **确认 `--dir` 是模板树**（有 `config.toml.hbs`），不是渲染后的工作区
2. **扫密钥并拒推**——`sk-…`、`zc_…`、`enc2:`、字面量 `api_key = "…"`。
   `{{llm.api_key}}` 这种正确写法不误报。模板会渲染进**每一个**租户的工作区，
   写死的 key 等于发给了所有租户
3. **算 digest 与服务端比对**（与服务端逐字节相同的算法：排序后的
   `路径\0文件哈希\n` 再 sha256）。**一致就直接退出**
4. 打包成 tar.gz，`PUT /admin/breeds/<name>`
5. 有租户没起来时以 exit 3 退出，并打印重试命令

### 退出码

技能层按这四个码给用户不同的话：

| 码 | 含义 | 该说什么 |
|---|---|---|
| 0 | 推成功，或本来就是最新 | 看 stdout：`already current` 就是没变化 |
| 1 | 参数/校验不过（含密钥泄漏） | 把 stderr 原样给用户，这是他要改的 |
| 2 | 服务端拒绝 | 400 一般是模板语法错，**带文件名和行号**，直接展示 |
| 3 | 模板已生效，但有租户没重启起来 | 明确区分：**推成功了**，是个别租户要单独看 |

第 3 种最容易说错。到那一步模板**已经上线**了，说成「部署失败」会让人重推，
而重推是无效的（digest 一致会直接跳过）。

---

## 4. 技能描述里必须写清的四件事

1. **推送会重启该品种所有租户的守护进程。** 在途的 `/chat` 会失败一次，SSE
   会断开。客户端看到的是连接被关闭，不是错误响应。**不是无损操作，别在高峰
   期推。**
2. **`default` 品种推不动**（服务端返回 400）。主线那只龙虾随二进制一起发布，
   走 git，不走这个技能。
3. **digest 一致 = 已经是最新。** 这种情况下技能不该假装做了什么，也不该"为
   保险起见"加 `--force`——那会白白重启一遍所有租户。
4. **技能不持有 admin token。** 从 OpenCode 的密钥管理里取，不落仓库、不落
   日志。见下节。

---

## 5. 凭据

`push-breed.sh` 目前用 `X-Admin-Token`，也就是 **ClawOps 的管理员令牌**。这个
令牌能做的远不止推模板：列出全部租户、停任意租户、给**任意** openid 签发会话
（即冒充任何用户）、删除品种。

> 这条与企微网关那边是**同一个问题**，处理方式也应当一致：给调用方发**限定
> 权限的令牌**，而不是万能钥匙。详见
> [`wecom-gateway-auth.md`](wecom-gateway-auth.md) 第 3 节——那份文档里提的
> `[[auth_clients]]` 机制，给 OpenCode 发一个 `scopes = ["breed_push"]` 的令
> 牌即可，它就只能推模板，别的一概不行。
>
> **该机制目前尚未实现**（ClawOps 侧待办）。在它落地之前，OpenCode 用的是
> admin token，因此：**这个令牌必须当作生产密钥对待**——不进仓库、不进技能
> 定义文件、不打进日志，只从密钥管理里注入。

---

## 6. 验收清单

改完之后，按这个顺序自测一遍：

1. ⬜ `./push-breed.sh --breed <name> --dir ./breed --dry-run`
   → 打出 digest，不发任何请求
2. ⬜ 故意在 `IDENTITY.md.hbs` 里写 `{{#if x}}` 不闭合，再推
   → exit 2，错误里有**文件名和行号**，且线上模板**没被改动**
3. ⬜ 故意在 `config.toml.hbs` 里写 `api_key = "sk-realkeyhere123456"`，再推
   → exit 1，**没有发出任何网络请求**
4. ⬜ 正常推一次 → exit 0，返回 JSON 里 `refreshed` 等于该品种租户数、
   `failures` 为空
5. ⬜ **紧接着再推一次** → exit 0 且输出 `already current`，**没有**重启任何
   租户
6. ⬜ 改一个字再推 → digest 变化，租户被重新渲染
7. ⬜ 断网/填错 URL → 报错清楚，不是静默成功

第 5 条最值得认真测：它是「一条命令」能被放心反复运行的前提。
