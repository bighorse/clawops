# 部署到百度云 `120.48.131.72`

> **这台机器我没能碰到。** 本会话的沙箱没有 ssh 客户端，也没有任何裸 TCP 出网
> （22 / 2222 都不通，连 `github.com:22` 也不通），HTTPS 走的策略代理对
> `120.48.131.72` 直接返回 403——组织出网白名单没放这个地址。所以下面是一份
> **可直接照抄执行的手册**，不是已经跑过的记录。
>
> 手册里的脚本本身是跑过的：`scripts/deploy.sh` 在容器里用桩 `systemctl` 完整
> 走通了「装二进制 → 同步模板 → 重启 → 健康检查 → 重渲染租户」，也验证了**坏
> 二进制自动回滚**那条路径。没跑过的只有真机上的 systemd 与 nginx 部分。

---

## 0. 需要先确认的三件事

在动手之前，问清楚：

1. **这台机器是干净的，还是已经在跑别的东西？** 端口段（默认 42618–50000）
   必须避开已占用端口——`ports.rs` 会实测占用，但一开始就选对更省事。
2. **对外域名和证书。** ClawOps 只监听 `127.0.0.1:8088`，必须有 nginx 之类反
   代 + TLS。小程序要求 request 合法域名**已备案**。
3. **哪个客户/哪只龙虾。** 决定 `default_breed` 用主线那只，还是先推一个品种
   上去。

---

## 1. 预检

```bash
ssh root@120.48.131.72

cat /etc/os-release              # 需要 Ubuntu 20.04+ / CentOS 8+ / Anolis
systemctl --version | head -1    # systemd ≥ 245，CentOS 7 不支持
nproc && free -g && df -h /      # 建议 ≥ 4 核 8G；每租户 MemoryMax=512M
ss -lntp | awk '{print $4}' | grep -oE '[0-9]+$' | sort -un | tail -20
```

最后一条看的是**已占用端口**，用来决定端口段。ClawOps 每个租户占一个端口。

---

## 2. 初始化（只做一次）

```bash
mkdir -p /opt && git clone https://github.com/bighorse/clawops.git /opt/clawops
cd /opt/clawops && git checkout claude/zeroclaw-config-sync-5vydaj

bash scripts/server-bootstrap.sh
```

装系统包（含 `rsync`、`acl`、`sqlite3`）、rustup、ufw、fail2ban，建目录，装两
个 systemd unit。**幂等，可重复跑。**

> ⚠️ 脚本会把 sshd 改成禁用密码登录，但**不会自动 reload**。先把公钥放进
> `/root/.ssh/authorized_keys`，确认另开一个会话能用密钥登进来，**再**
> `systemctl reload sshd`。顺序反了就把自己关在门外了。

### 2.1 zeroclaw 二进制

```bash
export PATH=/root/.cargo/bin:$PATH
git clone https://github.com/bighorse/zeroclaw.git /opt/zeroclaw
cd /opt/zeroclaw && cargo build --release
install -m 0755 target/release/zeroclaw /usr/local/bin/zeroclaw
zeroclaw --version
```

### 2.2 共享环境变量

**必须在开第一个租户之前就存在**——provisioner 用 `setfacl` 给每个新 uid 授权
读它，而那一步**只在开户时执行一次**。文件晚建，先开的租户永远读不到。

```bash
cat > /etc/clawops/zeroclaw.env <<'EOF'
ZEROCLAW_API_KEY=sk-填真实的明文key
EOF
chmod 600 /etc/clawops/zeroclaw.env
```

> ⚠️ **key 必须是明文。** zeroclaw 里以 `enc2:` 开头的是密文，只能被生成它的那
> 份 `.secret_key` 解开，跨机器复制必然失败。而且**报错会误导**：表现为
> `zeroclaw not reachable ... after 20000ms`，看起来像网络问题，真因在解密。

### 2.3 配置

```bash
cp /opt/clawops/clawops.example.toml /etc/clawops/clawops.toml
chmod 600 /etc/clawops/clawops.toml
vi /etc/clawops/clawops.toml
```

至少要改这些：

