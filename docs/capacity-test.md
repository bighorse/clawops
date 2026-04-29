# ClawOps 16GB 单机容量测试报告

> 测试日期: 2026-04-29
> 服务器: 百度云 1 台,16 GB RAM / 16 vCPU / Ubuntu 20.04 / SSD
> 部署: 单机 nginx + ClawOps + per-user zeroclaw daemon
> LLM: qwen3.6-flash via DashScope(HTTP 直连)

## TL;DR(给运营 / PM 看的)

| 用户规模 | 16GB 单机能否承载 | 备注 |
|---------|-----------------|------|
| 0-500 注册 | ✅ 当前配置轻松 | qwen-flash 免费层 RPM 够用 |
| 500-1000 注册 | ⚠️ 内存接近上限 | 或升 qwen 付费层提 RPM |
| 1000+ 注册 | ❌ 必须扩容 | 升内存到 32GB 或 → 多机分片 |

**真实瓶颈**:不是内存,是 **qwen DashScope RPM**(免费层 ~60/min,付费层 ~300/min)。
**同时并发 chat 上限**:实测 K=30 简单 chat / K=10 复杂 chat 全部 200,内存只占用 8%。

## 测试方法

三个脚本,服务器 root 跑(走 127.0.0.1 跳 GFW 抖动):

| 脚本 | 用途 | 钱 |
|------|------|---|
| `scripts/stress_idle.sh` | provision N 用户,记录每 daemon RSS + 系统内存 | 0 元(不调 LLM) |
| `scripts/stress_concurrent.sh` | K 个用户同时发 chat,记录 RT 分布 | ¥0.5-2 |
| `scripts/stress_cleanup.sh` | 清理 stress_* 用户 | 0 元 |

复测命令(以后回归用):

```bash
cd /opt/clawops && git pull
bash scripts/stress_idle.sh                                  # 阶段 1
K=10 bash scripts/stress_concurrent.sh                       # 阶段 2 简单
PROMPT="我想找商标注册服务" K=10 bash scripts/stress_concurrent.sh   # 阶段 3 复杂
bash scripts/stress_cleanup.sh                               # 收尾
```

## 阶段 1 — Idle 容量(provisioning 76 个 daemon)

线性曲线,每 +5 用户 +52~55 MB,每 daemon **首次启动 RSS = 11 MB**。

```
users  mem_used_mb  mem_avail_mb  daemons  daemon_avg_rss_mb  load_1m
  0       700         15287          1            19           0.00
 25       964         15023         26            11           0.24
 50      1226         14761         51            11           1.90
 75      1499         14488         76            11           1.99
```

注:跑到 75 时撞了 ClawOps 自身的 admin rate limit(60/min),不是内存上限。脚本已修(改用 `clawops provision` CLI 绕过 HTTP 限流)。

## 阶段 2 — K=10 简单并发 chat("你好")

| 指标 | 值 |
|------|---|
| HTTP | 10/10 = 200 |
| p50 / p95 / max | 1.65s / 2.39s / 2.39s |
| wall clock | 2.39s ≈ max(真并行) |
| 内存增量 | +31 MB(每活跃 +3 MB) |

## 阶段 2 — K=30 简单并发 chat

| 指标 | 值 |
|------|---|
| HTTP | 30/30 = 200 |
| p50 / p95 / max | 2.10s / 2.99s / 3.44s |
| wall clock | 3.46s ≈ max(线性扩展) |
| 内存增量 | +67 MB(每活跃 +2.2 MB) |

K 从 10 → 30(3x),p50 才 +27% —— qwen 没限流,服务器没瓶颈。

## 阶段 3 — K=10 复杂并发 chat("我想找商标注册服务",触发 commodity API + 多轮 tool)

| 指标 | 值 |
|------|---|
| HTTP | 10/10 = 200 |
| p50 / p95 / max | 9.92s / 12.3s / 12.3s |
| wall clock | 12.3s ≈ max(真并行) |
| 内存增量 | +33 MB(每活跃 +3 MB) |
| Response 大小 | 1100-1200 bytes(LLM 真在调 API 列产品) |

## 关键发现:idle RSS 修正

**初次跑出 11 MB/idle daemon,但**这是"刚启动从未 chat 过"的最低值。daemon 一旦处理过 chat,heap 就涨,空闲后**不释放**。

阶段 3 跑完后看 baseline 内存:75 个 daemon 已用过 chat → 净占用 ~1735 MB → **23 MB/daemon**,**不是 11 MB**。

**真实生产容量按 23 MB/idle daemon 算**:

