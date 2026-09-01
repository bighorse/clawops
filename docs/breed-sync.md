# 龙虾品种同步（breed sync）

一条命令，把在开发端（OpenCode / LoopClaw）调好的那只龙虾——它的
`config.toml`、`SOUL.md`、`IDENTITY.md`、`skills/`、`sops/`、`scripts/`——
推进 ClawOps 虾群，并让该品种下的租户立刻用上新模板。

```bash
scripts/push-breed.sh --breed shangji --dir ./breed
```

本文是**接口契约**：ClawOps 这边已经实现完，开发端（OpenCode）的部署技能照
着这份改就能对上。

---

## 为什么需要「品种」

在此之前，一台 ClawOps 只有一个 `provisioner.template_dir`，**所有租户渲染
自同一套模板**。要跑第二种龙虾，唯一办法是把整个仓库分叉成一个分支、再单独
部署一台服务器——`tenant/zhongbolun` 分支就是这么来的，它的
`templates/workspace/` 直接把主线的 7 个技能删掉换成了「熵玑参谋」那一套。
该分支的运维备忘里写着「**不要切到 main，也不要把本分支的模板合并出去**」，
这句纪律本身就说明了问题：模板是全局的，两个品种在同一进程里没法共存。

引入品种后，区分点从**分支**下沉到**数据行**：

| | 之前 | 现在 |
|---|---|---|
| 区分方式 | git 分支 + 独立服务器 | `users.breed` 列 |
| 一台机器能跑几种龙虾 | 1 | 任意多种 |
| 上线新版本 | ssh、git pull、cargo build、换二进制 | `push-breed.sh` |
| 影响面 | 整台机器 | 只有该品种的租户 |

---

## 品种是什么

一个品种 = 一棵 handlebars 模板树：

```
<breed>/
├── config.toml.hbs        必需——没有它守护进程起不来，推送会被拒
├── IDENTITY.md.hbs
├── SOUL.md.hbs
├── USER.md.hbs
├── AGENTS.md.hbs          可选，但有就必须推——见下
├── MEMORY.md.hbs          可选
├── HEARTBEAT.md.hbs       可选
├── TOOLS.md.hbs           可选
├── skills/<name>/SKILL.md.hbs
├── sops/<name>/SOP.toml.hbs
├── sops/<name>/SOP.md.hbs
└── scripts/…              原样拷贝，**不做模板渲染**
```

那四个「可选」不是可有可无：zeroclaw 把它们**全部**塞进系统提示词
（`agent/prompt.rs`），而且 `AGENTS.md` 还驱动 `security/policy.rs`（安全
策略）、`HEARTBEAT.md` 还驱动 `heartbeat/engine.rs`（定时任务）。在 OpenCode
工作台里调好的龙虾如果用了它们而品种里没带，推到虾群后会**安静地变成另一
只**——不报错，只是行为不同。反过来，租户换到不带这些文件的品种时，工作区里
的旧文件会被**删掉**，不会留着继续生效。

`scripts/` 不渲染是有意的：里面是 Python 源码和二进制资源，满是花括号，
过一遍 handlebars 只会被改坏。脚本运行期需要的东西从环境变量拿
（`systemd/zeroclaw@.service` 的 `EnvironmentFile`），不从模板上下文拿。

渲染时可用的变量见 `Provisioner::build_ctx`（`src/provisioner.rs`）。常用的：
`{{display_name}}`、`{{openid}}`、`{{port}}`、`{{paired_token}}`、
`{{breed}}`、`{{enterprise.company_name}}`、`{{llm.default_model}}`、
`{{llm.api_key}}`、`{{http_allowed_domains_toml}}`、
`{{sop_webhook.event_webhook_url}}`。

**密钥不进 bundle。** 模板会渲染进每个租户的工作区，写死在里面的 key 等于
发给了所有租户。密钥放 `clawops.toml`，模板里写 `{{llm.api_key}}`。
`push-breed.sh` 会扫描并拒绝明显的泄漏。

### 保留名 `default`

`default` 品种由 `provisioner.template_dir` 支撑，也就是仓库里
`templates/workspace/`。它**只读**：`PUT /admin/breeds/default` 会被拒。
理由是它随二进制一起发布，允许覆盖会让机器上的 git 工作区与实际运行的模板
对不上——这正是 `tenant/zhongbolun` 运维备忘里「服务器 `/opt/clawops` 的工
作区长期是脏的」那条坑。要改它，走 git。

---

## 配置

```toml
[provisioner]
backend = "systemd"
template_dir = "/etc/clawops/templates/workspace"   # default 品种，照旧
breeds_dir  = "/etc/clawops/breeds"                 # 新增：其余品种落在这
default_breed = "default"                           # 新租户默认拿哪种
max_bundle_bytes = 33554432                         # 32 MiB
```

