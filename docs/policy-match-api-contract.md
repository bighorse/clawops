# 政策匹配 SOP - 后端接口契约

> **背景**: clawops 在 zeroclaw daemon 内新增 `policy-match` SOP,替代原 FastGPT 政策匹配工作流。SOP 完全在 daemon 本地 LLM 内执行匹配逻辑,后端只承担**纯数据源**职责 (画像取得 + 结果落库 + 小程序卡片推送)。
>
> **范围**: 后端 (ztagent-service-api) 需新增 **2 个** 同步 HTTP endpoint。本契约一次性把字段、语义、降级行为写清,后端按约实现即可。
>
> **状态**: 待后端 review 签字。schema 调整请直接在本文件标注,clawops 这边的 [SOP.md.hbs](../templates/workspace/sops/policy-match/SOP.md.hbs) 会跟随同步。
>
> **生效版本**: v1.0.0(对应 SOP `policy-match` v1.0.0)

---

## 总览

| # | Method | Path | 用途 | 同步? | 后端工作量 |
|---|---|---|---|---|---|
| 1 | GET | `/agent/enterprise_profile_sync` | 取企业完整画像(工商 + 资质 + 三方搜索) | ✅ 同步 | 把现有 `resolve_policy_business` 头部串行的 4 个调用提出来,**移除** `background_tasks` 包装 |
| 2 | POST | `/agent/save_match_result` | 原子写匹配结果 + 触发小程序卡片推送 | ✅ 同步 | 复用 `resolve_policy_first_agent_result` 里的 delete + insert + redis weapp 推送逻辑 |

**共同约定**:

- 所有 endpoint 走 `ResponseWrapperModel` 包络: `{"request_id": "...", "message": null, "data": {...}}`
- `data` 为 `null` 视同失败(SOP 已按此约束设计降级逻辑)
- 字段命名: `snake_case`,与现有 `src/domain` 下 SQLModel 字段对齐
- 时间戳: ISO 8601 / RFC 3339(`2026-05-12T10:00:00+08:00`)
- 鉴权: 复用现有约定(目前 `policy_agent_manager_router` 用 `depends_authorization=lambda: None`,本契约建议保持一致 — 在 ClawOps 内部网络访问,daemon 不持有 token)

---

## Endpoint 1: `GET /agent/enterprise_profile_sync`

### 用途

同步取企业完整画像。**禁止异步**(不要 `background_tasks`、不要 rabbitmq 派发) — SOP 调用方需要拿到 enterprise_id 才能推进下一步。

### Query 参数

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `enterprise_name` | string | ✅ | 企业全称(可中文),用作 `QualificationEnterprise` 主键查询和三方 API 查询关键词 |
| `agent_user_info` | string | ✅ | 当前用户的唯一标识。clawops 透传的是**微信 openid**(28 位字符串),后端按现有 `agent_user_info` 字段语义保存即可 |
| `business_type` | string | ❌ | 默认 `null`。保留兼容字段,SOP 当前**不传** |
| `identity_marker` | string | ❌ | 默认 `null`。保留兼容字段,SOP 当前**不传** |

### 响应 (200 OK)

```json
{
  "request_id": "uuid",
  "message": null,
  "data": {
    "enterprise_id": 12345,
    "qualification_enterprise_id": 67890,
    "enterprise_name": "北京XX科技有限公司",
    "basic_info": "企业全称:...\n统一社会信用代码:...\n注册资本:...\n注册地址:...\n经营范围:...\n参保人数:...\n成立日期:...",
    "qualification_info": "专利:N 项\n商标:M 个\n资质证书:[国家高新技术企业认定/...]\n荣誉:[...]",
    "bocha_info": "三方搜索摘要文本拼接..."
  }
}
```

**字段语义**:

| 字段 | 来源 | 备注 |
|---|---|---|
| `enterprise_id` | `Enterprise.id` | **必填,非 null**。SOP step 5 写库的外键 |
| `qualification_enterprise_id` | `QualificationEnterprise.id` | 可为 null(企业首次提问且 QualificationEnterprise 表尚无记录) |
| `enterprise_name` | 入参回显 | 用于 SOP 路径命名 sanitize |
| `basic_info` | `get_enterprise_basic_info()` + `get_enterprise_detail_by_info()` | 拼接成自然语言文本,供 LLM 阅读;**不要**返回结构化对象(LLM 用文本判断更稳) |
| `qualification_info` | `get_enterprise_qualification_info_by_info()` | 同上,文本格式 |
| `bocha_info` | `Enterprise.bocha_info` 字段(经 `get_bocha_enterprise_info` 填充) | 同上 |

### 内部实现提示

