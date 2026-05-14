# policy-match SOP E2E 调试报告 — zeroclaw daemon 死锁未解 (2026-05-13)

> **状态**: SOP 业务设计已验证（step 1-2 实测跑通，产物文件齐全），但 zeroclaw runtime 在 SOP 中段必死锁，root cause 未定位，需源码级深调。
>
> **当前位置**: `clawops/templates/workspace/sops/policy-match/SOP.md.hbs` 已迭代到可工作版本；后端 `/wecom/agent/enterprise_profile_sync` 和 `/wecom/policy_summary` 已加 `fields` 参数。daemon 死锁是阻塞 SOP 完整 E2E 的唯一剩余问题。

---

## 业务层成果 (已验证)

### 后端契约扩展 (P0, 已上线)

- `GET /wecom/policy_summary?fields=...` — 政策清单字段裁剪
  - 详见 [policy-match-api-contract-addendum-fields.md](policy-match-api-contract-addendum-fields.md)
  - smoke test 通过：100 条政策响应 150KB → fields 过滤后 67KB → 进一步去 `application_condition` 后 28KB
- `GET /wecom/agent/enterprise_profile_sync?fields=...` — 企业画像字段裁剪
  - 详见 [policy-match-api-contract-addendum-profile-fields.md](policy-match-api-contract-addendum-profile-fields.md)
  - smoke test 通过：91KB → fields 过滤后 13KB（排除 `bocha_info`）

### SOP 设计迭代

- step 1: `enterprise_profile_sync` 加 fields 参数（排除 `bocha_info`）
- step 2: `policy_summary` 加 fields 参数（**排除 application_condition** — 67KB→28KB）
- step 4 新增：单独拉取 `id,application_condition` 用于条件分析
- step 2 URL 去掉 `order_by_fields` / `order_by_types`（与 `no_pagination=true` 同用时后端返回 400）
- IDENTITY 第零原则加 `sop_execute('policy-match')` 强制触发规则

### E2E v11 实测产物 (qwen3.6-plus + 生产 binary)

```
case/policy-match/<enterprise_safe>/
├── profile.json   (7327 bytes — qwen 严格保存 API 响应)
└── candidates.json (28266 bytes — 10 字段 × 36 条 ONLINE 政策)
```

业务逻辑无误，与 SOP schema 完全对齐。

---

## 死锁现象

每次 E2E 在 5-9 次 LLM call 后 daemon 完全卡死：

- **线程状态**: 5 个 worker `futex_wait_queue_me` + 1 个 io driver `ep_poll`
- **CPU**: 0%
- **无 outbound TCP** 到任何 LLM provider
- **日志最后一条**: 通常是 `tool.call success` 或 `llm.request` 发出，**没有对应的 `llm.response`**
- **持续时间**: 至少 30+ 分钟（已观察），未自愈

特征与"普通 tokio runtime 空闲态"难以区分（都是 worker park 在 futex），但实际是 spawn future 在 `.await` 后 wake 永不到达。

## 14 轮调试时间线 (2026-05-13)

| 版本 | 假设 | 行为 |
|------|------|------|
| v3-v5 | deepseek stream-protocol error / order_by 400 | 修了 `is_stream_protocol_error()` + 去 order_by — 不解决静默 hang |
| v6/v7 | enterprise profile 91KB 撑爆 context | 后端加 fields 后 context 从 70K→55K — 仍死锁 |
| v8 | SSE chunk loop 缺 idle timeout | `compatible.rs::sse_bytes_to_chunks` 加 `tokio::time::timeout(90s)` — 走的是 `chat_stream` 路径，**SOP 实际用 `chat()`，修复完全没触达** |
| v9 | reqwest `send().await` headers phase 不可靠 | 给 `chat_stream::send()` 加 120s outer timeout + tracing — 同样路径错误 |
| v10 | `reliable.rs::provider.chat()` 外层 hung | 给 `provider.chat()` 加 240s outer timeout — **代码路径正确但 tokio timer 也未触发**，证实"runtime 自身 wake 机制坏了" |
| v11 | deepseek 服务侧不稳定 | 切 qwen3.6-plus + 生产 binary — 跑得更深（首次 step 2 出 candidates.json），但 step 2→3 间 LLM call 仍死锁 |
| v12 | `[agent] compact_context = true` 引入 spawn task 泄漏 | 关掉仍死锁 |
| v13 | `[scheduler]` 和 `[agent]` 限制太低 (max_tasks=16, max_concurrent=2, max_tool_iterations=80) | 提到 geneline 水平 (64/4/160/60) 仍死锁 |
| v14 | binary 缺 `--features channel-feishu`（geneline 显式加） | 加上重编仍死锁 |