`breeds_dir` 不填即**单品种模式**：行为与升级前逐字节一致，`/admin/breeds/*`
的写操作返回 503。已有部署升级上来不需要改任何配置。

`breeds_dir` 必须与 `template_dir` 在同一文件系统上无所谓，但它**自身**要可
写：安装时先解到 `<breeds_dir>/.staging-*`，校验通过后用 `rename` 原子换入。

---

## HTTP 接口

全部要 `X-Admin-Token`，全部走 `/admin/*` 的限流（默认 60 次/分钟/IP）。

### `GET /admin/breeds`

```json
{
  "default_breed": "default",
  "breeds_dir": "/etc/clawops/breeds",
  "breeds": [
    {"name":"default","builtin":true,"files":14,"digest":"caf32ff8…","tenants":31,
     "path":"/etc/clawops/templates/workspace"},
    {"name":"shangji","builtin":false,"files":9,"digest":"ac2f0d8d…","tenants":7,
     "path":"/etc/clawops/breeds/shangji"}
  ]
}
```

`digest` 是整棵树的 sha256。**开发端和服务端的 digest 相同，就说明虾群跑的
就是你手上这份**——这是「我推的到底上没上」唯一可靠的答案。

### `GET /admin/breeds/:breed`

同上再附一份 `manifest`（`相对路径 -> 文件 sha256`），用来定位到底哪个文件不
一样。

### `PUT /admin/breeds/:breed`

请求体是模板树的 tar（gzip 可选，按 magic 自动识别）。

```bash
tar -C ./breed -czf - . | curl -X PUT --data-binary @- \
  -H "X-Admin-Token: $CLAWOPS_ADMIN_TOKEN" \
  -H 'Content-Type: application/gzip' \
  https://ai.example.com/clawops/admin/breeds/shangji
```

查询参数 `?refresh=false` 只安装不下发（品种还没有租户时用）。默认下发。

成功返回 200：

```json
{"breed":"shangji","digest":"ac2f0d8d…","files":9,
 "path":"/etc/clawops/breeds/shangji","tenants":7,
 "refreshed":7,"failures":[]}
```

`refreshed` 是**重新渲染并重启成功**的租户数。`failures` 里是没起来的，
格式 `{"openid":…,"error":…}`。注意：走到这一步模板**已经生效**了，
`failures` 非空不代表推送失败，代表有租户要单独去看——补救用
`POST /admin/breeds/:breed/refresh`。

落地顺序（任何一步失败都不动线上目录）：

1. 解包到 `<breeds_dir>/.staging-<breed>-<uuid>/`
   —— 拒绝 `..`／绝对路径／符号链接／硬链接，限制条目数和解压后总字节数
2. 校验：`config.toml.hbs` 必须在；**每个 `.hbs` 都过一遍 handlebars 编译**
3. 现有目录 `rename` 到 `.trash-*`，staging `rename` 进来；第二步 rename
   失败则把旧的 rename 回去
4. 删 `.trash-*`
5. `refresh=true` 时逐个重渲染该品种的租户

第 2 步的模板编译是最值钱的一环：模板语法错在推送时就被挡住，而不是三小时
后某个租户发第一条消息时才炸。

失败码：`400` bundle 有问题（含具体文件名和行号）、`401` token 不对、
`404` 品种不存在（GET）、`409` 品种还有租户（DELETE）、`503` 没配
`breeds_dir`。

### `POST /admin/breeds/:breed/refresh`

不上传，只把该品种的租户重渲染一遍。用于：模板是直接在服务器上改的；或者
上次推送有 `failures` 要重试。

### `DELETE /admin/breeds/:breed`

删模板目录。**还有租户绑着就拒绝（409）**——把模板从活租户脚下抽走，只会让
他们冻结在工作区里已有的那份，下次 refresh 直接 404。

### `PUT /admin/users/:openid/breed`

```json
{"breed": "shangji"}
```

把一个租户换品种，并立刻重渲染。旧品种独有的技能／SOP 会从工作区里消失——
`render_workspace` 每次都先清空 `skills/`、`sops/`、`scripts/` 再写，所以
删掉的能力是真的删掉了，不会留在租户那里继续被加载。

### `POST /admin/provision`

多了可选字段 `breed`。不填走 `default_breed`。

> `/auth/wx-login` 和 `/auth/wecom-login` 自助注册的租户一律拿
> `default_breed`。要按小程序 appid 分流品种的话，得在这两个 handler 里加
> 映射——目前**没有**实现，别以为配了就生效。

---

## 命令行

服务器上没有 admin token 在手时用，走的是同一套代码：

