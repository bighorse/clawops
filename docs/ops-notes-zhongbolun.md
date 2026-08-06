# 运维备忘 · 熵玑参谋实例（内部）

**内部文档，不对外发布。** 对小程序团队交付的是 `shangji-miniprogram-integration.md`，那份里不含服务器地址、路径、分支、内部数据源等信息。

---

## 落地位置

| 项 | 位置/值 |
|---|---|
| 服务器 | `root@47.94.58.57` |
| 源码 | `/opt/clawops`，分支 **`tenant/zhongbolun`** |
| 二进制 | `/usr/local/bin/clawops`（回滚：`clawops.old`） |
| 配置 | `/etc/clawops/clawops.toml`（600） |
| 模板 | `/etc/clawops/templates/workspace/` |
| 数据库 | `/var/lib/clawops/data/clawops.db` |
| 服务 | `systemctl {status,restart} clawops` |
| 监听 | `127.0.0.1:8088`，经 nginx 反代到 `https://ai.infocts.cn/clawops/` |
| 租户工作区 | `/home/claw-NNN/.zeroclaw/workspace/` |
| 租户端口段 | **43000–50000**（避开本机已占的 41101/41102/42618/42619） |
| 共享环境变量 | `/etc/clawops/zeroclaw.env`（MySQL 凭据，经 systemd 注入所有租户） |
| 模型 | DeepSeek 官方 `deepseek-v4-pro`，`api_url = https://api.deepseek.com` |

**企业详情页模板（待小程序提供）**：拿到后写进 `/etc/clawops/zeroclaw.env`：
```
ENTERPRISE_DETAIL_URL_TEMPLATE=/pages/enterprise/detail?name={name}
```
然后 `systemctl restart clawops` + `refresh-all-workspaces`。已在 `shell_env_passthrough`
白名单里，`search_enterprise.py` 会自己做 URL 编码。未配置时检索结果不带链接（不会输出坏链接）。
企业表无主键，只能按名称跳转，故占位符只有 `{name}`。

**改模板后下发到已有租户**：
```
curl -X POST http://127.0.0.1:8088/admin/refresh-all-workspaces -H "X-Admin-Token: <token>"
```

---

## 踩过的坑（都是实测代价换来的）

**⚠️ 分支纪律**：本实例固定 `tenant/zhongbolun`。**不要切到 main，也不要把本分支的模板合并出去**——模板是本实例专属的，合出去会覆盖别的实例。

**⚠️ 密钥不可跨实例复制**：zeroclaw 的 `api_key` 若以 `enc2:` 开头即为密文，只能被生成它的那份 `.secret_key` 解开。配置里**必须填明文**，否则租户守护进程启动即失败，且报错会误导成 `zeroclaw not reachable ... after 20000ms`（真因在解密，不在网络）。

**⚠️ 模型端点要和 key 配套**：这套用的是 DeepSeek **官方** key，`api_url` 必须指向 `https://api.deepseek.com`。指向阿里云百炼会得到 `401 invalid_api_key`——**换个基础模型仍然 401 只能说明 key 与端点不匹配，不代表 key 失效**，别据此断定 key 坏了。

**⚠️ 环境变量不会自动传给脚本**：守护进程的环境变量要出现在 `shell_env_passthrough` 白名单里，shell 子进程才拿得到。`MYSQL_*` 漏掉时表现为**静默降级**——SOP 照跑不报错，只是内部数据那块是空的。验证时**不能只看 `/proc/<pid>/environ`**（那里是有的），必须验到脚本实际运行的子进程。

**⚠️ `setfacl` 依赖 acl 包**：provisioner 给租户授权读 `zeroclaw.env` 用的是 `setfacl`，包没装时只 warn 不报错，于是凭据静默注入失败。且该授权**只在开户时执行**，所以 `zeroclaw.env` 必须在开户**之前**就存在。

**⚠️ nginx 两份配置会漂移**：`/etc/nginx/sites-enabled/ai` 才是生效的那份，`sites-available/ai` 是另一个独立文件且已不同步。改错文件不会报错，只是不生效。另外**切勿把备份放进 `sites-enabled/`**——include 用的是通配符，会把 `.bak` 也当配置加载，报 `conflicting server name`。备份放 `/root/nginx-backups/`。

**⚠️ 自测公网可达性不可靠**：在服务器上 curl 自己的公网域名，源 IP 会命中 `allow` 规则，测不出真实的外部访问情况。必须从外部测。

---

## 开放公网前的检查清单（顺序不可颠倒）

1. ⬜ 小程序后端 code2session 接口就绪并联调通过
2. ⬜ `[wx] backend_base_url` 填好；**删除 `allow_mock_login`**；重启确认启动日志里没有 mock 告警
   - 为空即 mock 模式，任何人可用 `mock_openid` 冒充任意用户。网关有守卫会拒绝启动，除非显式承认。
3. ⬜ **产物读取改为经由租户身份** —— 见下节，这条是有意留的
4. ⬜ 微信小程序后台配置 request 合法域名（`https://ai.infocts.cn`，须已备案）
5. ⬜ TLS 证书有效期确认（Let's Encrypt，当前至 2026-10-30）
6. ⬜ **最后**才移除 nginx 的 `allow/deny` 段

**顺序写死是有原因的**：先拆 `deny all` 再补 mock，中间那段窗口任何人都能冒充任意用户。

---

## 待办：产物读取改由租户身份提供

`/me/artifacts` 目前是**网关以 root 直读租户目录**。租户拥有自己的 home，能在目录里做手脚（符号链接、硬链接、替换中间目录、趁检查完再偷换），所以网关必须逐一校验路径。已发现并修复五类问题（越权、TOCTOU、硬链接、栈溢出、OOM），全部实测挡住。

但这是**枚举坏情况**的思路，永远可能漏。根治是**只允许好情况**：让 zeroclaw（本就以租户 uid 运行）暴露一个只读产物端点，网关只做鉴权转发，一个字节的租户目录都不碰——越权由内核的账号隔离直接拒绝，不需要网关写任何路径校验。

**决定的时机：不现在做，但必须在开放公网之前做。** 当前公网关闭、只有测试租户，风险可控；一旦对外，攻击者就不再只是自己人。计划在小程序联调完、准备上线时连同放开公网一起做。

详见记忆 `clawops-artifacts-endpoint-security`。

---

## 服务端硬约束（与对外文档保持一致）

| 约束 | 值 |
|---|---|
| 简报单份大小 | 4 MB |
| 目录递归深度 | 8 层 |
| 列表条目上限 | 500 条 |
| 扩展名 | 仅 `.md` |
| session TTL | 30 天，不滑动续期 |
| `/chat` 下游超时 | 900 秒 |
| 限流 | wx-login 10/分钟/IP；chat 30/分钟/用户；`/me/*` 无限流 |
