#!/usr/bin/env python3
"""企业智能搜索工具 —— 从自然语言查询中提取关键词，在科创企业数据库中多字段搜索并评分排序。

用法:
    python3 search_enterprise.py "<自然语言查询>"

连接参数通过环境变量:
    MYSQL_HOST / MYSQL_PORT / MYSQL_USER / MYSQL_PASSWORD / MYSQL_DATABASE

输出: JSON 数组，每个结果含:
    company, region, field, qualification, funding_status, intro, tech_keywords,
    match_score, match_fields, match_tokens, detail_url
"""

import json
import os
import re
import sys

# 常见无意义连接词（会被过滤掉，不参与搜索）
STOP_WORDS = {
    "的", "了", "在", "是", "有", "和", "与", "或", "及", "我", "你", "他", "她",
    "它", "们", "这", "那", "个", "些", "什么", "哪", "怎么", "一个", "一些",
    "企业", "公司", "有限", "科技", "技术", "有限公司", "帮我", "查找", "搜索",
    "找", "查", "一下", "一家", "哪些", "有没有", "有什么", "请", "帮忙",
}

# 字段权重：公司名最重要，行业/资质次之，简介最低
FIELD_WEIGHT = {
    "gsmc": 5,
    "ssly": 4,
    "kjzz": 3,
    "rzzt": 3,
    "cptz": 3,
    "ssdq": 2,
    "gsjj": 1,
}


def tokenize(query: str) -> list[str]:
    """把自然语言查询拆成有意义的搜索词。"""
    # 把停用词和连接词都替换为空格，切开复合词
    for sep in ("的", "和", "与", "或", "在", "是", "有", "及", "找", "查", "搜", "做",
                "请", "帮我", "一下", "哪些", "有没有", "有什么", "有没有做", "做什么",
                "公司", "企业", "有限", "有限公司"):
        query = query.replace(sep, " ")
    # 去标点
    parts = re.split(r"[，,。\.、；;：:！!？?\s（）()【】\[\]《》\"'「」『』—\-/]+", query)
    tokens = []
    for part in parts:
        part = part.strip()
        if not part or len(part) < 2:
            continue
        tokens.append(part)
    # 去重
    seen = set()
    unique = []
    for t in tokens:
        if t not in seen:
            seen.add(t)
            unique.append(t)
    return unique


def search(db_config: dict, query: str) -> list[dict]:
    """执行多字段搜索并评分。"""
    tokens = tokenize(query)
    if not tokens:
        return []

    import pymysql
    conn = pymysql.connect(
        host=db_config["host"], port=db_config["port"],
        user=db_config["user"], password=db_config["password"],
        database=db_config["database"],
        charset="utf8mb4", connect_timeout=10,
    )
    cur = conn.cursor()

    # 为每个 token 构建跨字段 OR 条件，再 token 之间 AND
    # 策略：每个 token 必须至少在某个字段命中
    conditions = []
    params = []
    search_fields = ["gsmc", "ssdq", "ssly", "kjzz", "rzzt", "gsjj", "cptz"]
    for token in tokens:
        field_conds = []
        for f in search_fields:
            field_conds.append(f"{f} LIKE %s")
            params.append(f"%{token}%")
        conditions.append("(" + " OR ".join(field_conds) + ")")

    where = " AND ".join(conditions)
    # 限制返回量 + 设置查询超时防止慢查询
    sql = f"SELECT gsmc, ssdq, ssly, kjzz, rzzt, gsjj, cptz FROM kcqy_zhxx WHERE {where} LIMIT 200"

    try:
        cur.execute("SET SESSION max_execution_time = 5000")  # 5秒超时
        cur.execute(sql, params)
        rows = cur.fetchall()
    except Exception as e:
        conn.close()
        return []

    # 对每条结果按字段匹配情况评分
    scored = []
    for row in rows:
        gsmc, ssdq, ssly, kjzz, rzzt, gsjj, cptz = row
        row_data = {
            "gsmc": gsmc or "",
            "ssdq": ssdq or "",
            "ssly": ssly or "",
            "kjzz": kjzz or "",
            "rzzt": rzzt or "",
            "gsjj": gsjj or "",
            "cptz": cptz or "",
        }

        score = 0
        matched_fields = set()
        matched_tokens = set()

        for token in tokens:
            token_lower = token.lower()
            for field, value in row_data.items():
                if token_lower in (value or "").lower():
                    score += FIELD_WEIGHT.get(field, 1)
                    matched_fields.add(field)
                    matched_tokens.add(token)

        # 额外加分：匹配字段越多越好
        score += len(matched_fields) * 2
        # 公司名匹配额外权重
        if "gsmc" in matched_fields:
            score += 5

        if score > 0:
            scored.append({
                "company": gsmc or "",
                "region": ssdq or "",
                "field": ssly or "",
                "qualification": kjzz or "",
                "funding_status": rzzt or "",
                "intro": (gsjj or "")[:200],
                "tech_keywords": cptz or "",
                "match_score": score,
                "match_fields": sorted(matched_fields),
                "match_tokens": sorted(matched_tokens),
            })

    conn.close()

    # 按分数降序，返回全部匹配结果（最多 50 条）
    scored.sort(key=lambda x: x["match_score"], reverse=True)
    return scored[:50]


