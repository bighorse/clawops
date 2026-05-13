# 政策匹配 SOP - 路由测试 spec

> **目的**: 在**后端 endpoint 尚未上线**的前提下,验证 zeroclaw daemon 内的 LLM 是否能根据 IDENTITY.md 的"第零原则·政策匹配路径"规则,正确把用户意图路由到 `sop_execute('policy-match')`,并正确提取 payload。
>
> **不验证**: 完整匹配结果、写库、小程序卡片推送(这些依赖后端 [policy-match-api-contract.md](policy-match-api-contract.md) 里的 2 个 endpoint)。
>
> **预期**: 测试用例都启动 SOP 后,step 1 调 `GET /agent/enterprise_profile_sync` 必然返回 4xx/5xx(endpoint 不存在),SOP 按设计降级 narrate "企业画像服务暂时不可用" 并以 `Failed` 终止 — **这是正常的、证明 SOP 已被路由**。

---

## 前置环境

| 组件 | 版本/状态 |
|---|---|
| clawops 代码 | 含本 PR 的 sops/ 扫描 + IDENTITY 路由规则 |
| 部署 backend | `mock`(本地 dev)或 `systemd`(staging) |
| zeroclaw daemon | 启用 `[sop] enabled = true`(模板已写) |
| 一个测试 openid | 已 provision,workspace 内 `sops/policy-match/` 目录存在 |
| LLM provider | 跟生产一致(默认 `qwen3.6-plus`),否则路由判断不可比 |

### 准备步骤

```bash
# 1. clawops build
cd /Users/mario/Code/clawops
cargo build --release

# 2. 起 clawops (mock backend)
./target/release/clawops --config clawops.toml serve

# 3. 另一个 shell:provision 测试用户(如果不存在)
./target/release/clawops --config clawops.toml provision \
  --openid test_routing_001 \
  --phone 13800138001 \
  --display-name "路由测试" \
  --enterprise-profile '{"company_name":"测试科技有限公司","industry":"软件","stage":"成长"}'

# 4. 验证 SOP 文件已渲染到测试用户 workspace
ls /home/claw-XXX/.zeroclaw/workspace/sops/policy-match/
# 期望: SOP.toml, SOP.md
```

### 观察渠道

每次发消息后,在以下三处确认 SOP 是否被触发:

| 渠道 | 命令/路径 | 看什么 |
|---|---|---|
| **runtime-trace** | `tail -f /home/claw-XXX/.zeroclaw/state/runtime-trace.jsonl` | 找含 `"tool":"sop_execute"` 的行 |
| **SOP 运行列表** | `sudo -u claw-XXX zeroclaw sop list`(或对应 daemon 内 admin endpoint) | 是否新增 run,sop_name=`policy-match`,status=`Running`/`Failed`/`Completed` |
| **对话回复** | clawops `/chat` 接口或小程序 | LLM 的最终回复内容是否符合"已为 X 匹配..."或降级文本 |

---

## 测试用例表

### A 组:正例 — **必须**触发 `sop_execute('policy-match')`

| # | 用户输入 | 期望 payload.enterprise_name | payload.openid | 期望 SOP 终止状态 |
|---|---|---|---|---|
| A1 | `帮我匹配下政策` | `"测试科技有限公司"`(取 IDENTITY 兜底) | `test_routing_001` | Failed(step 1 endpoint 不存在,降级 narrate"企业画像服务暂时不可用") |
| A2 | `我能申请什么政策` | 同上 | 同上 | 同上 |
| A3 | `我们公司适合什么政策?` | 同上 | 同上 | 同上 |
| A4 | `看看我有什么政策可以申请` | 同上 | 同上 | 同上 |
| A5 | `帮华为公司匹配下政策` | `"华为公司"` 或 `"华为"`(消息内指定优先) | 同上 | 同上 |
| A6 | `我企业能拿哪些政策补贴?` | 取 IDENTITY 兜底 | 同上 | 同上 |
| A7 | `帮我评估一下,我企业符合哪些政策` | 同上 | 同上 | 同上 |