```toml
[server]
host = "127.0.0.1"        # 绝不要 0.0.0.0，靠 nginx 反代
port = 8088

[database]
url = "sqlite:///var/lib/clawops/data/clawops.db?mode=rwc"
#                                              ^^^^^^^^^^ 少了它，首次启动报
#                                              "unable to open database file"

[zeroclaw]
binary = "/usr/local/bin/zeroclaw"
home_base = "/home"
port_range_start = 43000   # 按第 1 节实测结果调整
port_range_end   = 50000

[provisioner]
backend = "systemd"        # 不是 mock
template_dir = "/etc/clawops/templates/workspace"
breeds_dir   = "/etc/clawops/breeds"
default_breed = "default"

[zeroclaw_template]
default_provider = "deepseek"
default_model    = "deepseek-v4-pro"
api_url = "https://api.deepseek.com"   # 见下方「端点要和 key 配套」

[admin]
token = "生成一个：openssl rand -hex 32"

[wx]
backend_base_url = "https://小程序后端/…"   # 见下方「mock 登录」
```

> ⚠️ **`admin.token` 留空 = 所有 `/admin/*` 返回 503**，包括品种推送。
>
> ⚠️ **`[wx] backend_base_url` 留空 = mock 模式**，任何人都能用 `mock_openid`
> 冒充任意用户。公网开放前必须填好。
>
> ⚠️ **端点要和 key 配套。** DeepSeek 官方 key 就得指
> `https://api.deepseek.com`；指到阿里云百炼会得到 `401 invalid_api_key`。
> **换个基础模型仍然 401，只说明 key 与端点不匹配，不代表 key 坏了。**
>
> ⚠️ **计费有两处会静默失效**：①`[cost.prices]` 的 key 必须是
> `<provider>/<model>`，只写模型名匹配不上；②**别给内置 provider 配
> `api_url`**——一配，provider 名会变成 `custom:<url>`，计费 key 跟着变，同样
> 对不上。两种情况都表现为**未知模型按 0 计费、限额形同虚设，且没有任何报
> 错**。判别方法：看租户的 `workspace/state/costs.jsonl` 有没有生成。

---

## 3. 部署

```bash
bash /opt/clawops/scripts/deploy.sh --ref claude/zeroclaw-config-sync-5vydaj
```

它做的事，按顺序：

1. 检查工作区干净（**有未提交改动就拒绝**——服务器上直接改而没提交的情况发生
   过，且那是唯一一份），checkout 指定 ref
2. `cargo build --release`。**不走管道**——管道会吃掉退出码，让失败的构建看起
   来是成功的
3. 备份数据库到 `/var/backups/clawops/`（有 `sqlite3` 就用 WAL-safe 的
   `.backup`），备份旧二进制到 `clawops.old`
4. **原子换二进制**：同目录 `cp` 到临时名再 `mv`（换 inode）。正在运行的可执
   行文件不能直接覆盖，会报 `Text file busy`
5. 同步模板，`systemctl restart clawops`，轮询 `/health` 最多 30 秒
6. **健康检查不过就自动回滚**到旧二进制并重启，退出码 1
7. 重渲染所有租户工作区

`PATH` 里已经带上 `/root/.cargo/bin`——非交互 ssh 不加载 profile，`cargo` 不在
默认 PATH 里，这是 `ssh root@host 'deploy.sh'` 最常见的失败方式。

> ⚠️ **首次构建 5–10 分钟，中途可能断连。** 把「构建」和「停服务换文件」分成
> 两个 ssh 会话，或者用 `tmux`。

---

## 4. 验证

```bash
systemctl status clawops --no-pager
curl -s http://127.0.0.1:8088/health

TOKEN=$(grep -E '^token' /etc/clawops/clawops.toml | sed 's/.*"\(.*\)"/\1/')
curl -s -H "X-Admin-Token: $TOKEN" http://127.0.0.1:8088/admin/breeds | jq

# 开一个测试租户，走完整条链路
clawops --config /etc/clawops/clawops.toml provision \
  --openid smoke001 --display-name "冒烟测试"
clawops --config /etc/clawops/clawops.toml list
sudo -u claw-001 XDG_RUNTIME_DIR=/run/user/$(id -u claw-001) \
  systemctl --user status zeroclaw@claw-001 --no-pager
```

租户守护进程起不来时，按这个顺序查：

