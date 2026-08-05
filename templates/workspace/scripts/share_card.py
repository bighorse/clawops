#!/usr/bin/env python3
"""企业快评分享卡片生成器。

用法:
    python3 share_card.py \
        --company "公司全称" \
        --tagline "一句话定位" \
        --p1 "第一条重点" \
        --p2 "第二条重点" \
        --p3 "第三条重点" \
        --out "输出路径.png"

依赖: pip install pillow qrcode
字体: 需系统安装中文字体（如 wqy-microhei），路径见下方 FONT_PATH 常量。
"""

import argparse
import textwrap
import qrcode
from PIL import Image, ImageDraw, ImageFont

# ── 可调常量 ──────────────────────────────────────────────
# 中文字体路径：按服务器实际字体位置修改
FONT_PATH = "scripts/fonts/wqy-microhei.ttc"
FONT_PATH_BOLD = "scripts/fonts/wqy-microhei.ttc"

# 画布尺寸
WIDTH = 750
HEIGHT = 1000

# 色板
COLOR_DARK_BG = (30, 33, 40)        # 深色头部 / 底部
COLOR_ACCENT = (56, 139, 253)       # 强调色（序号圆点、链接文字）
COLOR_WHITE = (255, 255, 255)
COLOR_LIGHT_GRAY = (200, 201, 204)
COLOR_MID_GRAY = (140, 143, 148)
COLOR_CARD_BG = (248, 249, 250)     # 中部底色
COLOR_DIVIDER = (220, 222, 226)
COLOR_COMPLIANCE = (160, 163, 168)  # 合规条文字色

# 布局区域（y 坐标）
HEADER_Y_START = 0
HEADER_HEIGHT = 240
BODY_Y_START = HEADER_HEIGHT
BODY_HEIGHT = 520
FOOTER_Y_START = HEADER_Y_START + HEADER_HEIGHT + BODY_HEIGHT  # 760
FOOTER_HEIGHT = HEIGHT - FOOTER_Y_START  # 240
# ───────────────────────────────────────────────────────────


def load_fonts():
    """加载字体，找不到时抛出明确错误。"""
    try:
        font_title = ImageFont.truetype(FONT_PATH_BOLD, 38)
        font_company = ImageFont.truetype(FONT_PATH_BOLD, 28)
        font_tagline = ImageFont.truetype(FONT_PATH, 22)
        font_subtitle = ImageFont.truetype(FONT_PATH_BOLD, 18)
        font_body = ImageFont.truetype(FONT_PATH, 21)
        font_compliance = ImageFont.truetype(FONT_PATH, 16)
        font_cta = ImageFont.truetype(FONT_PATH_BOLD, 20)
        font_small = ImageFont.truetype(FONT_PATH, 15)
        return font_title, font_company, font_tagline, font_subtitle, font_body, font_compliance, font_cta, font_small
    except OSError:
        raise SystemExit(
            f"找不到中文字体文件。请确认字体路径正确，或 pip install pillow 后安装中文字体。\n"
            f"当前字体路径: {FONT_PATH}\n"
            f"建议: apt install fonts-wqy-microhei 或 yum install wqy-microhei-fonts"
        )


def wrap_text(draw, text, font, max_width):
    """按像素宽度自动换行，返回行列表。"""
    lines = []
    for paragraph in text.split("\n"):
        if not paragraph:
            lines.append("")
            continue
        words = list(paragraph)
        current_line = ""
        for char in words:
            test_line = current_line + char
            bbox = draw.textbbox((0, 0), test_line, font=font)
            if bbox[2] - bbox[0] > max_width:
                if current_line:
                    lines.append(current_line)
                current_line = char
            else:
                current_line = test_line
        if current_line:
            lines.append(current_line)
    return lines