**通过标准**:
- runtime-trace 出现 `sop_execute` 工具调用,`name=policy-match`
- payload 含 `enterprise_name`(值符合表中规则)和 `openid=test_routing_001`
- 对话回复**不**出现具体政策名 / 金额 / 申报条件 — 因为没真去拿数据,凭记忆作答就是违规

### B 组:负例 — **不应**触发 `policy-match` SOP,应走 `policy-recommend` skill

| # | 用户输入 | 期望路由 | 通过标准 |
|---|---|---|---|
| B1 | `什么是高新企业认定?` | `policy-recommend` skill | runtime-trace 出现 `http_request` GET `bdhrapi.2048office.com/policies/...`,**不**出现 `sop_execute` |
| B2 | `怀政发[2024]16号文是什么?` | 同上 | 同上 |
| B3 | `有哪些研发补贴政策?` | 同上(查询型) | 同上,**不**出现 `sop_execute`(这条容易误判,见已知风险) |
| B4 | `这条政策怎么申报?` | 同上,可能转 commodity-recommend(代办意图) | 不出现 `sop_execute` |
| B5 | `谁拿过青创无忧十条?` | `policy-recommend` skill(announcements 端点) | 同上 |

**通过标准**:
- runtime-trace **不**出现 `sop_execute('policy-match')`
- LLM 调 `http_request` 走外部 2048office API(`bdhrapi.2048office.com/policies/...`)而**不**调 `bdhrapi.2048office.com/agent/...`

### C 组:边界 — payload 提取 / 兜底链

| # | 用户输入 | 测试焦点 | 期望行为 |
|---|---|---|---|
| C1 | `帮"北京字节跳动科技有限公司"匹配政策` | 引号内的完整企业名提取 | payload.enterprise_name 包含完整全称(可带"有限公司") |
| C2 | (先发 `我是字节跳动法务负责人`,等 LLM 静默 `memory_store`,再发) `帮我匹配政策` | memory_recall 兜底 | payload.enterprise_name 来自 `user_profile_company_name` memory,而非 IDENTITY 的 `测试科技有限公司`。**这条最容易出问题** — 如果 LLM 把 IDENTITY 静态值视为最高优先级,会错。需明确兜底链:消息指定 > memory_recall > IDENTITY 静态 |
| C3 | **新建一个 openid** `test_routing_blank`,provision 时不传 `enterprise-profile`,然后发 `帮我匹配政策` | 兜底链全失败 | LLM **不应**启动 SOP,而是回 `请告诉我要为哪家企业匹配政策?`(SOP.md 的兜底规则) |
| C4 | `给我推荐一些政策` | "推荐"措辞模糊 — 介于查询型和匹配型之间 | 可接受任一路由(documented as ambiguous);如果路由不稳定,需考虑在 IDENTITY 加更明确的判别 |

### D 组:抗噪 — 同一会话多轮,SOP 触发不应重复

| # | 用户多轮输入 | 期望 |
|---|---|---|
| D1 | 第 1 轮 `帮我匹配政策` → SOP 跑完 Failed(endpoint 不存在)。第 2 轮 `再匹配一次` 或 `重新跑下` | 应再次触发 `sop_execute`(cooldown=30s 已过)。不应"懒",不应说"刚才匹配过了"|
| D2 | 第 1 轮 `帮我匹配政策` 启动后,第 2 轮**立刻**(<30s)再发 `帮我匹配政策` | `cooldown_secs=30` 应拒绝,LLM 应说"上次匹配请求 30 秒内,稍等" 而**不**重复触发 |
| D3 | 第 1 轮 `帮我匹配政策`,SOP 跑完。第 2 轮 `我手机号 13800138888` | 应触发 IDENTITY 里的"政策匹配留资跟进"段:`memory_store(category=policy-match-lead)` + 调飞书 webhook(如配置) |

---

## 已知风险(测试时重点观察)

### 1. LLM 路由不稳定(B3 / C4 高危)

**风险**: B3 `有哪些研发补贴政策?` 措辞含"补贴"+"政策",但属于查询型,应走 `policy-recommend` skill。LLM 可能误读"哪些"+"政策"= "帮我匹配"。

**判别依据**:
- 主语是"我/我企业"→ 匹配型(SOP)
- 主语缺省、问"哪些/什么/怎么申报"→ 查询型(skill)

