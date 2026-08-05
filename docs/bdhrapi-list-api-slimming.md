# bdhrapi 列表接口改造需求：响应瘦身 + 报名 PII 治理

- **日期**：2026-06-10
- **发起**：ClawOps / zeroclaw 平台侧
- **目标接口**：`GET /shared/event`、`GET /commodity/services/products`（bdhrapi.2048office.com）
- **优先级**：P0（第 2 节，涉及个人信息泄露）、P1（第 3 节，响应瘦身）

---

## 1. 背景：两次线上故障的根因都指向列表接口响应过大

zeroclaw 智能助手（claw-011 租户，2026-06-10 事故）调用活动列表接口后，接口返回的完整 JSON 会进入 LLM 对话上下文并随会话保留。实测数据：

| 调用 | 响应大小 |
|---|---|
| `/shared/event?...&page_size=5` | **219,757 字节**（5 条记录） |
| `/shared/event?keyword=AI&...&page_size=10` | 141,595 字节 |
| `/shared/event?keyword=智能&...&page_size=10` | 102,434 字节 |

一次普通的"推荐活动"对话发出 3~4 次这类查询，累计约 **490KB ≈ 19 万 token**，直接把 LLM 的 25 万 token 上下文窗口顶爆，会话报错且永久卡死（当天 14:07 故障）。平台侧已做防御性修复（历史压缩与自动重试），但**数据源头不瘦身，体验和成本问题会一直存在**：每次活动推荐对话要多消耗约 20 万 token 的推理算力。

单条活动记录的字段大小分布（实测 `/shared/event` 第一条记录）：

| 字段 | 大小 | 占比 |
|---|---|---|
| `event_enrolls`（完整报名名单，115 条） | **60,394 字节** | ~96% |
| `enroll_fields` | 789 字节 | |
| `template_fields` | 573 字节 | |
| 其余全部字段（name/venue/start_time/...） | <600 字节 | |

**结论：列表接口 96% 的载荷是 `event_enrolls`，而列表场景根本不需要它。**

## 2. 【P0】`event_enrolls` 在列表接口全量返回报名者个人信息

`/shared/event` 列表接口的每条活动都内嵌完整报名记录，单条报名记录包含：

```
enroll_name（姓名）、company（公司全称）、phone（手机号，明文）、
email、identity_number（身份证号字段）、open_id（微信）、
sign_operator_phone、remark、custom_fields ...
```

实测一个活动返回了 **115 条带明文手机号的报名记录**。这意味着：

1. 任何能调用该列表接口的客户端都能批量拉取报名者姓名+公司+手机号；
2. 平台侧 AI 助手调用该接口后，这些 PII 进入 LLM 对话上下文与持久化历史，**并已实际发送至第三方推理端点**；
3. 按《个人信息保护法》最小必要原则，列表场景对报名名单没有任何使用需求。

**要求（P0）：**
- 从 `/shared/event` 列表响应中**移除 `event_enrolls`**，如需统计请改为 `enroll_count`（整数）；
- 报名明细收敛到独立接口（如 `GET /shared/event/{id}/enrolls`），并加权限校验（仅活动主办方/管理员可见）；
- 排查其他列表接口是否存在同类内嵌 PII 集合的问题（如 `sign_operator_phone` 出现的所有响应）。

## 3. 【P1】列表接口支持字段裁剪，与详情分离

列表与详情职责分离，参照以下两种方案任选其一（推荐 A）：

**方案 A：list/detail 视图分离（推荐，改动小、不易误用）**
- `GET /shared/event`（列表）固定返回精简字段：
  `id, name, organizer, venue, start_time, end_time, deadline, status, is_top, enroll_count, images(仅首图)`
  ——单条预计 <1KB，整页 page_size=10 约 10KB（现状 ~200KB，**缩小 95%**）；
- `GET /shared/event/{id}`（详情）返回完整字段（除第 2 节的 PII 收敛项）。

**方案 B：`fields` 查询参数白名单**
- 形如 `?fields=id,name,start_time,venue`，服务端按白名单过滤；
- 未传 `fields` 时保持现状（兼容存量调用方），但 `event_enrolls` 仍按 P0 移除。

`/commodity/services/products` 同样处理：`description`（长富文本）和 `images` 在列表场景可截断或移除，保留 `service_name`、`category_id`、`enterprise_id`、`shop_id`、价格等检索字段。

## 4. 验收标准

1. `/shared/event` 列表响应中不再出现 `phone`、`identity_number`、`open_id`、`enroll_name` 等 PII 字段（自动化断言）；
2. `page_size=10` 的列表响应 ≤ 20KB；
3. 现有小程序/前端调用方回归通过（如选方案 B，未传 `fields` 的调用方仅受 P0 移除影响，需要确认前端没有直接消费 `event_enrolls`——若有，迁移到新的报名明细接口）；
4. 平台侧验证：AI 助手"推荐活动"完整对话的上下文增量从 ~19 万 token 降到 1 万 token 以内。

## 5. 附：给推理服务（vLLM，220.203.247.222:8011）维护方的两条建议

与本需求并行、非阻塞：

1. **固定服务模型别名**：vLLM 启动加 `--served-model-name`（如固定为业务别名），后续更换底层模型不再破坏客户端写死的模型名（2026-06-10 上午的 404 故障即此原因：模型从 Qwen3.5-122B-A10B 换为 Qwen3.6-35B-A3B，调用方配置未同步）；
2. **上下文长度**：若显存允许，将 `--max-model-len` 提升到模型原生上限（Qwen3.6-35B 支持 256K+），并更换 `bj123` 这类弱 API key、限制来源 IP——该端点当前公网可达。

---

*平台侧联系人：Mario（ClawOps）。本文档随附原始实测数据可复现：对上述接口直接 curl 即可。*
