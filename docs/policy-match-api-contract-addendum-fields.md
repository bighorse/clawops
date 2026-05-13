# policy-match 接口契约修订 — 字段过滤 (Addendum 2026-05-13)

> **背景**: 2026-05-13 生产 E2E 测试发现:`GET /wecom/policy_summary?no_pagination=true&status=ONLINE` 返回的 100 条政策完整 JSON 约 100-150 KB,直接进入 LLM context 导致 deepseek-v4-pro 在第 11 次调用时偶发 hang(140K input tokens,服务侧无响应)。
>
> **结论**: SOP.md.hbs 里"LLM 落盘前精简"的约束只能控制 file_write 内容,**无法阻止 API 响应原本就进入 LLM 当前 message context**。要根本解决,需后端在 endpoint 上支持**字段裁剪**,把响应体砍到 30-40 KB。

---

## 改动范围

**只改一个 endpoint**: `GET /wecom/policy_summary`

新增一个 query 参数 `fields`,语义是"只返回指定字段(逗号分隔),其他字段一律省略"。

## Query 参数

| 名 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `fields` | string | ❌ | 逗号分隔的字段白名单。**不传 / 留空 = 返回所有字段**(向后兼容,小程序前端的现有请求不受影响) |

支持的字段值(SOP 在 step 2 实际会传):

```
fields=id,name,policy_direction,policy_belong,application_condition,application_at,application_date_at,support_money,support_decimal_money,link,status
```

## 调用示例

```bash
curl "https://bdhrapi.2048office.com/wecom/policy_summary?\
no_pagination=true&\
status=ONLINE&\
order_by_fields=is_top&\
order_by_types=DESC&\
fields=id,name,policy_direction,policy_belong,application_condition,application_at,application_date_at,support_money,support_decimal_money,link,status"
```

## 期望响应

跟现在一致的包络 + 分页结构,只是 `data.data[]` 里每条对象**只含 query 里 `fields` 指定的字段**:

```json
{
  "request_id": "...",
  "message": null,
  "data": {
    "page_index": 1,
    "page_size": 200,
    "data_count": 100,
    "data": [
      {
        "id": 97,
        "name": "信息软件企业行业模型首方案",
        "policy_direction": "...",
        "policy_belong": "...",
        "application_condition": "...",
        "application_at": "...",
        "application_date_at": "...",
        "support_money": "...",
        "support_decimal_money": 100.0,
        "link": "...",
        "status": "ONLINE"
      }
    ]
  }
}
```

**禁止保留的字段**(SOP 不需要,且体积大易撑爆 context):

- `description` — 摘要,常含 HTML 标签,数百字
- `sponsor` / `department` / `contact_information` — 发文单位,匹配无关
- `application_method` / `application_material` — 申报方式/材料,step 1-6 都不用
- `policy_basis` — 政策依据,匹配无关
- `published_at` / `created_at` / `updated_at` — 时间戳,匹配无关

## 实现提示(可选)

如果后端用 SQLModel + `SQLModelCRUDRouter`,字段过滤典型实现:

```python
# query 参数
fields: Optional[str] = Query(None, description="逗号分隔的字段白名单")

# 序列化时按 fields 裁剪
if fields:
    allowed = set(f.strip() for f in fields.split(",") if f.strip())
    data = [
        {k: v for k, v in row.dict().items() if k in allowed}
        for row in rows
    ]
else:
    data = [row.dict() for row in rows]
```

或者用 FastAPI 的 `response_model_include` 动态注入。

## 期望效果

| 指标 | 现状(无 fields) | 改后(fields=11 字段) |
|---|---|---|
| 响应体大小 (100 条) | 100-150 KB | **20-30 KB** |
| LLM 看到的 input_tokens | 140K(撑爆) | **40-50K**(安全) |
| 4xx/5xx | 不应变化 | 不应变化 |
| 向后兼容 | — | 不传 fields = 行为不变 |

## SOP 端配合

后端上线后,clawops 这边把 [SOP.md.hbs](../templates/workspace/sops/policy-match/SOP.md.hbs) **step 2** 调用 URL 改成附 `&fields=...`,然后 `git push` + 服务器 `git pull` + `clawops refresh-workspace --all`(或新 provision 用户自动用新模板) 即可生效。**无需 backend 强制版本绑定**,不传 fields 时旧行为保留。

## smoke test

后端上线后跑一次:

```bash
# 1. 不传 fields → 完整字段(向后兼容)
curl "https://bdhrapi.2048office.com/wecom/policy_summary?page_size=1" | jq '.data.data[0] | keys'
# 期望: 输出全部字段名(包括 description, sponsor, ...)

# 2. 传 fields → 只返指定
curl "https://bdhrapi.2048office.com/wecom/policy_summary?page_size=1&fields=id,name,policy_direction" | jq '.data.data[0]'
# 期望: 只有 id / name / policy_direction 三个 key
```

两个都通过 = 后端 done,clawops 这边改 SOP.md.hbs 即可。

## 优先级

**P0** — 不上线这条,policy-match SOP 在 deepseek-v4-pro 下基本不可用(140K context 必 hang)。