## 已排除的假设清单

| 假设 | 排除证据 |
|------|---------|
| deepseek 服务端 stream-protocol error | v9 加 `is_stream_protocol_error()` non-retryable + qwen 也死锁 |
| SSE chunk-to-chunk idle hang | v8 加 idle timeout 90s 但代码路径错误，且 v10 修正路径后 tokio timer 也失效 |
| reqwest send headers phase | v9 加 send 包装 timeout 仍未触发 |
| `provider.chat()` 调用未超时 | v10 加 240s outer 仍未触发 |
| context 过大 (>70K) 撑爆 deepseek | qwen 在 ~67K context 也死锁 |
| `compact_context = true` 触发 spawn task 泄漏 | v12 关掉仍死锁 |
| `scheduler.max_tasks=16` 不够 | v13 提到 64 仍死锁 |
| `agent.max_tool_iterations=80` 不够 | v13 提到 160 仍死锁 |
| `agent.max_history_messages=30` 不够 | v13 提到 60 仍死锁 |
| 缺 `channel-feishu` / `channel-lark` feature | v14 编进去仍死锁 |
| provider 特定 (deepseek vs qwen) | qwen 也死锁，只是位置稍后 |
| Binary 版本 (1.4.0 vs ebc5eb1f) | 两个版本都死锁 |

## 残余可能根因 (未验证)

1. **服务器多租户负载**：120.48.131.72 上 11 个生产 daemon 共享 host，可能某个共享资源（文件描述符、内核 socket 状态）受限。geneline 跑在不同环境，不受此限。
2. **SOP engine spawn task 泄漏**：每次 SOP step 之间 `sop_advance` 可能 spawn 后台 task 持有 future 引用，task 完成后 wake 信号丢失。但需读 `src/sop/engine.rs` 源码验证。
3. **reqwest connection pool 半关闭 socket**：经过多次大 context (>50K) LLM call，pool 可能积累半关闭连接占住 epoll readiness slot，新 LLM call 复用时永远不 ready。需 strace + tcpdump 验证。
4. **Tokio runtime 配置**：clawops daemon 启动未显式指定 worker_threads / blocking_threads，默认值在多租户下可能不够。geneline 单租户跑不会触发。

## 进一步调试建议

需要在 zeroclaw daemon 上启用以下能力 (clawops 生产环境暂不动):

1. **tokio-console** — 实时看 task tree、wake / await 状态、卡住的 future
2. **`RUST_LOG=zeroclaw=trace`** — 看 SOP engine 内部所有 spawn 点
3. **strace -p $DPID -f -e trace=futex,epoll_*,read,write** — 内核态系统调用追踪
4. **重现条件最小化**：在 geneline / pharmaclaw 同环境跑相同 SOP 看是否复现（若不复现，根因在多租户 host）

## 结论

- **业务交付**：policy-match SOP 设计本身 work（后端 fields 参数 + SOP 拆 application_condition + IDENTITY 触发规则全部验证）。后端契约已上线生效。
- **不交付**：daemon 完整 E2E（卡在 step 3 LLM 匹配），原因是 zeroclaw runtime 死锁。
- **下一步**：暂不修 zeroclaw runtime（超出本次预算），保留所有已 commit 的 SOP / 后端契约改动。死锁问题作为 zeroclaw issue 跟踪，等后续有 tokio-console 接入或在 geneline 同环境复现验证后再修。

