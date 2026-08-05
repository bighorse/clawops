# 资质接口回归：resolve_enterprise_qualification 对所有企业返回 wechat_page_url=null — 给 2048 后端

**日期**：2026-07-01

## 现象
企业微信里查企业资质，不再返回资质详情页卡片/链接。例：查"中孵高科产业孵化（北京）有限公司"的资质，机器人回"暂未返回资质详情页链接"，用户拿不到卡片。

## 定位：resolve 接口现在对所有企业都返回 null
`POST https://bdhrapi.2048office.com/wecom/agent/resolve_enterprise_qualification`
body `{"agent_user_info":"uin:...","enterprise_name":"<全称>"}`
现在对**所有**测试企业都返回 `data.wechat_page_url = null`（HTTP 200，但字段为空）：

| 企业 | 企业画像 id | resolve 返回 wechat_page_url |
|---|---|---|
| 中孵高科产业孵化（北京）有限公司 | 20（存在） | **null** |
| 百维互联科技发展（北京）有限公司 | 43（存在） | **null** |
| 拓尔思信息技术股份有限公司 | 16（存在） | **null** |

企业画像（enterprise_profile_sync）对这些企业都能正常返回 id，说明企业本身在库；只是资质详情页链接字段变空了。

## 之前是正常的（有网关侧对话记录佐证）
同一接口此前会返回有效的 `/pages/qualification/index?id=…` 深链，历史记录：
- 2026-06-11 08:06 中孵高科产业孵化（北京）有限公司 → "点击查看详情：/pages/qualification/index?id=…"
- 2026-06-11 00:52 拓尔思信息技术股份有限公司 → 深链
- 2026-05-29 / 2026-05-28 多家企业 → 深链

即 resolve 在 **~2026-06-11 仍正常**，**2026-07-01 起对所有企业返回 null** → 后端回归。

## 请后端排查
为什么 `resolve_enterprise_qualification` 自 ~06-11 之后对所有企业返回 `wechat_page_url=null`？可能方向：资质详情页的生成/写库链路、页面 id 映射、或该字段的填充逻辑近期改动/故障。恢复该字段返回后，企业微信资质卡片即可正常返回。

（网关/机器人侧无需改动——只要 resolve 恢复返回 wechat_page_url，现有流程会自动把它渲染成资质详情卡片。）
