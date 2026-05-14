# policy-match 小程序前端对接说明 (2026-05-14)

> **背景**: zeroclaw daemon 端 SOP 已实测跑通完整 6 步 (E2E v27, 后端写库 + 卡片推送都验证)。本文给小程序前端开发讲清楚怎么触发 + 怎么处理 ~9 分钟的长响应。

---

## 触发方式

小程序在聊天页发送任意触发 policy-match 的消息,如:

- "帮我匹配下政策"
- "我想看看适用我们公司的政策"
- "政策匹配"
- "看看有什么补贴可以申报"

daemon 端 IDENTITY 第零原则强制识别这类意图 → 调用 `sop_execute('policy-match')`。

## 调用 endpoint

小程序已对接的 clawops gateway `/chat` 不变,**不需要任何前端代码改动**:

```
POST https://<clawops-gateway>/chat
Headers:
  Authorization: Bearer <user-paired-token>   # 已有的 wx-login 流程产物
  Content-Type: application/json
Body:
  {
    "message": "帮我匹配下政策",
    "openid": "<wx-openid>"     # wx.login() code 经后端 exchange 拿到的
  }
```

## 响应特性 — 重点!

policy-match SOP 完整跑完需要 **6-10 分钟** (qwen3.6-plus + 大 context),原因:

| 阶段 | 耗时 | 说明 |
|------|------|------|
| step 1 取企业画像 | 5-15s | 后端 enterprise_profile_sync 同步调用 |
| step 1 LLM 写 profile.json | 2-3 分钟 | qwen 写 13KB JSON |
| step 2 取政策清单 | <100ms | 后端 policy_summary 28KB |
| step 3 LLM 匹配 (100 条 → Top 10) | 2-3 分钟 | 大 context 推理 |
| step 4 LLM 条件分析 | 2-3 分钟 | 47 个 condition 评估 |
| step 5 写库 + 卡片推送 | 1s | POST save_match_result |
| step 6 对话回复 | 5-10s | 总结 Top 3 + 留资 prompt |

**HTTP 连接保持 ~9 分钟**是预期行为(等所有 step 跑完才返回最终回复)。

## 前端必须处理的两件事

### 1. 长 loading 状态

小程序 `wx.request` 默认 timeout 60s,**必须显式设大**:

```javascript
wx.request({
  url: 'https://<clawops-gateway>/chat',
  method: 'POST',
  timeout: 900000,  // 15 min,留余量
  data: { message: userInput, openid: wxOpenid },
  success(res) {
    // res.data.response 是 LLM 最终回复,含 Top 3 + 小程序深链 + 留资 prompt
    displayMessage(res.data.response);
  },
  fail(err) {
    if (err.errMsg.includes('timeout')) {
      // 超过 15 min — 极少见,展示 fallback
    }
  }
});
```

**UI 交互建议** (避免用户以为卡死):

- 立即在聊天列表显示用户消息
- 紧接着展示"政策顾问正在为您匹配...(预计 5-10 分钟)" 占位 bubble
- bubble 内带跳动动画 + 进度提示("正在分析企业画像 → 检索政策库 → 智能匹配 → 条件评估")
- 每 30-60s 切换进度文案,营造活跃感(其实是模拟,daemon 不发进度)
- 9 分钟后 success → bubble 替换为真实 LLM 回复

### 2. 双通道接收 — 小程序卡片推送

daemon 在 step 5 调 `save_match_result` 时,后端会**主动 push 一条"政策匹配完成"卡片**到该用户的小程序订阅消息/客服消息(`weapp_push_triggered=true`)。

这意味着:

- **HTTP /chat 同步等回复**: 一条文本消息(LLM 总结)
- **后端 weapp push**: 一张卡片(可点击进入小程序政策匹配页 `/pages/policy_match/index?enterprise_id=<id>`)

前端建议:

- 用户在聊天页等 9 分钟期间,**如果切到后台**, 卡片推送是兜底通知,用户能直接进政策匹配页看完整匹配结果
- 用户**没切后台**, /chat 返回的文本回复里也含小程序深链 `[小程序政策匹配页](/pages/policy_match/index?enterprise_id=16)`, 前端拦截 markdown 链接 click → `wx.navigateTo`

## 测试用例

让产品/测试同学按以下流程跑通端到端:

1. 在小程序新建一个测试账号(没用过 policy-match 的)
2. 进聊天页, 发"帮我匹配下政策"
3. **预期** loading 5-10 分钟 → 收到文本回复 + 卡片推送
4. 文本回复格式应像 `已为 **<公司名>** 匹配 10 条适用政策...TOP 3 优先级: (1)... (2)... (3)... → [小程序政策匹配页](/pages/policy_match/index?enterprise_id=<id>)`
5. 卡片推送点击 → 跳转到 `/pages/policy_match/index?enterprise_id=<id>` 看完整 10 条 + 条件分析

## 已知约束 (不需要前端处理)

- daemon 每次 chat 起一个 fresh isolated tokio runtime (~10-30ms 开销),从前端看不见
- 大 context 的 chat() 内部用 reqwest + http1_only + pool_max_idle=0,稳定走 dashscope qwen3.6-plus
- 后端 fields 过滤已上线 (profile 73→13KB, policy 67→28KB),否则 SOP 会 hang

## 故障排查

| 现象 | 排查方向 |
|------|---------|
| 小程序长 loading 9 分钟后超时,文本未到 | 看 daemon `/tmp/claw-<uid>.log`,SOP 是否 sop_advance 完整 6 次 |
| 文本回复到了但没卡片推送 | 看 daemon log 里 `save_match_result` 响应是否 `weapp_push_triggered=true` |
| 文本回复到了但 enterprise_id 错 | 看 step 1 返回的 enterprise_id 是否非 0 (企业未注册场景) |
| 用户 openid 没绑企业 | 后端 enterprise_profile_sync 会 Failed, daemon SOP `sop_advance status=Failed` 终止 |

## 相关代码 (clawops 仓库)

- SOP 定义: [templates/workspace/sops/policy-match/SOP.md.hbs](../templates/workspace/sops/policy-match/SOP.md.hbs)
- IDENTITY 触发规则: [templates/workspace/IDENTITY.md.hbs](../templates/workspace/IDENTITY.md.hbs)
- 后端契约: [policy-match-api-contract.md](policy-match-api-contract.md) + addendum-fields.md + addendum-profile-fields.md
- daemon 死锁修复历史: [policy-match-zeroclaw-deadlock-investigation.md](policy-match-zeroclaw-deadlock-investigation.md)