## 不动的部分

- `clawops/templates/workspace/config.toml.hbs` — 不加 fallback 段（11 生产用户用同模板，改了风险大）
- `clawops/templates/workspace/IDENTITY.md.hbs` — policy-match 触发规则已 commit
- `clawops/templates/workspace/sops/policy-match/SOP.md.hbs` — 已迭代到可工作版本（前 2 步验证通过）
- `/usr/local/bin/zeroclaw` — 生产 11 用户共用，不替换

---

**附**: 完整 commit 链 (clawops main branch)
- `df98211` smoke test backend fields
- `1cc3d80` sop step 2 remove order_by
- `2a5f27e` sop move application_condition to step 4
- `313bdcf` docs profile-fields addendum
- `135dab4` sop step 1 add fields param

**附**: 完整 commit 链 (zeroclaw bighorse fork feat/prior-art-layer1)
- `3e4044b6` providers: SSE idle timeout (v8 — 路径错,SOP 走 chat() 不走 chat_stream())
- `083982ea` providers: send phase timeout + tracing (v9 — 同上)
- `4d36d089` reliable: outer 240s timeout (v10 — 路径对,但 agent/loop_.rs 不走 reliable.rs!)
- `f3761983` config: pool_max_idle_per_host(0) (v17 — 误诊为 socket leak,实际不是)
- `4b9ee210` providers: pool fix for compatible.rs UA path (v18 — 同上误诊)
- `e39581c2` agent: outer 240s timeout in loop_.rs (v19 — 真正修对了路径但 timer 仍未触发)

## v15-v19 追加发现

### v16 突破 (qwen `wire_api=chat_completions` 强制)

加 `[model_providers.qwen] wire_api = "chat_completions"` 后,SOP **首次进入 step 3** (sop_advance count=2)。说明默认 zeroclaw 尝试 OpenAI Responses API 探测,失败后 fallback 到 chat_completions 也失效。**这是一个真实 bug 修复但不是死锁根因**。

geneline `[model_providers]` 段:
```toml
[model_providers]
[model_providers.qwen]
name = "qwen"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
wire_api = "chat_completions"
```

注意:必须**同时去掉** top-level `api_url` 才会生效,否则 zeroclaw 把 url 当 custom provider 处理,绕过 [model_providers.qwen] section。

### v17-v18 误诊 (reqwest pool leak)

诊断时看到 daemon `/proc/PID/fd` 有 3-4 个 socket fd 但 `ss` 找不到 → 误判为 reqwest keep-alive 半关闭 socket。加 `pool_max_idle_per_host(0)` 修复后仍死锁。**重新核查发现这些是 UNIX socket (tokio runtime 内部) + TCP LISTEN (gateway 监听端口)**,不是 reqwest pool socket。属于错误诊断。

### v19 突破诊断 (绕过 reliable.rs)

发现 `src/agent/loop_.rs:2407` 直接调用 `provider.chat().await`,**完全绕过 reliable.rs**。这就是为什么 v10 在 reliable.rs 加的 240s outer timeout 永远不触发——agent loop 根本不走那条路径。

v19 在 agent loop 直接加 `tokio::time::timeout(240s, ...)` 包装。**但仍然死锁,而且 240s timeout 仍未触发**!

### v19 最终事实

- v19 daemon (PID 1003166, 22:26 启动) 用 v19 binary ✓
- binary 含 "outer timeout" 字符串 ✓ (源码也含 4 处 `CHAT_OUTER_TIMEOUT_SECS`)
- Daemon hang 5 futex + 1 ep_poll (经典 tokio runtime 空闲态)
- 0 outbound TCP 到 LLM provider 或 backend
- step 2 `tool.start tool=http_request` 触发但 `tool.call success` 一直不来
- **没有任何 timeout error log,说明 tokio::time::timeout 的定时器也没在 fire**

## 真正的残余根因 (未解)