def generate_card(company, tagline, p1, p2, p3, qr_content, out_path):
    """生成 750x1000 竖版分享卡片。"""
    fonts = load_fonts()
    font_title, font_company, font_tagline, font_subtitle, font_body, font_compliance, font_cta, font_small = fonts

    img = Image.new("RGB", (WIDTH, HEIGHT), COLOR_CARD_BG)
    draw = ImageDraw.Draw(img)

    # ═══════════════════════════════════════════
    # 头部（深色背景）
    # ═══════════════════════════════════════════
    draw.rectangle([(0, HEADER_Y_START), (WIDTH, HEADER_Y_START + HEADER_HEIGHT)], fill=COLOR_DARK_BG)

    # 产品名（小字，左上）
    product_label = "ZeroClaw · 企业快评"
    draw.text((40, 30), product_label, font=font_small, fill=COLOR_MID_GRAY)

    # 分隔线（装饰）
    draw.rectangle([(40, 58), (WIDTH - 40, 60)], fill=COLOR_ACCENT)

    # 公司名（大字）
    draw.text((40, 80), company, font=font_company, fill=COLOR_WHITE)

    # 一句话定位
    tagline_wrapped = wrap_text(draw, tagline, font_tagline, WIDTH - 80)
    y_tagline = 130
    for line in tagline_wrapped:
        draw.text((40, y_tagline), line, font=font_tagline, fill=COLOR_LIGHT_GRAY)
        y_tagline += 30

    # ═══════════════════════════════════════════
    # 中部（三条要点，浅色背景）
    # ═══════════════════════════════════════════
    body_y = BODY_Y_START + 30
    draw.text((40, body_y), "关键速览", font=font_subtitle, fill=COLOR_DARK_BG)
    body_y += 40

    points = [p1, p2, p3]
    for idx, point in enumerate(points, 1):
        # 序号圆点
        circle_x, circle_y = 50, body_y + 6
        draw.ellipse([(circle_x, circle_y), (circle_x + 24, circle_y + 24)], fill=COLOR_ACCENT)
        draw.text((circle_x + 8, circle_y + 2), str(idx), font=font_small, fill=COLOR_WHITE, anchor="ma")

        # 内容文字
        point_lines = wrap_text(draw, point, font_body, WIDTH - 120)
        text_y = body_y
        for line in point_lines:
            draw.text((90, text_y), line, font=font_body, fill=COLOR_DARK_BG)
            text_y += 32

        body_y += max(len(point_lines) * 32 + 12, 50)

        # 分隔线
        if idx < 3:
            draw.rectangle([(60, body_y), (WIDTH - 60, body_y + 1)], fill=COLOR_DIVIDER)
            body_y += 24

    # 合规声明条
    compliance_y = BODY_Y_START + BODY_HEIGHT - 50
    compliance_text = "公开信息摘要 · 不构成投资建议"
    bbox = draw.textbbox((0, 0), compliance_text, font=font_compliance)
    tw = bbox[2] - bbox[0]
    draw.text(((WIDTH - tw) / 2, compliance_y), compliance_text, font=font_compliance, fill=COLOR_COMPLIANCE)

    # ═══════════════════════════════════════════
    # 底部（深色，CTA + 二维码）
    # ═══════════════════════════════════════════
    draw.rectangle([(0, FOOTER_Y_START), (WIDTH, HEIGHT)], fill=COLOR_DARK_BG)

    # 行动号召文字
    cta_text = "想查哪家公司？跟分析师说一声就行"
    bbox = draw.textbbox((0, 0), cta_text, font=font_cta)
    cta_w = bbox[2] - bbox[0]
    draw.text(((WIDTH - cta_w) / 2, FOOTER_Y_START + 30), cta_text, font=font_cta, fill=COLOR_WHITE)

    # 副标题
    sub_text = "在对话中发送「快评：公司名」即可发起"
    bbox = draw.textbbox((0, 0), sub_text, font=font_small)
    sub_w = bbox[2] - bbox[0]
    draw.text(((WIDTH - sub_w) / 2, FOOTER_Y_START + 65), sub_text, font=font_small, fill=COLOR_MID_GRAY)

    # 生成二维码
    qr = qrcode.QRCode(box_size=4, border=2)
    qr.add_data(qr_content)
    qr.make(fit=True)
    qr_img = qr.make_image(fill_color="white", back_color=COLOR_DARK_BG).convert("RGB")

    # 二维码缩放（约 120x120）
    qr_size = 120
    qr_img = qr_img.resize((qr_size, qr_size), Image.LANCZOS)

    # 二维码背景框
    qr_x = (WIDTH - qr_size - 16) // 2
    qr_y = FOOTER_Y_START + 100
    draw.rectangle([(qr_x - 1, qr_y - 1), (qr_x + qr_size + 1, qr_y + qr_size + 1)], outline=COLOR_ACCENT, width=1)
    img.paste(qr_img, (qr_x, qr_y))

    # 二维码下方说明文字
    qr_label = "扫码与我对话"
    bbox = draw.textbbox((0, 0), qr_label, font=font_small)
    qr_label_w = bbox[2] - bbox[0]
    draw.text(((WIDTH - qr_label_w) / 2, qr_y + qr_size + 10), qr_label, font=font_small, fill=COLOR_MID_GRAY)

    # 保存
    img.save(out_path, "PNG")
    print(f"卡片已生成: {out_path}")


def main():
    parser = argparse.ArgumentParser(description="企业快评分享卡片生成器")
    parser.add_argument("--company", required=True, help="公司全称")
    parser.add_argument("--tagline", required=True, help="一句话定位（≤28 字）")
    parser.add_argument("--p1", required=True, help="第一条重点（≤60 字）")
    parser.add_argument("--p2", required=True, help="第二条重点（≤60 字）")
    parser.add_argument("--p3", required=True, help="第三条重点（≤60 字）")
    parser.add_argument("--qr", default="https://example.com", help="二维码内容（URL 或文本）")
    parser.add_argument("--out", required=True, help="输出 PNG 路径")
    args = parser.parse_args()

    # 字数校验
    if len(args.tagline) > 28:
        print(f"⚠️ 警告：定位文案 {len(args.tagline)} 字，建议 ≤28 字")
    for label, text in [("p1", args.p1), ("p2", args.p2), ("p3", args.p3)]:
        if len(text) > 60:
            print(f"⚠️ 警告：{label} {len(text)} 字，建议 ≤60 字")
        if any(kw in text for kw in ["分享得", "转发领", "邀请返", "扫码领", "注册送"]):
            raise SystemExit(f"❌ 错误：{label} 文案含禁止的利诱型表述，请修改后重试。")

    generate_card(args.company, args.tagline, args.p1, args.p2, args.p3, args.qr, args.out)


if __name__ == "__main__":
    main()
