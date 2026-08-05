# 给 2048 后端的待办清单（企业微信政策/资质匹配）

**日期**：2026-07-01（P0-0 于 2026-07-24 重写为服务器实证版）　**背景**：网关(clawops)+zeroclaw 侧已修完能修的部分（见文末"我方已完成"）；以下几条的根子在 2048 后端，需你们处理。按影响面排序。

---

## 🔴 P0-0（最高优先，疑似所有"消息到不了 clawops"的总根子）　后端 `/chat` 发的 Bearer 不是 clawops 签发的 token
**现象**：后端调 `POST https://clawops.2048office.com/chat` 全部返回 **401 "invalid token"**（nginx：7/24 的 /chat 全 401，来源 IP 180.76.243.107/120.48.62.181、UA python-requests；且 7/24 后端 `/auth/wecom-login` 调用 = **0 次**）。消息进不了 clawops → 退回后端自身兜底（假承诺/"会话人数较多"）。

**根因（已在服务器逐条实证，2026-07-24）**：后端 `/chat` 的 `Authorization: Bearer` 里放的是一个 **clawops 从未签发过的 token**（实测样本 `113f7936…a1aabae`，64位，后端自报"过期时间 2026-07-29、来自 wecom-login"）。逐条证据：
- 全服只有 1 个活库 `/var/lib/clawops/data/clawops.db`（另 3 个 .db 都是 0 字节空壳）；运行进程经 `lsof` 确认只打开这一个库。
- 该 token **不在** sessions 表；在 clawops 日志（6-25 至今）**0 次出现**。
- 活库**没被清过**：session 一路回溯到 5-08，6-29 当天记录都在。clawops 签发 token 是**同步写库**的（6 月的 session 到现在都留着），若这 token 是 clawops 发的必然在表里。它不在表、也没进过日志 = **clawops 从没发过它**。
- 时间线矛盾坐实"后端今天换了 token 来源"：`/chat` 状态码 7-10~7-23 **全 200**，**只有 7-24 翻成全 401**；该 token 过期时间 7-29 = 今天 +5 天（≠ clawops 的 30 天 TTL；若按 30 天倒推应签发于 6-29 01:43:42，但活库里查无此记录）→ 像后端用**自己那套 TTL 今天新签的 token**。

**纠正上一版（本清单早前误判，务必知悉）**：早前写"后端把 32 位 `identity_marker` 当 Bearer 发"——**不成立**。Postman 抓包证明 Bearer 里是 64 位 token，`identity_marker` 只在 body（clawops 不读 body 这个字段）。早前之所以"复现"成功，是因为**任何** clawops 没签发过的串都会得到同一句 401——对了症状、错了病因。

**精确时间线坐实根因（2026-07-24，决定性）**：把 nginx 里 `wecom-login` 与成功的 `/chat` 排到一起，指纹极清楚——**历史上每一次 200 的 /chat，前几秒必有一次 wecom-login**（7-10 相隔 2s、7-15 3s、7-17 18s/13s、7-22 2s、7-23 2s）。即后端一贯做法是"**发消息前先 login 拿新 token、秒级内立刻用**"（也是 sessions 表里一堆冗余 uin session 的来源）。而 **7-24 崩掉的 4 次（10:47/10:50/10:53 来自 180.76.243.107、11:00 来自 120.48.62.181）之前，没有任何 wecom-login**；直到 12:17 恢复 login→12:20 /chat 立刻 200。→ **7-24 后端把"发消息前先 login"这一步弄丢了，改成直接复用一个存着的 token（`113f7936…`），而那个存值不是 clawops 活着的 token → 401。**

**真正的 bug（在后端新加的 token 缓存里）**：后端**一 login 就好使**（12:17 login→200），但缓存里存的 `113f7936…` 却是 clawops **从没签发过**的。→ 后端的缓存层**写进/读出的不是 `wecom-login` 响应 JSON 里的 `token` 字段**，而是别处的值（自造 / 占位 / 旧字段）。

**请后端改**：顺着代码里 `token` 变量往上查——它到底来自 `/auth/wecom-login` 的**响应体 `token` 字段**，还是后端自己 session 系统 / 自己生成的值？
- 治标：遇 401 重新 login。
- **治本**：token 缓存里存的必须**就是** wecom-login 返回的 `token`；30 天内复用、遇 401 刷新（**别**回退成"每条消息都 login"那种浪费打法，也**别**用自管/自造 token）。用"打印 wecom-login 响应体 vs 实际发出的 Bearer"一比即现形。

**⚠️ 值得警惕（格式巧合太可疑）**：这个假 token 是 64 位十六进制，与 clawops token 格式（两段 uuid 去横线）**完全一致**。高度怀疑后端在**照 clawops 格式自己生成/仿造 token**，或缓存了一份自管 token 误当等价。若如此，仅"遇 401 重试"治标不治本——须确认后端存的 `token` 是否真来自 clawops wecom-login 的响应体。

