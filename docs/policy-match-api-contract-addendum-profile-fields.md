# policy-match 接口契约修订 — enterprise_profile_sync 字段裁剪 (Addendum 2026-05-13)

> **背景**: E2E 测试（2026-05-13）发现 `GET /wecom/agent/enterprise_profile_sync` 响应约 **91 KB**，全量塞入 LLM context 后，context 达 70K tokens，后续 step 2 policy_summary 调用（28 KB）再加 7K tokens，总计 ~77K tokens，导致 deepseek-v4-pro 在 LLM call 阶段持续 hang（provider_timeout 未能中断，永久卡死）。
>
> **根因**: `bocha_info` 字段为博查/百度三方搜索爬取的网页数据，约 60-70 KB，对 policy-match 业务无实质作用（政策申报依据官方注册信息，与 `basic_info` + `qualification_info` 对齐即可）。
>
> **结论**: 与 `/wecom/policy_summary` 同样的方案 — 在 endpoint 上支持 `fields` query 参数，让 SOP step 1 只拉取必要字段，把响应从 91 KB 压到 ~20 KB。

---

## 改动范围

**只改一个 endpoint**: `GET /wecom/agent/enterprise_profile_sync`

新增一个 query 参数 `fields`，语义与 `/wecom/policy_summary?fields=...` 完全一致：只返回指定字段，其他字段一律省略。

## Query 参数

| 名 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `fields` | string | ❌ | 逗号分隔的字段白名单。**不传 / 留空 = 返回所有字段**（向后兼容，现有调用不受影响） |

SOP step 1 实际会传：

```
fields=enterprise_id,qualification_enterprise_id,enterprise_name,basic_info,qualification_info
```

## 调用示例

```bash
curl "https://bdhrapi.2048office.com/wecom/agent/enterprise_profile_sync?\
enterprise_name=拓尔思信息技术股份有限公司&\
agent_user_info=some_openid&\
fields=enterprise_id,qualification_enterprise_id,enterprise_name,basic_info,qualification_info"
```

## 期望响应

与现在一致的包络结构，只是 `data` 对象**只含 `fields` 指定的字段**：

```json
{
  "request_id": "...",
  "message": null,
  "data": {
    "enterprise_id": 16,
    "qualification_enterprise_id": 98,
    "enterprise_name": "拓尔思信息技术股份有限公司",
    "basic_info": "企业详细的基本信息如下:\n企业名称: ...",
    "qualification_info": "..."
  }
}
```

**排除的字段**（不传 fields 时仍返回，向后兼容）：

- `bocha_info` — 博查/百度爬取的三方搜索数据，约 60-70 KB，policy-match 不依赖

## 实现提示（可选）

与 `/wecom/policy_summary` 实现方式相同：

```python
fields: Optional[str] = Query(None)

if fields:
    allowed = set(f.strip() for f in fields.split(",") if f.strip())
    data = {k: v for k, v in result.dict().items() if k in allowed}
else:
    data = result.dict()
```

## 期望效果

| 指标 | 现状（无 fields） | 改后（fields=5 字段） |
|---|---|---|
| 响应体大小 | 91 KB | **~20 KB** |
| step 1 后 LLM input_tokens | ~66K | **~49K** |
| step 2 后 LLM input_tokens | ~77K（hang） | **~56K**（安全） |
| 向后兼容 | — | 不传 fields = 行为不变 |

## SOP 端配合

后端上线后，clawops 这边把 [SOP.md.hbs](../templates/workspace/sops/policy-match/SOP.md.hbs) **step 1** 的调用 URL 改成附 `&fields=...`：

```
http_request GET https://bdhrapi.2048office.com/wecom/agent/enterprise_profile_sync
  ?enterprise_name=<enterprise_name>
  &agent_user_info=<openid>
  &fields=enterprise_id,qualification_enterprise_id,enterprise_name,basic_info,qualification_info
```

然后 `git push` + 服务器 `git pull` + `clawops refresh-workspace --all` 生效。

## smoke test

```bash
# 1. 不传 fields → 完整字段（向后兼容）
curl "https://bdhrapi.2048office.com/wecom/agent/enterprise_profile_sync?enterprise_name=拓尔思信息技术股份有限公司&agent_user_info=test" \
  | jq '.data | keys'
# 期望: 输出全部字段名（包括 bocha_info）

# 2. 传 fields → 只返指定
curl "https://bdhrapi.2048office.com/wecom/agent/enterprise_profile_sync?enterprise_name=拓尔思信息技术股份有限公司&agent_user_info=test&fields=enterprise_id,enterprise_name,basic_info,qualification_info" \
  | jq '.data | keys'
# 期望: ["basic_info", "enterprise_id", "enterprise_name", "qualification_info"]

# 3. 响应大小 < 25 KB
curl "https://bdhrapi.2048office.com/wecom/agent/enterprise_profile_sync?enterprise_name=拓尔思信息技术股份有限公司&agent_user_info=test&fields=enterprise_id,qualification_enterprise_id,enterprise_name,basic_info,qualification_info" \
  | wc -c
# 期望: < 25000
```

三个都通过 = 后端 done，clawops 这边改 SOP.md.hbs step 1 即可。

## 优先级

**P0** — 不上线这条，policy-match SOP 在 deepseek-v4-pro 下基本不可用（step 1 profile 响应 91 KB 把 context 推到 70K，step 2 再加 28 KB 就 hang）。
