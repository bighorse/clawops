#!/usr/bin/env python3
"""科创企业数据库查询工具。

用法:
    python3 query_enterprise_db.py <公司名>

连接参数通过环境变量:
    MYSQL_HOST  — 数据库地址（默认 127.0.0.1）
    MYSQL_PORT  — 端口（默认 3306）
    MYSQL_USER  — 用户名
    MYSQL_PASSWORD — 密码
    MYSQL_DATABASE — 逻辑库名

输出: JSON（单行）, 结构:
    {"found": true/false,
     "company": "公司全称",
     "region": "所属地区",
     "field": "所属领域",
     "qualification": "科技资质",
     "intro": "公司简介",
     "tech_keywords": "产品技术特征",
     "match_type": "exact"|"fuzzy",
     "note": "附加说明"}

退出码: 0=查询成功, 1=未找到, 2=连接/查询错误
"""

import json
import os
import sys


def main():
    if len(sys.argv) < 2:
        print(json.dumps({"found": False, "error": "缺少参数: 请提供公司名"}))
        sys.exit(1)

    company_name = sys.argv[1].strip()
    if not company_name:
        print(json.dumps({"found": False, "error": "公司名为空"}))
        sys.exit(1)

    host = os.environ.get("MYSQL_HOST", "")
    port = int(os.environ.get("MYSQL_PORT", "0") or "0")
    user = os.environ.get("MYSQL_USER", "")
    password = os.environ.get("MYSQL_PASSWORD", "")
    database = os.environ.get("MYSQL_DATABASE", "")

    # 环境变量不全时读配置文件
    if not all([host, port, user, password, database]):
        for cfg_path in ("state/.mysql.json", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "state", ".mysql.json")):
            if os.path.exists(cfg_path):
                try:
                    with open(cfg_path) as f:
                        cfg = json.load(f)
                    host = host or cfg.get("host", "127.0.0.1")
                    port = port or int(cfg.get("port", 3306))
                    user = user or cfg.get("user", "")
                    password = password or cfg.get("password", "")
                    database = database or cfg.get("database", "")
                    break
                except Exception:
                    pass

    if not all([user, password, database]):
        print(json.dumps({"found": False, "error": "数据库连接参数不完整, 需设 MYSQL_* 环境变量或 state/.mysql.json 配置文件"}))
        sys.exit(2)

    try:
        import pymysql
        conn = pymysql.connect(
            host=host, port=port, user=user,
            password=password, database=database,
            charset="utf8mb4", connect_timeout=10
        )
        cur = conn.cursor()
    except Exception as e:
        print(json.dumps({"found": False, "error": f"数据库连接失败: {e}"}))
        sys.exit(2)

    try:
        # 1) 精确匹配
        cur.execute(
            "SELECT gsmc, ssdq, ssly, kjzz, gsjj, cptz FROM kcqy_zhxx WHERE gsmc = %s LIMIT 5",
            (company_name,)
        )
        rows = cur.fetchall()

        # 2) 模糊匹配（LIKE 前后加 %）
        if not rows:
            like_pattern = f"%{company_name}%"
            cur.execute(
                "SELECT gsmc, ssdq, ssly, kjzz, gsjj, cptz FROM kcqy_zhxx WHERE gsmc LIKE %s LIMIT 10",
                (like_pattern,)
            )
            rows = cur.fetchall()

        # 3) 进一步模糊（拆分关键词后组合匹配）
        if not rows and len(company_name) >= 3:
            keywords = company_name[:4]  # 取前4字做匹配
            cur.execute(
                "SELECT gsmc, ssdq, ssly, kjzz, gsjj, cptz FROM kcqy_zhxx WHERE gsmc LIKE %s LIMIT 10",
                (f"%{keywords}%",)
            )
            rows = cur.fetchall()

        if not rows:
            result = {"found": False, "note": f"数据库中未收录「{company_name}」的相关企业信息"}
        else:
            # 取匹配度最高的一条（第一条）
            row = rows[0]
            match_type = "exact" if row[0] == company_name else "fuzzy"

            result = {
                "found": True,
                "company": row[0] or "",
                "region": row[1] or "",
                "field": row[2] or "",
                "qualification": row[3] or "",
                "intro": row[4] or "",
                "tech_keywords": row[5] or "",
                "match_type": match_type,
                "candidates": len(rows),
                "note": f"匹配类型: {match_type}, 共 {len(rows)} 条候选"
            }

        print(json.dumps(result, ensure_ascii=False))
        sys.exit(0 if result.get("found") else 1)

    except Exception as e:
        print(json.dumps({"found": False, "error": f"查询失败: {e}"}))
        sys.exit(2)
    finally:
        try:
            conn.close()
        except Exception:
            pass


if __name__ == "__main__":
    main()