**可直接跑的复现（后端本机即可，`X-Admin-Token` 你们有）**：
```
① 你现在发的 token（clawops 没签发过）→ 401：
   curl -s -X POST https://clawops.2048office.com/chat \
     -H "Authorization: Bearer 113f7936359449a2a8125ee164d331deab5ee01bcbb148b49d904f858a1aabae" \
     -H "Content-Type: application/json" -d '{"content":"你能做什么?"}'
   → invalid token

② 正确流程 → 200：
   TOKEN=$(curl -s -X POST https://clawops.2048office.com/auth/wecom-login \
     -H "X-Admin-Token: <你们的管理员令牌>" -H "Content-Type: application/json" \
     -d '{"uin":"<该用户uin>"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
   curl -s -X POST https://clawops.2048office.com/chat \
     -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
     -d '{"content":"你能做什么?"}'
   → 正常回复
```
（早前对照实验亦证明 clawops 侧无需改：手动 `wecom-login {uin:7881303049925005}` → 200 拿 64 位 token → `/chat` Bearer 该 token → 200 正常回复。）

## P0-1　资质接口回归：`resolve_enterprise_qualification` 对所有企业返回 `wechat_page_url=null`
**现象**：企业微信查资质不再返回资质详情页卡片。
**证据**：`POST /wecom/agent/resolve_enterprise_qualification {agent_user_info, enterprise_name}` 现在对所有企业返回 `data.wechat_page_url=null`（中孵高科 画像 id=20 / 百维互联 id=43 / 拓尔思股份 id=16，企业都在库，链接字段全空）。历史记录证明**之前正常**：2026-06-11 08:06 中孵高科、06-11 00:52 拓尔思股份、05-29/05-28 多家都出过 `/pages/qualification/index?id=…` 深链。→ **~06-11 正常、07-01 起全 null = 回归**。
**请查**：资质详情页生成/写库链路、id 映射、或该字段填充逻辑近期改动。恢复返回后卡片自动恢复（网关侧不用动）。详见 `wecom-resolve-qualification-null-regression-2048.md`。

## P0-2　政策匹配意图未转发给 clawops，后端自己回了兑现不了的假承诺
**现象**：用户在企微问政策匹配（如"XX可以做哪些政策""为XX做个政策匹配"），机器人回"正在检索并整理…报告生成大约1-3分钟…主动发送给你"，但报告永不到达。
**证据**：这类消息在 clawops 侧**查无记录**（chat_messages/sop_tasks/日志全无），即**没转发到 clawops**；同期其它意图（如"帮我找记账服务""公司注册"）能正常到达 clawops 并有真实回答。说明后端有自己的意图分流：commodity/一般查询转发，policy-match 却被后端自身"报告生成"流程接管、只回话术不产出。
**请查/改**：把政策匹配意图也转发给 clawops `/chat`（那边 SOP 已能稳定跑完并经 `save_match_result` 推卡片）；在没有真实产出时不要回"报告生成中…主动发送"这类空头承诺。
**注**：先修 P0-0（/chat 鉴权）再验证本条——当前 /chat 全 401，可能"消息到不了 clawops"整体就是 P0-0 导致的；P0-0 修好后若仍有"某些意图不转发"，再按本条处理。

## P1　企业画像接口 `enterprise_profile_sync` 精确匹配，常见名称变体即 404
**现象**：用户少打/错打"股份/集团/（北京）"或用半角括号 → 查不到企业 → 匹配失败。
**证据**：`拓尔思信息技术有限公司`→404，正确"拓尔思信息技术**股份**有限公司"→200(id=16)；`中孵高科有限公司`→404，正确"中孵高科产业孵化（北京）有限公司"→200(id=20)。用户几乎不会一字不差打出工商全称。
**请改**：企业名支持**模糊/前缀匹配**（用"拓尔思信息技术"能命中"…股份有限公司"）。这是"查不到→失败"的根子。（我方已做：名称提取更稳 + 失败时明确提示用户"查不到该企业，请确认名称"。）

## P2　"当前会话人数较多，请稍后再试" 限流会直接丢弃用户消息
**现象**：后端繁忙时对用户连发两条"当前会话人数较多，请稍后再试"，该轮请求被丢弃。
**证据**：该文案不在 clawops 代码里（是后端并发限流发的）。
**请评估**：并发受限时能否**排队/重试**而非直接丢弃用户消息，或至少给出更明确的重试指引，减少"发了没反应"的体感。

---

## 附：我方（clawops/zeroclaw）已完成，供对齐
- `push_message` 接口你们已加 → clawops 已接：SOP 失败/超时时把原因推给企微用户（不再空等）。详见 `wecom-sop-failure-delivery-spec-2048.md`。
- 政策清单 URL 用错(404)自纠正、SOP 任务去重、企业名提取修正、SOP 引擎"中途停手自动续推"、出口守门员拦截内部结构/假异步承诺外泄——均已上线。
- 结论：**只要请求转发到 clawops、且后端 resolve/画像/写库正常，政策与资质匹配即可稳定产出卡片**。当前卡点集中在上面几条后端项。
