# ClawOps

ZeroClaw 多租户运维网关。小程序端通过 ClawOps 统一入口鉴权、转发消息到每用户专属的 `zeroclaw daemon` 进程。

> 阶段:**Phase 2 完成**(2026-04-25) — config 模板补全 + zeroclaw 1.4.0 真实解析通过、自动生成 paired_token 注入 gateway、SSE 字节流代理、微信 code2session 登录 + Bearer 鉴权。Reaper / 监控 / 横向分片留给 Phase 3。

## 架构

```
小程序 ──HTTPS──▶ ClawOps (:8088) ──HTTP──▶ zeroclaw@<uid> (127.0.0.1:4261x)
                     │
                     ├─ SQLite (users / port_allocations / provision_log)
                     └─ ProcessManager (mock / systemd)
```

每用户对应一个 Linux uid、一个 zeroclaw daemon、一个 `/home/claw-NNN/.zeroclaw/` 目录。ClawOps 不进入 zeroclaw 进程内部,只做路由 + 生命周期管理。

## 品种(breeds)

一台 ClawOps 可以同时跑多种龙虾。每个品种是一棵独立的模板树,`users.breed`
决定某个租户从哪一棵渲染——区分点在数据行上,不在 git 分支上。

```bash
# 开发端:一条命令推上去,只重启该品种的租户
scripts/push-breed.sh --breed shangji --dir ./breed

# 服务器上
clawops breeds                                  # 品种 + digest + 租户数
clawops set-breed --openid o_xxx --breed shangji
```

`provisioner.breeds_dir` 不填即单品种模式,行为与引入品种前完全一致。
接口契约、开发端部署技能的改法、以及 `tenant/zhongbolun` 的迁移步骤见
[`docs/breed-sync.md`](docs/breed-sync.md)。

## 开发环境(macOS / Linux 无 root)

```bash
cargo build
mkdir -p /tmp/clawops-dev && cd /tmp/clawops-dev
cp /Users/mario/Code/clawops/clawops.example.toml clawops.toml
# 编辑 clawops.toml 中的路径和 template_dir,然后:

clawops --config clawops.toml provision \
  --openid test001 --phone 13800138000 --display-name "张三" \
  --enterprise-profile ./profile.json

clawops --config clawops.toml serve
# 另一个 shell:
curl http://127.0.0.1:8088/health
curl -X POST http://127.0.0.1:8088/chat \
  -H 'Content-Type: application/json' \
  -d '{"openid":"test001","content":"你好"}'
```

mock backend 下 `/chat` 返回固定 echo,不会真的拉起 zeroclaw。

## HTTP API

小程序面向接口(需 `Authorization: Bearer <session_token>`):

| Method | Path                | 说明 |
|--------|---------------------|------|
| POST   | `/auth/wx-login`    | 微信 `code2session`,首次自动 provision,返回 30 天 token |
| POST   | `/chat`             | 转发用户消息到其 zeroclaw `/webhook`,token 反解 openid |
| GET    | `/events`           | SSE 字节流代理上游 `/api/events`(支持 `?token=` query 兜底) |
| GET    | `/health`           | 健康检查(无需鉴权) |

运维接口(目前无鉴权,仅靠 127.0.0.1 防护;生产必须加 admin token):

| Method | Path                      | 说明 |
|--------|---------------------------|------|
| GET    | `/admin/users`            | 用户列表 |
| GET    | `/admin/users/:openid`    | 单用户详情 |
| POST   | `/admin/provision`        | 手动新建(不走微信) |
| POST   | `/admin/stop/:openid`     | 停止用户 zeroclaw,释放端口 |
| GET    | `/admin/breeds`           | 列出品种 + 模板树 digest + 租户数 |
| GET    | `/admin/breeds/:breed`    | 单品种详情 + `路径 -> sha256` 清单 |
| PUT    | `/admin/breeds/:breed`    | 推送品种 bundle(tar/tar.gz),原子换入并下发 |
| DELETE | `/admin/breeds/:breed`    | 删除品种(仍有租户则 409) |
| POST   | `/admin/breeds/:breed/refresh` | 只重渲染该品种的租户 |
| PUT    | `/admin/users/:openid/breed`   | 给租户换品种并立即重渲染 |

### wx-login 请求格式

```jsonc
// 真实微信:小程序拿 wx.login() 返回的 code
{ "code": "0a3...", "phone": "+8613xxx", "display_name": "王某",
  "enterprise_profile": { "company_name": "...", "industry": "..." } }

// 开发 mock(wx.appid 为空时):用 mock_openid 直连
{ "code": "anything", "mock_openid": "o_demo_user_a" }
```

## 生产部署(Linux + systemd)

```bash
git clone https://github.com/bighorse/clawops.git /opt/clawops
bash /opt/clawops/scripts/server-bootstrap.sh   # 系统包 + 目录 + systemd unit
vi /etc/clawops/clawops.toml                    # 照 clawops.example.toml 改
bash /opt/clawops/scripts/deploy.sh --ref main  # 构建、原子换二进制、重启、下发
```

`deploy.sh` 幂等,健康检查不过会**自动回滚到上一个二进制**。逐步骤说明、
配置样例、以及一份「公网开放前的检查清单」见
[`docs/deploy-baidu.md`](docs/deploy-baidu.md)。

要求:Ubuntu 20.04+ 或 CentOS 8+ / Anolis OS(systemd ≥ 245),CentOS 7 不支持。
ClawOps 以 root 运行(要 `useradd` / `loginctl` / `systemctl --user`)。

## 对接文档

| 文档 | 给谁看 |
|---|---|
| [`docs/breed-sync.md`](docs/breed-sync.md) | 品种机制的接口契约 |
| [`docs/opencode-deploy-skill.md`](docs/opencode-deploy-skill.md) | OpenCode 端:部署技能怎么改 |
| [`docs/wecom-gateway-auth.md`](docs/wecom-gateway-auth.md) | 企微网关端:认证怎么收敛 |
| [`docs/deploy-baidu.md`](docs/deploy-baidu.md) | 运维:上新机器的完整手册 |

## 后续(未实施)

- Phase 2: SSE `/events` 聚合 + Reaper 定时清理(90 天无活跃停进程)
- Phase 3: Prometheus `/metrics` + 每日 rsync 备份 + 横向分片
- 真 systemd unit file 的完整 `zeroclaw@.service` 模板
- `/pair` 流程(目前 Phase 1 config 下 `require_pairing = false`)
- 微信 code2session 登录
- paired_token 加密存储(目前明文)
