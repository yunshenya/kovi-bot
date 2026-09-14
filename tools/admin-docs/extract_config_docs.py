#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""从源码与示例配置里机械提取「配置项 -> 说明」元数据。

管理后端的配置页需要给每个字段一句人话说明。说明有两个权威来源：

1. `plugins/model/src/config/*.rs` 里结构体字段上方的 `///` 文档注释（字段级，
   覆盖最全）；
2. `bot.conf.example.toml` 里字段上方的 `#` 注释（运维视角，常常写着
   "为什么是这个值"，并标出需要重启的项）。

两个来源都不需要人工转抄，所以这个脚本只做机械提取，产出
`plugins/model/src/admin/config_docs.json`，由 Rust 侧 `include_str!` 内嵌。
手写说明容易和代码漂移；重新跑一次脚本就能跟上。

用法：
    python3 tools/admin-docs/extract_config_docs.py

字段数低于阈值时以非 0 退出，提示提取逻辑没跟上代码改动。
"""

from __future__ import annotations

import json
import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CONFIG_DIR = os.path.join(REPO_ROOT, "plugins", "model", "src", "config")
EXAMPLE_TOML = os.path.join(REPO_ROOT, "bot.conf.example.toml")
OUTPUT = os.path.join(REPO_ROOT, "plugins", "model", "src", "admin", "config_docs.json")

# 结构体 -> TOML 分区路径。这份映射是配置树的形状本身（`ModelConfig` 的字段名 +
# serde rename），只能人工维护；字段说明则全部由脚本提取。
STRUCT_TO_SECTION = {
    "IdentityConfig": "identity",
    "Prompt": "prompt",
    "ServerConfig": "server_config",
    "ProactiveConfig": "proactive",
    "GroupInterjectionConfig": "group_interjection",
    "MemoryConfig": "memory",
    "MindConfig": "mind",
    "MessageBatchConfig": "message_batch",
    "MoodConfig": "mood",
    "TopicConfig": "topic",
    "TrafficConfig": "traffic",
    "ToolsConfig": "tools",
    "McpServerConfig": "tools.mcp_servers[]",
    "ReminderConfig": "reminders",
    "AgentTaskConfig": "agent_tasks",
    "AgentRunConfig": "agent_runs",
    "WorldSensorsConfig": "world_sensors",
    "WorldSensorConfig": "world_sensors.sensors[]",
    "WorldModelConfig": "world_model",
    "GagLedgerConfig": "gag_ledger",
    "VisionConfig": "vision",
    "QqCallConfig": "qq_call",
    "QqVoiceConfig": "qq_voice",
    "QqSingConfig": "qq_sing",
    "QqStickerConfig": "qq_sticker",
    "ExecutiveConfig": "executive",
    "ExecutiveConflictConfig": "executive.conflict",
    "ExecutiveConfidenceConfig": "executive.confidence",
    "ExecutiveAttentionBudgetConfig": "executive.attention_budget",
    "ExecutivePlanConfig": "executive.plan",
    "ExecutiveExpectationConfig": "executive.expectation",
    "ExecutiveDecisionRecordConfig": "executive.decision_record",
    "CognitiveModelConfig": "model",
    "IntrinsicConfig": "model.intrinsic",
    "ModelFallbackConfig": "model.fallback",
    "TurnGateConfig": "model.turn_gate",
    "AdminConfig": "admin",
}

# 这些字段是密钥或等价物：界面上默认打码，留空写入表示"不改"。
SECRET_FIELDS = {
    "server_config.actor_authorization",
    "qq_call.pulse_cookie",
    "admin.token",
}

# 枚举字段的合法取值，用于把输入框渲染成下拉框。值来自各结构体的 validate()。
ENUM_FIELDS = {
    "server_config.wire_api": ["chat_completions", "responses"],
    "server_config.thinking_mode": ["auto", "disabled"],
    "vision.provider": ["disabled", "intrinsic", "auto", "builtin", "mcp"],
    "world_model.mode": ["disabled", "shadow", "active"],
    "model.turn_gate.mode": ["disabled", "shadow", "active"],
    "model.turn_gate.response_mode": ["disabled", "shadow", "active"],
}

STRUCT_RE = re.compile(r"^pub struct ([A-Za-z0-9_]+)\b.*\{\s*$")
FIELD_RE = re.compile(r"^\s*([a-z_][a-z0-9_]*)\s*:\s*.+,\s*$")
DOC_RE = re.compile(r"^\s*///\s?(.*)$")
TOML_SECTION_RE = re.compile(r"^\[([A-Za-z0-9_.]+)\]\s*$")
TOML_KEY_RE = re.compile(r"^([a-z_][a-z0-9_]*)\s*=")
COMMENTED_KEY_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*\s*[=:]")


def clean(doc_lines):
    """把多行注释压成一句可读的说明。"""
    parts = [line.strip() for line in doc_lines if line.strip()]
    return re.sub(r"\s+", " ", " ".join(parts)).strip()


def parse_source_structs():
    """返回 {结构体名: {"doc": str, "fields": {字段: 说明}}}。

    依赖 rustfmt 的稳定排版：`pub struct X {` 顶格，字段缩进四格，`}` 顶格。
    这样不需要数花括号（字段类型里的 `BTreeMap<String, String>` 本来也不含
    花括号，但简单规则更不容易被将来的嵌套类型绊倒）。
    """
    structs = {}
    for name in sorted(os.listdir(CONFIG_DIR)):
        if not name.endswith(".rs"):
            continue
        with open(os.path.join(CONFIG_DIR, name), "r", encoding="utf-8") as handle:
            lines = handle.readlines()

        pending_doc = []
        struct_doc = []
        current = None

        for raw in lines:
            line = raw.rstrip("\n")
            doc = DOC_RE.match(line)

            if current is None:
                if doc:
                    pending_doc.append(doc.group(1))
                    continue
                matched = STRUCT_RE.match(line)
                if matched:
                    struct_name = matched.group(1)
                    struct_doc = pending_doc
                    pending_doc = []
                    current = struct_name
                    structs.setdefault(struct_name, {"doc": "", "fields": {}})
                    structs[struct_name]["doc"] = clean(struct_doc)
                    continue
                if line.strip() and not line.lstrip().startswith("#["):
                    # 属性行（derive/serde）与结构体文档之间不隔着空行，
                    # 遇到属性要保留已积累的文档注释。
                    pending_doc = []
                continue

            # 结构体内部
            if doc:
                pending_doc.append(doc.group(1))
                continue
            if line.startswith("}"):
                current = None
                pending_doc = []
                continue
            field = FIELD_RE.match(line)
            if field:
                structs[current]["fields"][field.group(1)] = clean(pending_doc)
                pending_doc = []
                continue
            if line.strip() and not line.strip().startswith("#"):
                # 属性行（#[serde(...)]）或字段类型续行：保留已积累的文档。
                continue

    return structs


def parse_example_toml():
    """返回 ({分区.字段: 注释}, {分区: 分区说明})。"""
    hints = {}
    section_hints = {}
    if not os.path.exists(EXAMPLE_TOML):
        return hints, section_hints

    with open(EXAMPLE_TOML, "r", encoding="utf-8") as handle:
        lines = handle.readlines()

    section = ""
    pending = []
    for raw in lines:
        stripped = raw.strip()

        if stripped.startswith("#"):
            text = stripped.lstrip("#").strip()
            if text and not COMMENTED_KEY_RE.match(text):
                pending.append(text)
            continue

        if stripped == "":
            # 空行切断注释块：注释只贴着它下面那一项。
            pending = []
            continue

        header = TOML_SECTION_RE.match(stripped)
        if header:
            # 紧贴分区头的注释块描述的是这个分区本身。
            section = header.group(1)
            if pending:
                section_hints[section] = clean(pending)
            pending = []
            continue

        key = TOML_KEY_RE.match(stripped)
        if key and pending:
            hints["%s.%s" % (section, key.group(1))] = clean(pending)
        pending = []

    return hints, section_hints


def main():
    structs = parse_source_structs()
    hints, section_hints = parse_example_toml()

    if not structs:
        print("提取失败：没有解析到任何配置结构体", file=sys.stderr)
        return 1

    fields = {}
    sections = {}
    missing = []
    for struct_name, section in sorted(STRUCT_TO_SECTION.items()):
        body = structs.get(struct_name)
        if body is None:
            missing.append(struct_name)
            continue
        if body["doc"]:
            sections[section] = body["doc"]
        for field_name, doc in sorted(body["fields"].items()):
            path = "%s.%s" % (section, field_name)
            entry = {}
            if doc:
                entry["doc"] = doc
            hint = hints.get(path)
            if hint and hint != doc:
                entry["hint"] = hint
            if path in SECRET_FIELDS:
                entry["secret"] = True
            if path in ENUM_FIELDS:
                entry["options"] = ENUM_FIELDS[path]
            if entry:
                fields[path] = entry

    for section, hint in sorted(section_hints.items()):
        if hint and section not in sections:
            sections[section] = hint

    payload = {
        "generated_from": [
            "plugins/model/src/config/*.rs",
            "bot.conf.example.toml",
        ],
        "sections": sections,
        "fields": fields,
    }

    os.makedirs(os.path.dirname(OUTPUT), exist_ok=True)
    with open(OUTPUT, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, ensure_ascii=False, indent=2, sort_keys=True)
        handle.write("\n")

    print("写入 %s" % os.path.relpath(OUTPUT, REPO_ROOT))
    print("  分区 %d 个，字段 %d 个（密钥 %d 个）" % (
        len(sections),
        len(fields),
        sum(1 for entry in fields.values() if entry.get("secret")),
    ))
    if missing:
        print("警告：源码里找不到这些结构体: %s" % ", ".join(missing), file=sys.stderr)
    if len(fields) < 100:
        print("提取失败：字段数 %d 明显少于预期，提取逻辑没跟上代码改动" % len(fields),
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