def generate_reason(item: dict, query_tokens: list[str]) -> str:
    """根据匹配情况生成推荐理由。"""
    reasons = []
    if "gsmc" in item["match_fields"]:
        reasons.append("公司名匹配")
    if item["field"] and "ssly" in item["match_fields"]:
        reasons.append(f"所属领域包含相关关键词")
    if item["qualification"] and "kjzz" in item["match_fields"]:
        qual = item["qualification"]
        if len(qual) > 30:
            qual = qual[:30] + "..."
        reasons.append(f"具备相关资质")
    if item["funding_status"] and "rzzt" in item["match_fields"]:
        reasons.append(f"融资状态匹配")
    if item["region"] and "ssdq" in item["match_fields"]:
        reasons.append(f"所在地：{item['region']}")
    if "cptz" in item["match_fields"]:
        reasons.append("技术方向匹配")
    if "gsjj" in item["match_fields"] and "gsmc" not in item["match_fields"]:
        reasons.append("公司简介匹配")

    if not reasons:
        reasons.append(f"综合匹配 {len(item['match_tokens'])} 个关键词")

    return "；".join(reasons)


def load_db_config() -> dict:
    """加载数据库配置：优先环境变量，其次 workspace/state/.mysql.json"""
    config = {
        "host": os.environ.get("MYSQL_HOST", ""),
        "port": int(os.environ.get("MYSQL_PORT", "0") or "0"),
        "user": os.environ.get("MYSQL_USER", ""),
        "password": os.environ.get("MYSQL_PASSWORD", ""),
        "database": os.environ.get("MYSQL_DATABASE", ""),
    }
    if all([config["host"], config["port"], config["user"], config["password"], config["database"]]):
        return config

    # 环境变量不全，尝试读配置文件
    cfg_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "state", ".mysql.json")
    if not os.path.exists(cfg_path):
        cfg_path = "state/.mysql.json"
    if os.path.exists(cfg_path):
        try:
            with open(cfg_path) as f:
                file_cfg = json.load(f)
            config["host"] = config["host"] or file_cfg.get("host", "")
            config["port"] = config["port"] or file_cfg.get("port", 0)
            config["user"] = config["user"] or file_cfg.get("user", "")
            config["password"] = config["password"] or file_cfg.get("password", "")
            config["database"] = config["database"] or file_cfg.get("database", "")
        except Exception:
            pass
    return config


def main():
    if len(sys.argv) < 2:
        print(json.dumps({"error": "请提供搜索查询，如：python3 search_enterprise.py '深圳的人工智能公司'"}, ensure_ascii=False))
        sys.exit(1)

    query = sys.argv[1].strip()
    if not query:
        print(json.dumps({"error": "查询为空"}, ensure_ascii=False))
        sys.exit(1)

    db_config = load_db_config()
    if not all([db_config["user"], db_config["password"], db_config["database"]]):
        print(json.dumps({"error": "数据库连接参数不完整，请检查 state/.mysql.json"}, ensure_ascii=False))
        sys.exit(2)

    results = search(db_config, query)
    tokens = tokenize(query)

    if not results:
        print(json.dumps({
            "query": query,
            "tokens": tokens,
            "total": 0,
            "results": [],
            "note": "未找到匹配企业，试试缩短关键词或换一种说法",
        }, ensure_ascii=False))
        sys.exit(0)

    output = []
    for item in results:
        output.append({
            "company": item["company"],
            "region": item["region"],
            "field": item["field"],
            "qualification": item["qualification"],
            "funding_status": item["funding_status"],
            "intro": item["intro"],
            "tech_keywords": item["tech_keywords"],
            "match_score": item["match_score"],
            "reason": generate_reason(item, tokens),
            # 占位链接：有了小程序详情页 URL 模板后替换
            "detail_url": f"【小程序详情页·待配置】?company={item['company']}",
        })

    print(json.dumps({
        "query": query,
        "tokens": tokens,
        "total": len(output),
        "results": output,
    }, ensure_ascii=False))
    sys.exit(0)


if __name__ == "__main__":
    main()
