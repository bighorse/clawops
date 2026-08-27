//! Coherence checks on a breed, run at install time.
//!
//! `breeds::validate_tree` already refuses a bundle whose templates don't
//! compile. That catches typos, and nothing else: it renders each template
//! against an **empty** context purely to exercise the parser, so it cannot
//! see what the tenant will actually get. Every bug found in the first
//! hand-authored breed lived in that blind spot — the templates compiled
//! perfectly and still produced a lobster that could not answer one message.
//!
//! So these rules run against the **rendered** config, using the same
//! context the provisioner builds for a real tenant.
//!
//! Errors block the install. Warnings are reported and let it through: a
//! breed that merely smells wrong is the author's call, but a breed that
//! provably cannot work is not.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    /// Stable id so a caller can suppress or test one rule.
    pub rule: &'static str,
    pub message: String,
}

impl Finding {
    fn err(rule: &'static str, message: impl Into<String>) -> Self {
        Self { level: Level::Error, rule, message: message.into() }
    }
    fn warn(rule: &'static str, message: impl Into<String>) -> Self {
        Self { level: Level::Warning, rule, message: message.into() }
    }
}

/// Everything the linter needs: the template sources (to spot patterns that
/// only exist before rendering) and the rendered config (to spot what the
/// tenant actually gets).
pub struct Input<'a> {
    /// `relative path -> file text`, for the whole breed.
    pub sources: &'a BTreeMap<String, String>,
    /// `config.toml.hbs` rendered with a real tenant context.
    pub rendered_config: &'a str,
    /// Provider id the swarm resolves to, e.g. `qwen`.
    pub swarm_provider: &'a str,
    /// Base URL the swarm resolves to, empty when unset.
    pub swarm_api_url: &'a str,
}

pub fn lint(input: &Input) -> Vec<Finding> {
    let mut out = Vec::new();

    // ── E1: the rendered config must be valid TOML ──────────────────
    //
    // Nothing checked this before. A breed whose template compiles but
    // renders to broken TOML installs cleanly, and is only discovered when
    // every daemon on it fails to start — hours later, far from the cause.
    let cfg: toml::Value = match input.rendered_config.parse() {
        Ok(v) => v,
        Err(e) => {
            out.push(Finding::err(
                "rendered-config-invalid-toml",
                format!(
                    "config.toml.hbs 渲染后不是合法 TOML：{e}。\
                     模板语法没问题，但渲染出来的内容 zeroclaw 读不了，\
                     该品种下每个租户的 daemon 都会起不来。"
                ),
            ));
            return out; // every later rule reads this document
        }
    };

    skill_instructions_reachable(&cfg, &mut out);
    cost_table_matches_runtime(&cfg, input, &mut out);
    model_pinned_but_endpoint_inherited(input, &mut out);
    runtime_writes_under_scripts(input, &mut out);
    dangling_path_references(&cfg, input, &mut out);
    out
}

/// E2 — `compact` skill injection tells the model to read the skill file on
/// demand; `noread_prefixes` covering `skills/` makes that read fail. The
/// model then runs on a one-line description with none of the skill's actual
/// rules, which is indistinguishable from having no skill at all — except it
/// looks configured.
fn skill_instructions_reachable(cfg: &toml::Value, out: &mut Vec<Finding>) {
    let mode = cfg
        .get("skills")
        .and_then(|s| s.get("prompt_injection_mode"))
        .and_then(toml::Value::as_str)
        .unwrap_or("full");
    if mode != "compact" {
        return;
    }
    let blocked = cfg
        .get("autonomy")
        .and_then(|a| a.get("noread_prefixes"))
        .and_then(toml::Value::as_array)
        .is_some_and(|v| {
            v.iter()
                .filter_map(toml::Value::as_str)
                .any(|p| p == "skills" || p == "skills/" || p.starts_with("skills/"))
        });
    if blocked {
        out.push(Finding::err(
            "compact-skills-unreadable",
            "skills.prompt_injection_mode = \"compact\" 只把技能的 name/description \
             注入提示词，指令要模型自己去读 skills/ 下的文件；而 \
             autonomy.noread_prefixes 挡住了 skills/。两者同时开，技能正文永远到不了\
             模型，它只能凭一句描述作答。改用 prompt_injection_mode = \"full\"\
             （指令直接预加载，此时挡住 skills/ 才是对的），或从 noread_prefixes \
             去掉 skills/。",
        ));
    }
}