**如果 B3 经常误触发 SOP**,需要回去把 IDENTITY 里"政策匹配路径·触发条件"的关键词收紧,例如显式要求"我/我们/我企业"做主语。

### 2. memory_recall 优先级(C2 高危)

**风险**: IDENTITY 里 `{{enterprise.company_name}}` 是 provision 时注入的静态值,LLM 视野里它**优先级高**(出现在 system prompt),memory_recall 拉回来的画像可能被忽略。

**如果 C2 测试失败**:`enterprise_name` 仍取 IDENTITY 静态值而非 memory 里的真企业名 → 用户 memory 里的画像被废弃。

**应对**:
- 选项 a:SOP.md step 1 显式要求**先 memory_recall**,memory 命中就覆盖
- 选项 b:provision 时若 IDENTITY 渲染的 enterprise.company_name 为占位值/空,模板用 `{{else}}` 留空字符串,让 LLM 自然走 memory_recall
- 当前 IDENTITY 模板 L3 用 `{{#if enterprise.company_name}}...{{else}}(企业信息待补全){{/if}}` 已处理 — **但 SOP.md 兜底链没显式说"用户改口换企业时怎么办"**。C2 测试失败就回 SOP.md 加这条

### 3. cooldown 体验(D2)

`cooldown_secs=30` 在测试中故意短,生产可能要调到 300。D2 触发时 LLM 应该**自然解释**,不是抛技术错误。

### 4. 路由测试不可重现性

LLM 输出有 stochastic 成分。同一条用例**跑 3 次**,3 次都通过才算稳。建议:

```bash
# 每条用例跑 3 次,记录 pass/total
for i in 1 2 3; do
  curl -X POST http://127.0.0.1:8088/chat \
    -H "Authorization: Bearer $TOKEN" \
    -d '{"content":"帮我匹配下政策"}' | jq '.'
  sleep 5  # 跨 cooldown,避免互相阻断
done
```

3/3 ✅ / 2/3 ⚠️(可接受但需观察)/ ≤1/3 ❌(必须改 IDENTITY)

---

## 通过/失败汇总表(测试者填)

| 用例 | 跑 3 次结果 | 通过? | 备注 |
|---|---|---|---|
| A1 | / | ⬜ | |
| A2 | / | ⬜ | |
| A3 | / | ⬜ | |
| A4 | / | ⬜ | |
| A5 | / | ⬜ | enterprise_name 是否完整提取? |
| A6 | / | ⬜ | |
| A7 | / | ⬜ | |
| B1 | / | ⬜ | |
| B2 | / | ⬜ | |
| B3 | / | ⬜ | 易误判,重点观察 |
| B4 | / | ⬜ | |
| B5 | / | ⬜ | |
| C1 | / | ⬜ | |
| C2 | / | ⬜ | memory 优先级,易失败 |
| C3 | / | ⬜ | |
| C4 | / | ⬜ | 路由可接受任一 |
| D1 | / | ⬜ | |
| D2 | / | ⬜ | |
| D3 | / | ⬜ | 飞书 webhook 触发是否正确 |

**全表 3/3 通过** → 进入下一阶段(等后端 endpoint 1 上线,跑主流程测试)
**部分用例 2/3** → 记下风险用例,后端 endpoint 上线后再回归
**任一用例 0/3 或 1/3** → 暂停推进,回 IDENTITY/SOP.md 修规则,改完重测

---

## 测试报告模板

测试者填写下面这块,贴到 issue / Slack 同步:

```
日期: 2026-MM-DD
测试者: <name>
clawops commit: <sha>
LLM: qwen3.6-plus(温度 0.6)
测试用户 openid: test_routing_001

A 组 (7 条): X 通过, Y 部分通过, Z 失败
B 组 (5 条): ...
C 组 (4 条): ...
D 组 (3 条): ...

失败用例详情:
- C2: 3 次都失败,LLM 用 IDENTITY 的"测试科技"而非 memory 的"字节跳动" → 待 SOP.md 加显式 memory_recall 优先级规则

下一步:
- [ ] 修复 C2 失败 → 改 SOP.md step 1 兜底链
- [ ] 等后端 endpoint 1 上线 → 跑 [主流程测试]
```