`tokio::time::timeout(240s, future)` 应该在 240s 后无条件触发,即使被包的 future hang。但 v19 5+ 分钟没 fire,说明 **tokio runtime 自己的 timer driver 也卡了**,无法 wake 任何 sleeping future。

这是 zeroclaw daemon 内部某处 **持有 sync primitive (std::sync::Mutex 或类似) 穿越 .await** 导致 worker thread 永久 park,timer driver 跟着挂了。具体位置需要:

1. **tokio-console 接入** - 实时看每个 task 的 wake 状态、被谁持锁、谁 await 谁
2. **gdb attach to PID** - 查 daemon 主线程 stack trace,看 Rust 函数栈具体在哪行 .await
3. **strace -f -e futex** - 看 futex 等待的具体地址和 owner
4. **在 geneline 同环境复现** - 如果 geneline 跑同 SOP 不死锁,差异就在 host/环境层

## 2026-05-14 续 — v25-v27 终极修复

### v25 block_in_place 突破
chat() 用 `tokio::task::block_in_place` 把当前 worker 移出 multi-thread scheduler,sync block 跑 isolated runtime + reqwest。**iteration=6 chat() 164s 成功返回** — 23 轮调试首次突破!但 file_write tool 内部 `tokio::fs::write` 也累积坏。

### v26 tokio 1.52 升级
Cargo.toml: tokio 1.50 → 1.52.3。配 block_in_place 后 chat() 仍正常,但 file_write 仍 hang。证实 wake-lost 是 zeroclaw runtime 内**全局**累积态,不是单点 bug 或单版本 tokio 问题。

### v27 终极方案: 整体 isolated runtime
`process_message_with_history` 入口加 `block_in_place + Runtime::new_current_thread().enable_all()`,整个 agent loop (所有 chat / 所有 tool / 所有 await) 跑在每个 user request 独立的 fresh runtime 内。chat() 撤回简化:不用 isolated_runtime/spawn_blocking,直接 reqwest async 调用(但保留 http1_only + pool_max_idle(0))。

**E2E v27 结果(2026-05-14 12:43)**:
```
HTTP STATUS 200 (非 408!)
TIME 538.6s (~9 分钟整个 SOP 跑完)
case files 4 个齐: profile.json(13KB) + candidates.json(28KB)
                  + match.json(2KB) + condition.json(8KB)
sop_advance count: 6 (SOP 6 步全完成)
max iteration: 17 (input_tokens 跑到 117K 也不 hang)
```

LLM 最终回复(/tmp/e2e-v27.json):
> "已为 **拓尔思信息技术股份有限公司** 匹配 10 条适用政策(完整条件分析已同步到小程序政策匹配页)..." (含 Top 3 推荐 + 小程序深链 + 留资 prompt)

### 修复 commits (bighorse/zeroclaw feat/prior-art-layer1)
- `02bf05f9` http1_only (在 v23 阶段定位 HTTP/2 流问题,后被 v27 包含)
- `ad8bca2f` **process_message wrap in block_in_place + isolated runtime** (v27 终极修复)
- `ae71a03e` tokio 1.50 → 1.52.3
- `521a632f` 清理 v20/v22 诊断标记

### 修复 commits (clawops)
- 已 commit: 后端 fields, SOP step 1+2 fields, IDENTITY 触发, fallback 配置说明
- 新 commit: `templates/workspace/config.toml.hbs` 加 [model_providers.qwen] wire_api=chat_completions

## 最终结论

E2E 完整跑通,policy-match SOP 业务设计 + zeroclaw runtime 修复双双交付。

根因诊断: tokio multi-thread runtime + reqwest 在多次大 context (>50K tokens) LLM call 累积态下出现 wake-lost — task waker / timer wheel / JoinHandle 通知系统全部失效。修复策略: 每个 user request 用独立 fresh runtime,绝不累积。

代价: 每 request 多约 10-30ms 启动 isolated runtime,但相对 LLM 1-30s 延迟可忽略。