/// E3 — a `[cost.prices]` table whose keys can't match the runtime provider
/// key is dead weight: spend silently records as zero. That is worse than
/// having no table, because the dashboard says 0 instead of saying nothing.
fn cost_table_matches_runtime(cfg: &toml::Value, input: &Input, out: &mut Vec<Finding>) {
    let Some(prices) = cfg
        .get("cost")
        .and_then(|c| c.get("prices"))
        .and_then(toml::Value::as_table)
    else {
        return;
    };
    if prices.is_empty() {
        return;
    }
    let model = cfg
        .get("default_model")
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let provider = cfg
        .get("default_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let base_url = cfg
        .get("model_providers")
        .and_then(|m| m.get(provider))
        .and_then(|p| p.get("base_url"))
        .and_then(toml::Value::as_str)
        .unwrap_or(input.swarm_api_url);

    // zeroclaw keys custom endpoints by URL and built-ins by provider id.
    let expected = [
        format!("custom:{}/{model}", base_url.trim_end_matches('/')),
        format!("{provider}/{model}"),
    ];
    if expected.iter().any(|k| prices.contains_key(k.as_str())) {
        return;
    }
    let listed: Vec<&str> = prices.keys().map(String::as_str).take(4).collect();
    out.push(Finding::err(
        "cost-prices-never-match",
        format!(
            "[cost.prices] 里没有一条能匹配运行时实际使用的模型。\
             渲染后跑的是 provider=\"{provider}\" base_url=\"{base_url}\" \
             model=\"{model}\"，需要的键是 \"{}\" 或 \"{}\"；\
             而表里是 {listed:?}。不匹配不会报错，只会让成本统计恒为 0。",
            expected[0], expected[1]
        ),
    ));
}

/// W1 — the pattern behind the first breed's fatal bug: the model name is
/// pinned in the breed while the endpoint is inherited from the swarm. The
/// two are then free to drift apart, and they did — a DeepSeek model name
/// pointed at a DashScope endpoint. Source-level, because after rendering
/// there is no way to tell a literal from a substitution.
fn model_pinned_but_endpoint_inherited(input: &Input, out: &mut Vec<Finding>) {
    let Some(src) = input.sources.get("config.toml.hbs") else {
        return;
    };
    let pinned_model = src.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("default_model") && !t.contains("{{")
    });
    let inherited_url = src.contains("{{llm.api_url}}");
    if pinned_model && inherited_url {
        out.push(Finding::warn(
            "model-pinned-endpoint-inherited",
            format!(
                "config.toml.hbs 把 default_model 写死了，但 base_url 用的是 \
                 {{{{llm.api_url}}}}，会跟着虾群全局配置走（当前解析为 \
                 \"{}\"）。模型和端点由两个来源决定，很容易对不上——\
                 拿 A 家的模型名去问 B 家的端点，第一条消息就会失败。\
                 要么两者都写死，要么两者都用占位符。",
                if input.swarm_api_url.is_empty() { "<未设置>" } else { input.swarm_api_url }
            ),
        ));
    }
}

/// W2 — `scripts/` is template-owned: the provisioner clears it on every
/// render. A skill that appends runtime knowledge there loses it on the next
/// push, which is exactly when a knowledge-base breed gets pushed.
fn runtime_writes_under_scripts(input: &Input, out: &mut Vec<Finding>) {
    let mut hits: Vec<&str> = Vec::new();
    for (path, text) in input.sources {
        if !(path.starts_with("skills/") || path.starts_with("sops/")) {
            continue;
        }
        let writes = text
            .lines()
            .any(|l| l.contains("scripts/") && (l.contains("file_write") || l.contains("file_edit")));
        if writes {
            hits.push(path);
        }
    }
    if !hits.is_empty() {
        out.push(Finding::warn(
            "runtime-write-under-scripts",
            format!(
                "{hits:?} 指示写入 scripts/ 下的文件。provisioner 每次渲染都会\
                 先清空 scripts/ 再整棵拷贝，所以运行期写进去的内容会在下一次\
                 推送品种或 refresh 时全部消失——而更新知识库正好就要推送品种。\
                 运行期产物请写到 state/ 或 feedback/ 这类不由模板托管的目录。"
            ),
        ));
    }
}