```bash
sudo -u claw-001 XDG_RUNTIME_DIR=/run/user/$(id -u claw-001) \
  journalctl --user -u zeroclaw@claw-001 -n 50 --no-pager
cat /home/claw-001/.zeroclaw/config.toml        # 渲染对了吗
sudo -u claw-001 cat /etc/clawops/zeroclaw.env  # setfacl 生效了吗
```

> ⚠️ **`setfacl` 依赖 `acl` 包**。没装的话 provisioner 只 warn 不报错，凭据静
> 默注入失败。`server-bootstrap.sh` 会装，但如果这台机器是别人装好的，自己确
> 认一遍。

---

## 5. nginx 反代

```nginx
location /clawops/ {
    proxy_pass http://127.0.0.1:8088/;
    proxy_http_version 1.1;
    proxy_set_header Host              $host;
    proxy_set_header X-Real-IP         $remote_addr;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;

    # SSE：这三条缺一不可，否则 /events 会被缓冲住，前端看起来像没数据
    proxy_buffering off;
    proxy_cache off;
    proxy_read_timeout 3600s;
}
```

`X-Forwarded-For` 是必须的：ClawOps 的限流按它取源 IP，缺了会把所有请求算作同
一个 IP，`wx-login` 10/分钟的限流会瞬间打满。

> ⚠️ **`sites-enabled/` 里不要放备份文件**。include 用的是通配符，`.bak` 也会
> 被当配置加载，报 `conflicting server name`。备份放 `/root/nginx-backups/`。
>
> ⚠️ **在服务器上 curl 自己的公网域名测不出真实情况**——源 IP 会命中 `allow`
> 规则。必须从外部测。

公网开放前先用 `allow/deny` 关着，等第 6 节的清单全绿再放开。

---

## 6. 公网开放前的检查清单（顺序不可颠倒）

1. ⬜ `[wx] backend_base_url` 填好，重启后启动日志里**没有** mock 告警
2. ⬜ `[admin] token` 是随机值，且**没有**出现在任何仓库、日志、聊天记录里
3. ⬜ 企微网关的认证按 [`wecom-gateway-auth.md`](wecom-gateway-auth.md) 收敛
4. ⬜ 小程序后台配好 request 合法域名（已备案）
5. ⬜ TLS 证书有效期确认
6. ⬜ 从**外部网络**测一次完整链路：登录 → `/chat` → `/events`
7. ⬜ **最后**才移除 nginx 的 `allow/deny` 段

顺序写死是有原因的：先拆 `deny all` 再补 mock 登录，中间那段窗口任何人都能冒
充任意用户。

---

## 7. 日常操作

```bash
# 推一个品种上来（从开发端跑，见 opencode-deploy-skill.md）
scripts/push-breed.sh --breed <name> --dir ./breed

# 改了主线模板后下发
bash /opt/clawops/scripts/deploy.sh --ref main

# 只重渲染某个品种
clawops --config /etc/clawops/clawops.toml refresh-breed --breed <name>

# 看现状
clawops --config /etc/clawops/clawops.toml breeds
clawops --config /etc/clawops/clawops.toml list
journalctl -u clawops -f
```

> ⚠️ **任何重启都会打断在途请求和 SSE。** 浏览器端会看到一串
> `ERR_CONNECTION_CLOSED`，此时正在发的 `/chat` 会静默失败（前端只是回到可输
> 入状态，不报错）。重启后**刷新页面再测**，别以为是功能坏了。

---

## 8. 回滚

`deploy.sh` 健康检查不过会自动回滚。手工回滚：

```bash
install -m 0755 /usr/local/bin/clawops.old /usr/local/bin/.clawops.new
mv /usr/local/bin/.clawops.new /usr/local/bin/clawops
systemctl restart clawops
```

数据库回滚（**会丢掉这期间新开的租户**，慎用）：

```bash
systemctl stop clawops
cp /var/backups/clawops/clawops-<时间戳>.db /var/lib/clawops/data/clawops.db
systemctl start clawops
```

> 迁移是**只增不减**的（`0010_user_breed.sql` 只做 `ADD COLUMN … DEFAULT`）。
> 回滚二进制**不需要**回滚数据库：旧二进制看不见 `breed` 列，照常工作。