把现有 [resolve_policy_business](../../ztagent-service-api/src/app/agent/bot_agent_message_business.py#L83) 头部的串行采集逻辑(L92-L116)抽出来同步返回。当前那段长这样:

```python
qualification_enterprise = get_db_qualification_enterprise_by_name(...)
enterprise = get_enterprise_basic_info(session, settings, rabbitmq, enterprise_name, ...)
enterprise = get_enterprise_qualification_info(session, settings, rabbitmq, ...)
enterprise = get_bocha_enterprise_info(session, settings, rabbitmq, ...)
basic_total_info = get_enterprise_detail_by_info(enterprise)
qualification_total_info = get_enterprise_qualification_info_by_info(enterprise)
bocha_info = enterprise.bocha_info
```

抽到新 endpoint 后,**移除** `send_ai_agent_request_by_params` 这一步(rabbitmq 派发给 FastGPT — 本 SOP 完全用 daemon 本地 LLM 替代,不再走 FastGPT)。

### 错误处理

| HTTP | data | 含义 | SOP 行为 |
|---|---|---|---|
| 200 | 上述完整 data | 成功 | 推进 step 2 |
| 200 | `null` 或缺 `enterprise_id` | 三方 API 失败或企业不存在 | step 1 失败终止,告诉用户"企业画像服务暂时不可用" |
| 4xx/5xx | — | 接口/网络错误 | 同上 |

**特别**: 不要因为"三方 API 限流"或"bocha 临时无结果"就 5xx — 这种情况下 `basic_info` 留空字符串即可,`enterprise_id` 仍要给出。LLM 在 step 3 会处理画像不全的情况。

---

## Endpoint 2: `POST /agent/save_match_result`

### 用途

**原子替换**指定企业的政策匹配结果(覆盖该 enterprise 的旧 `EnterprisePolicySummary` + `EnterprisePolicySummaryCondition`),并触发小程序政策匹配页卡片推送。

### Request body

```json
{
  "enterprise_id": 12345,
  "agent_user_info": "openid_xxxxx",
  "matched_policies": [
    {
      "policy_summary_id": 101,
      "match_reason": "企业为高新技术企业,符合研发投入扶持政策方向",
      "rank": 1,
      "expired": false,
      "conditions": [
        {
          "requirement": "申报主体需为高新技术企业",
          "enterprise_profile": "已认定为国家高新技术企业(2024-06)",
          "match_status": "yes"
        },
        {
          "requirement": "上年度研发投入占营收 ≥ 5%",
          "enterprise_profile": "N/A",
          "match_status": "unknown"
        }
      ]
    }
  ]
}
```

### Body 字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `enterprise_id` | int | ✅ | 来自 Endpoint 1 的 `enterprise_id`,外键到 `Enterprise.id` |
| `agent_user_info` | string | ✅ | 用户 openid,用于触发推送时定位 sender |
| `matched_policies` | array | ✅ | 长度 0-N。**长度为 0** 时:后端**仍要清空**该 enterprise 的旧匹配数据,不要触发推送(避免推空卡片) |
| `matched_policies[].policy_summary_id` | int | ✅ | `PolicySummary.id` 外键。后端**必须**校验存在性,不存在则整条 reject(避免 LLM 幻觉污染) |
| `matched_policies[].match_reason` | string | ✅ | 1 句话理由,LLM 生成,30 字内 |
| `matched_policies[].rank` | int | ✅ | 排序号,1 起步 |
| `matched_policies[].expired` | bool | ✅ | 政策是否已过期(LLM 基于 `application_at` 字段判断) |
| `matched_policies[].conditions` | array | ✅ | 长度 3-8,LLM 拆解的条件项 |
| `conditions[].requirement` | string | ✅ | 条件原文或精炼描述 |
| `conditions[].enterprise_profile` | string | ✅ | 企业对应实际值,LLM 不知时填 `"N/A"`(**禁止留空字符串**) |
| `conditions[].match_status` | enum | ✅ | `yes` / `no` / `partial` / `unknown` 四值,**严格枚举** |

### 后端写入语义(**必须原子**)

在单个事务内:

1. `DELETE FROM enterprise_policy_summary_condition WHERE enterprise_policy_summary_id IN (SELECT id FROM enterprise_policy_summary WHERE enterprise_id = :enterprise_id)`
2. `DELETE FROM enterprise_policy_summary WHERE enterprise_id = :enterprise_id`
3. 遍历 `matched_policies[]`:
   - INSERT `EnterprisePolicySummary` (复制 `PolicySummary.id` 对应行的快照字段:name / link / sponsor / department / contact_information / support_money / published_at / application_at / application_method / application_condition / application_material,参考 [resolve_policy_first_agent_result](../../ztagent-service-api/src/app/agent/bot_agent_message_business.py#L329) 的字段复制范式)
   - 取得新 `EnterprisePolicySummary.id`
   - 遍历该政策的 `conditions[]`,INSERT `EnterprisePolicySummaryCondition`,外键到新 `EnterprisePolicySummary.id`
4. **事务提交后** 触发推送(下一节)

中途任何一步异常,**事务回滚**,500 给调用方,SOP 端走"展示但不写库"降级。

### 推送触发(写库成功后)

复用 [resolve_policy_first_agent_result](../../ztagent-service-api/src/app/agent/bot_agent_message_business.py#L329) 末尾(L390+)的 redis weapp 推送那段。需要的 key:

- `EnterprisePolicySummary_{agent_user_info}_{enterprise_id}` — 数据 ready 标记
- `SEND_WEAPP_EnterprisePolicySummary_{agent_user_info}_{enterprise_id}` — 推送任务键

具体 redis schema / 小程序订阅协议沿用现有约定,后端自决,SOP 端不关心。

**长度为 0 的 matched_policies**: 仍跑事务 1-2(清空旧数据),**不**触发推送、**不**写 redis 标记。

### 响应 (200 OK)

```json
{
  "request_id": "uuid",
  "message": null,
  "data": {
    "enterprise_id": 12345,
    "saved_count": 8,
    "condition_count": 32,
    "weapp_push_triggered": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `enterprise_id` | int | 回显 |
| `saved_count` | int | 实际写入的 `EnterprisePolicySummary` 行数 |
| `condition_count` | int | 实际写入的 `EnterprisePolicySummaryCondition` 行数 |
| `weapp_push_triggered` | bool | 推送是否触发(false = matched_policies 为空或推送配置缺失) |

### 错误处理

| HTTP | 含义 | SOP 行为 |
|---|---|---|
| 200 | 成功 | 推进 step 6 |
| 400 | body schema 不合法 / `policy_summary_id` 不存在 / `enterprise_id` 不存在 | step 5 降级 — 仍在对话内展示概要,提醒"小程序卡片可能未同步" |
| 500 | 写库/推送故障 | 同上降级 |

---

## 字段命名一致性自检(**后端 review 时务必过一遍**)

下表是 SOP.md 假定的字段名。如果后端实际命名不同,**改这里**,SOP.md 同步替换:

| SOP 假定字段 | 后端 SQLModel 字段 | 一致? |
|---|---|---|
| `enterprise_id` | `Enterprise.id` (int) | ⬜ |
| `qualification_enterprise_id` | `QualificationEnterprise.id` (int) | ⬜ |
| `enterprise_name` | `Enterprise.name` 或 `QualificationEnterprise.name` (str) | ⬜ |
| `policy_summary_id` | `PolicySummary.id` (int) | ⬜ |
| `match_reason` | 新字段,**建议加到 `EnterprisePolicySummary`** 或单独列存 | ⬜ |
| `rank` | 新字段,同上 | ⬜ |
| `expired` | 新字段,同上 | ⬜ |
| `match_status` 枚举 `yes/no/partial/unknown` | `EnterprisePolicySummaryCondition.match_status` 现有值是? | ⬜(**待后端确认**) |

> ⚠️ **`match_status` 枚举值**: SOP.md 用了 `yes`/`no`/`partial`/`unknown`。后端表里现有 `match_status` 字段的取值可能是 `"匹配"/"不匹配"/...` 或其他。需要任一方对齐。
>
> 建议:**保持中文** `"匹配"/"不匹配"/"部分匹配"/"未知"`(跟现有数据兼容),SOP.md 同步改。

---

## 待后端确认的开放问题

1. **`match_status` 枚举值**:沿用现有中文还是改英文?决定后 SOP.md 同步改占位符。
2. **`match_reason` / `rank` / `expired` 字段**:`EnterprisePolicySummary` 表现在有没有这几个字段?没有需要 alembic 加列。
3. **`enterprise_id` 取得时机**:Endpoint 1 是否必须**已经把 Enterprise 行 commit 入库**(SOP step 5 依赖这个 id 做外键)?如果是先生成临时 id 后异步入库,Endpoint 1 要改成"等 commit 完成后返回"。
4. **推送 redis schema**:`EnterprisePolicySummary_{user}_{ent}` 这个 key 现在的 TTL / value 结构是?新写入逻辑需要严格兼容,否则前端订阅会失效。
5. **鉴权**:是否完全跟 `policy_agent_manager_router` 一样不验 token?或者要校验 `agent_user_info` 必须是已注册用户?

---

## 端到端 smoke test 用例

后端两个 endpoint 上线后,用 `curl` 跑一次:

```bash
# 1. 取画像
curl "https://bdhrapi.2048office.com/agent/enterprise_profile_sync?enterprise_name=北京XX科技有限公司&agent_user_info=test_openid_001"

# 期望: data.enterprise_id 是个正整数,basic_info/qualification_info/bocha_info 是字符串

# 2. 写匹配结果(用上一步返回的 enterprise_id)
curl -X POST "https://bdhrapi.2048office.com/agent/save_match_result" \
  -H "Content-Type: application/json" \
  -d '{
    "enterprise_id": <step1 返回的 enterprise_id>,
    "agent_user_info": "test_openid_001",
    "matched_policies": [
      {
        "policy_summary_id": <从 /policy_summary?no_pagination=true 拿一个真实 id>,
        "match_reason": "smoke test",
        "rank": 1,
        "expired": false,
        "conditions": [
          {"requirement": "测试条件", "enterprise_profile": "N/A", "match_status": "unknown"}
        ]
      }
    ]
  }'

# 期望: saved_count=1, condition_count=1, weapp_push_triggered=true
# 验证: 小程序政策匹配页能看到这条 smoke test 卡片
```

两个用例都通过后,clawops 这边触发 SOP 跑真实匹配,即可端到端验收。