```
可用预算:14 GB(16GB - 700MB 系统 - 1500MB 安全余量)
14 GB / 23 MB = ~620 个 idle daemon
留 30% 应对峰值活跃 spike → 推荐 ~430 个
```

## 真实瓶颈分析

按瓶颈优先级:

1. **qwen DashScope RPM**(最先撞)
   - 免费层 ~60 RPM,复杂 chat 每次内部 2-3 次 LLM 调用
   - **同时并发 chat ≤ 20-30 个**(免费层),超过会 429
   - 付费层 ~300 RPM:同时并发 ≤ 100-150
2. **服务器内存**(第二瓶颈)
   - 每 idle daemon ~23 MB,峰值活跃 ~50-100 MB
   - 假设 5% 同时活跃:1000 注册 = 50 活跃 × 100 + 950 idle × 23 ≈ 27 GB(会爆)
   - **实际承载 = qwen RPM 与内存的最小值**
3. **CPU / 网络 / 磁盘**(没碰到)
   - 75 daemon load 1.99,16 vCPU 完全过剩
   - chat 期间网络 < 10 Kbps,可忽略
4. **第三方 API**(commodity / tavily)
   - 没观察到限流,但**自家 commodity API 是 GET 阻塞**,大量并发可能拖

## 运营建议(分阶段)

### 阶段 A:0-300 注册用户

- **当前 16GB 单机够用**
- qwen-flash 免费层够,不用付费
- 不需要任何架构变化
- 建议:Reaper 90 天清理打开(已默认开),让冷用户释放内存

### 阶段 B:300-800 注册用户

- 16GB 接近上限,但**仍可单机**
- 建议:**升 qwen 付费层**,RPM 从 60 → 300,放开活跃天花板
- 监控:`ss -lntp | grep zeroclaw | wc -l` 看 daemon 数,`free -m` 看 mem_avail
- 阈值:mem_avail < 3000 MB 时报警

### 阶段 C:800-1500 注册用户

- 单机 16GB 不够
- 选项 1:**升 32GB 单机**(最简,~2 倍单机成本)
- 选项 2:**横向扩第二台 16GB**(需要 ClawOps 支持 user→shard 路由,目前**没有**,Phase 7 的事)

### 阶段 D:1500+ 注册

- 必须横向多机
- 增加缓存层(redis 共享 sessions / chat-history)
- 考虑共享 LLM RPM 池(代理层缓存重复请求)

## 已知风险点(未来某天会咬人)

1. **23 MB idle 是 chat 用过一次后的水位线**。**反复多轮长 chat** 后 daemon RSS 会再涨,无上限的话最终撞 systemd `MemoryMax=512M` 被 OOM killer 杀。Reaper 会兜底回收 90 天 idle 的,但活跃用户的累积没人收。Phase 7 可能需要"daemon 内存阈值重启" 兜底。
2. **qwen 单 key 跨用户共享**:目前所有 daemon 共用一个 ZEROCLAW_API_KEY,API 调用账单聚合在一处,**单用户难限**。当前 ClawOps 已设 `[autonomy] max_cost_per_day_cents = 500`(每用户 / 每天 ~¥3.6),理论上够用,但需要监控 `/api/cost`。
3. **chat_messages 表无限增长**。每次 chat 写 2 行,1000 用户 × 100 turns = 200K 行,SQLite 还行;但若有用户聊上万 turn,需要 trim。

## 复测建议周期

- **每次重大版本上线**:跑 K=10 简单 + K=10 复杂,对比 RT 是否劣化
- **用户量翻倍**:跑完整 idle 测 + cleanup
- **qwen 升付费 / 换 provider**:重测 K=30 / K=50 / K=100 看新 RPM 限

## 测试环境快照

```
$ uname -a
Linux instance-mf91gba0 5.4.0-216-generic #236-Ubuntu SMP

$ free -m  (baseline)
              total        used        free      shared  buff/cache   available
Mem:          16022         700        8772           1        6549       15287

$ rustc --version
rustc 1.95.0

$ /usr/local/bin/zeroclaw --version
zeroclaw 1.4.0

$ /usr/local/bin/clawops --help | head -1
ZeroClaw multi-tenant ops gateway
```

`config.toml` 关键参数(影响容量):
- `[autonomy] max_memory_mb = 512`(每 daemon 硬上限)
- `[autonomy] max_cost_per_day_cents = 500`(每用户每日 LLM 预算)
- `[gateway]` host=127.0.0.1, paired_token 长度 67 char (`zc_<64hex>`)
- `[reaper] idle_stop_minutes = 129600`(90 天)
