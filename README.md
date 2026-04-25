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

### wx-login 请求格式

```jsonc
// 真实微信:小程序拿 wx.login() 返回的 code
{ "code": "0a3...", "phone": "+8613xxx", "display_name": "王某",
  "enterprise_profile": { "company_name": "...", "industry": "..." } }

// 开发 mock(wx.appid 为空时):用 mock_openid 直连
{ "code": "anything", "mock_openid": "o_demo_user_a" }
```

## 生产部署(Linux + systemd)

1. 确认 OS 和 systemd 版本:`cat /etc/os-release && systemctl --version | head -1` —— 要求 Ubuntu 20.04+ 或 CentOS 8+ / Anolis OS(systemd ≥ 245)。CentOS 7 不支持。
2. 安装 zeroclaw binary 到 `/usr/local/bin/zeroclaw`。
3. 把 `systemd/zeroclaw@.service` 放到 `/etc/systemd/user/`。
4. ClawOps 以 root 或具备 `useradd/loginctl/systemctl` NOPASSWD sudo 权限的用户运行。
5. `clawops.toml` 中 `provisioner.backend = "systemd"`。

## 后续(未实施)

- Phase 2: SSE `/events` 聚合 + Reaper 定时清理(90 天无活跃停进程)
- Phase 3: Prometheus `/metrics` + 每日 rsync 备份 + 横向分片
- 真 systemd unit file 的完整 `zeroclaw@.service` 模板
- `/pair` 流程(目前 Phase 1 config 下 `require_pairing = false`)
- 微信 code2session 登录
- paired_token 加密存储(目前明文)