```bash
clawops breeds                                     # 列品种 + digest + 租户数
clawops install-breed --breed shangji --bundle x.tar.gz
clawops refresh-breed --breed shangji
clawops set-breed --openid o_xxx --breed shangji
clawops provision --openid o_xxx --breed shangji
clawops list                                       # 现在带 breed 列
```

`clawops breeds` 还会对「有租户、但模板目录不存在」的品种打 WARNING——那批
租户的下一次 refresh 必然失败，值得当场看见。

---

## OpenCode 端的部署技能怎么改

> 下面这段是给 OpenCode 那边的改造说明。`https://oc.2048office.com/`
> 在本次会话的出网策略里被拦（CONNECT 返回 403），所以这份代码没有直接落到
> 那边，需要人工搬一次。

### 一、开发端的目录约定

OpenCode 项目里的龙虾源码就是一棵**模板树**，不是渲染后的工作区：

```
<project>/
├── breed/                  ← 推送的就是这个目录
│   ├── config.toml.hbs
│   ├── IDENTITY.md.hbs
│   ├── SOUL.md.hbs
│   ├── USER.md.hbs
│   ├── skills/…/SKILL.md.hbs
│   ├── sops/…/SOP.{toml,md}.hbs
│   └── scripts/…
└── .clawops                ← 推给谁、叫什么名字
```

`.clawops` 用 shell 可直接 source 的格式：

```sh
CLAWOPS_BREED=shangji
CLAWOPS_URL=https://ai.infocts.cn/clawops
# token 不进仓库：从环境或密钥管理里取
```

**技能和 SOP 基本不含插值**，所以现有的 `SKILL.md` 直接改名加 `.hbs`
后缀即可，内容一个字都不用动。真正需要占位符的只有 `config.toml.hbs`
（模型、密钥、端口、paired_token）和 `IDENTITY.md.hbs`／`USER.md.hbs`
（企业名、用户名）——可以直接抄 `templates/workspace/` 里现成的那份。

### 二、部署技能的动作

把现在那个「部署」技能的实现整个换成一条命令：

```bash
set -a; . ./.clawops; set +a
scripts/push-breed.sh --breed "$CLAWOPS_BREED" --dir ./breed
```

`scripts/push-breed.sh` 就在本仓库里，可以整个拷到 OpenCode 项目（它只依赖
`bash / curl / tar / sha256sum / find`，没有别的）。它做的事：

1. 检查 `--dir` 确实是模板树（有 `config.toml.hbs`）
2. **扫描密钥**——`sk-…`、`zc_…`、`enc2:`、字面量
   `api_key = "…"` 一律拒推。`{{llm.api_key}}` 这种正确写法不会误报
3. 用与服务端**完全相同**的算法算 digest（排序后的
   `路径\0文件哈希\n` 再 sha256），跟 `GET /admin/breeds/:breed` 比对，
   **一致就直接退出**——省掉一次全品种守护进程重启
4. 打包 PUT
5. `failures` 非空时以 exit 3 退出，并打印重试命令

退出码：`0` 推成功或本来就是最新、`1` 参数／校验不过、`2` 服务端拒绝、
`3` 推上去了但有租户没起来。技能层按这四个码给用户不同的话就行。

### 三、技能描述里要写清楚的三件事

1. **推送会重启该品种所有租户的守护进程**。在途的 `/chat` 会失败一次，
   SSE 会断。不是无损操作，别在高峰期推。
2. **`default` 推不动**。主线那只龙虾走 git，不走这个技能。
3. **digest 一致 = 已经是最新**。技能不该在这种情况下假装做了什么。

---

## 迁移 `tenant/zhongbolun`

那台机器（`ai.infocts.cn`）现在的形态是「main 的代码 + 分叉的模板」。
迁到品种模型上：

```bash
# 1. 服务器上：把该分支的模板打成一个品种 bundle
git -C /opt/clawops archive tenant/zhongbolun templates/workspace \
  | tar -x -C /tmp/shangji --strip-components=2

# 2. 换成 main 的二进制，配上 breeds_dir
#    （clawops.toml 加 breeds_dir + default_breed）

# 3. 装品种，先不下发
clawops install-breed --breed shangji --bundle /tmp/shangji.tar.gz --no-refresh

# 4. 把现有租户挪过去（他们此刻还绑在 default 上）
clawops list | awk '{print $1}' | while read -r oid; do
  clawops set-breed --openid "$oid" --breed shangji
done

# 5. 核对
clawops breeds
```

第 4 步之后 `tenant/zhongbolun` 分支就只剩代码差异了——那一部分里
`copy_scripts`（`scripts/` 原样拷贝）已经并进 main 作为品种机制的一部分，
其余（CORS、`allow_mock_login`、reaper `tick_secs`、唤醒时重写端口、
`requires_enterprise_name`）还需要单独合。合完那个分支就可以删了。