/// W3 — path guards naming files the breed doesn't ship. Harmless at
/// runtime, but it is the fingerprint of a config copied from another
/// lobster, and the copied parts are where the real mistakes hide.
fn dangling_path_references(cfg: &toml::Value, input: &Input, out: &mut Vec<Finding>) {
    let mut refs: Vec<String> = Vec::new();
    for (section, key) in [
        ("autonomy", "readonly_prefixes"),
        ("autonomy", "noread_prefixes"),
    ] {
        if let Some(arr) = cfg.get(section).and_then(|s| s.get(key)).and_then(toml::Value::as_array) {
            refs.extend(arr.iter().filter_map(toml::Value::as_str).map(str::to_string));
        }
    }
    let mut dangling: Vec<String> = refs
        .into_iter()
        .filter(|r| r.ends_with(".md"))
        .filter(|r| !input.sources.contains_key(&format!("{r}.hbs")))
        .collect();
    dangling.sort();
    dangling.dedup();
    if !dangling.is_empty() {
        out.push(Finding::warn(
            "dangling-path-reference",
            format!(
                "config 里的路径白名单提到了这些文件，但品种里没有它们：{dangling:?}。\
                 通常是从别的龙虾抄配置留下的痕迹——抄来的部分正是最容易出错的地方，\
                 建议顺手核一遍。"
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn input<'a>(
        src: &'a BTreeMap<String, String>,
        rendered: &'a str,
    ) -> Input<'a> {
        Input {
            sources: src,
            rendered_config: rendered,
            swarm_provider: "qwen",
            swarm_api_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        }
    }

    fn rules(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|x| x.rule).collect()
    }

    /// The real breed rendered to this: a DeepSeek model name, a DashScope
    /// endpoint, and a price table keyed for a third address. Three separate
    /// ways of saying the same thing, and the lobster could not answer once.
    const OPC_RENDERED: &str = r#"
default_provider = "qwen"
default_model = "deepseek-v4-flash"

[model_providers.qwen]
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"

[skills]
prompt_injection_mode = "compact"

[autonomy]
noread_prefixes = ["skills/", "AGENTS.md", "HEARTBEAT.md"]
readonly_prefixes = ["skills/", "HEARTBEAT.md"]

[cost.prices."custom:http://127.0.0.1:42800/v1/deepseek-v4-flash"]
input = 0.35
output = 0.69
"#;

    #[test]
    fn catches_every_defect_of_the_first_hand_authored_breed() {
        let src = sources(&[
            (
                "config.toml.hbs",
                "default_model = \"deepseek-v4-flash\"\nbase_url = \"{{llm.api_url}}\"\n",
            ),
            (
                "skills/opc-knowledge-update/SKILL.md.hbs",
                "file_write 追加到 `scripts/知识库/待确认.md`\n",
            ),
        ]);
        let found = lint(&input(&src, OPC_RENDERED));
        let got = rules(&found);

        // The two that make it unusable must block the push.
        for rule in ["compact-skills-unreadable", "cost-prices-never-match"] {
            let f = found.iter().find(|f| f.rule == rule).unwrap_or_else(|| {
                panic!("rule {rule} did not fire; got {got:?}")
            });
            assert_eq!(f.level, Level::Error, "{rule} must block: {}", f.message);
        }
        // The two that are judgement calls must report, not block.
        for rule in ["model-pinned-endpoint-inherited", "runtime-write-under-scripts"] {
            let f = found.iter().find(|f| f.rule == rule).unwrap_or_else(|| {
                panic!("rule {rule} did not fire; got {got:?}")
            });
            assert_eq!(f.level, Level::Warning, "{rule} must not block");
        }
        // HEARTBEAT.md is guarded but never shipped — the fingerprint of a
        // config copied from another lobster.
        assert!(got.contains(&"dangling-path-reference"), "got {got:?}");
    }

    /// Rendered TOML that doesn't parse is the failure nothing caught
    /// before: templates compile, install succeeds, and every daemon on the
    /// breed then refuses to start.
    #[test]
    fn blocks_a_config_that_renders_into_broken_toml() {
        let src = sources(&[("config.toml.hbs", "port = {{port}}\n")]);
        let found = lint(&input(&src, "port =\n[unclosed\n"));
        assert_eq!(rules(&found), vec!["rendered-config-invalid-toml"]);
        assert_eq!(found[0].level, Level::Error);
    }

    /// A coherent breed must pass silently — a linter that cries wolf on
    /// good input gets switched off, and then catches nothing at all.
    #[test]
    fn a_coherent_breed_produces_no_findings() {
        let src = sources(&[(
            "config.toml.hbs",
            "default_model = \"{{llm.default_model}}\"\nbase_url = \"{{llm.api_url}}\"\n",
        )]);
        let rendered = r#"
default_provider = "qwen"
default_model = "qwen3.6-flash"

[model_providers.qwen]
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"

[skills]
prompt_injection_mode = "full"

[autonomy]
noread_prefixes = ["skills/"]
readonly_prefixes = []

[cost.prices."custom:https://dashscope.aliyuncs.com/compatible-mode/v1/qwen3.6-flash"]
input = 0.1
output = 0.2
"#;
        assert!(lint(&input(&src, rendered)).is_empty());
    }

    /// `full` mode preloads the instructions, so blocking reads of skills/
    /// is then correct hardening rather than a contradiction.
    #[test]
    fn full_injection_with_unreadable_skills_is_fine() {
        let src = sources(&[("config.toml.hbs", "x = 1\n")]);
        let rendered = "[skills]\nprompt_injection_mode = \"full\"\n\n[autonomy]\nnoread_prefixes = [\"skills/\"]\n";
        assert!(rules(&lint(&input(&src, rendered))).is_empty());
    }

    /// No price table at all is a deliberate choice (spend tracked
    /// elsewhere); only a table that can never match is the bug.
    #[test]
    fn an_absent_price_table_is_not_a_finding() {
        let src = sources(&[("config.toml.hbs", "x = 1\n")]);
        let rendered = "default_provider = \"qwen\"\ndefault_model = \"qwen3.6-flash\"\n";
        assert!(!rules(&lint(&input(&src, rendered))).contains(&"cost-prices-never-match"));
    }
}
