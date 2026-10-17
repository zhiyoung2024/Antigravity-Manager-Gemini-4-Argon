//! Ponytail (单束收拢、最小侵入) 核心切面模块
//!
//! 设计原则：
//! 1. 将 Google 最新 `v1internal` Hub 身份门控探测（含本地 `Antigravity.app` Info.plist 与
//!    `language_server` 二进制内嵌 Blaze `buildChangelist` 动态提取）集中收拢于单一模块。
//! 2. 将 `Gemini 4 Argon` (`gemini-4-argon` / `-low` / `-medium` / `-high`) 的模型识别、
//!    分档路由、独立配额桶匹配、以及 `ThinkingConfig` 第一性原理不变量（禁止 `MINIMAL`、
//!    强制 `thinkingBudget >= 1024`、原生 `thinkingLevel` 映射）收口在统一入口，避免侵入和破坏原有逻辑。

use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// 经过本机官方 Antigravity.app (v2.24.0) 逆向验证的权威保底 Hub 版本号
pub const FALLBACK_HUB_VERSION: &str = "2.24.0";

/// 经过本机官方 `language_server` (`0x1093eb0e0` `buildChangelist`) 逆向验证的权威保底 CL
pub const FALLBACK_HUB_CL: &str = "1000335682";

/// Argon 最小合法思考预算（Google v1internal 会将 < 1024 映射为 THINKING_LEVEL_MINIMAL 并返回 400）
pub const ARGON_MIN_THINKING_BUDGET: i64 = 1024;
pub const ARGON_MEDIUM_THINKING_BUDGET: i64 = 4000;
pub const ARGON_HIGH_THINKING_BUDGET: i64 = 10001;
pub const ARGON_MAX_OUTPUT_TOKENS: i64 = 65535;
pub const ARGON_MODEL_PLACEHOLDER: &str = "MODEL_PLACEHOLDER_M421";

/// 本地探测到的官方 Hub 指纹信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubFingerprint {
    pub version: String,
    pub cl: String,
    pub source: &'static str,
}

/// 缓存解析后的全局 Hub 指纹
pub static RESOLVED_HUB_FINGERPRINT: LazyLock<HubFingerprint> =
    LazyLock::new(resolve_hub_fingerprint);

/// 解析官方 Antigravity Hub 的 (version, cl) 指纹：
/// 优先级：环境变量 (`AG_HUB_VERSION` / `AG_HUB_CL`) > 本地安装的 `Antigravity.app` 实测提取 > 内置 2.24.0 保底基线
pub fn resolve_hub_fingerprint() -> HubFingerprint {
    let env_ver = std::env::var("AG_HUB_VERSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let env_cl = std::env::var("AG_HUB_CL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));

    if let (Some(version), Some(cl)) = (env_ver.clone(), env_cl.clone()) {
        return HubFingerprint {
            version,
            cl,
            source: "env_override",
        };
    }

    if let Some((local_ver, local_cl)) = detect_local_antigravity_hub() {
        // 仅当本地版本 >= 保底基线版本时采用本地版本，防止旧版客户端触发上游版本门控
        if crate::constants::compare_semver(&local_ver, FALLBACK_HUB_VERSION)
            >= std::cmp::Ordering::Equal
        {
            return HubFingerprint {
                version: env_ver.unwrap_or(local_ver),
                cl: env_cl.unwrap_or_else(|| {
                    local_cl.unwrap_or_else(|| FALLBACK_HUB_CL.to_string())
                }),
                source: "local_app_bundle",
            };
        }
    }

    HubFingerprint {
        version: env_ver.unwrap_or_else(|| FALLBACK_HUB_VERSION.to_string()),
        cl: env_cl.unwrap_or_else(|| FALLBACK_HUB_CL.to_string()),
        source: "fallback_baseline",
    }
}

/// 构造对齐官方 `codeassistclient.UserAgent` (`0x101997d60`) 的出站 User-Agent：
/// `antigravity/hub/{ver} (aidev_client; os_type={os}; arch={arch}; cl={cl})`
pub fn build_hub_user_agent() -> String {
    let fp = &*RESOLVED_HUB_FINGERPRINT;
    let os_type = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "darwin",
        "linux" => "linux",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!(
        "antigravity/hub/{} (aidev_client; os_type={}; arch={}; cl={})",
        fp.version, os_type, arch, fp.cl
    )
}

/// 探测本机安装的官方 Antigravity 客户端，同时提取 `Info.plist` 版本号与 `language_server` 二进制中的真实 `buildChangelist`
pub fn detect_local_antigravity_hub() -> Option<(String, Option<String>)> {
    for app_root in candidate_app_roots() {
        let plist_path = app_root.join("Contents").join("Info.plist");
        if let Some(ver) = extract_version_from_plist(&plist_path) {
            let ls_path = app_root
                .join("Contents")
                .join("Resources")
                .join("bin")
                .join("language_server");
            let cl = extract_cl_from_language_server(&ls_path);
            return Some((ver, cl));
        }
    }
    None
}

fn candidate_app_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "macos")]
    {
        roots.push(PathBuf::from("/Applications/Antigravity.app"));
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join("Applications").join("Antigravity.app"));
        }
    }
    roots
}

/// 从 macOS `Info.plist` 中提取 `CFBundleShortVersionString`
pub fn extract_version_from_plist(plist_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(plist_path).ok()?;
    let key_marker = "<key>CFBundleShortVersionString</key>";
    let pos = content.find(key_marker)?;
    let rest = &content[pos + key_marker.len()..];
    let s_start = rest.find("<string>")? + "<string>".len();
    let s_end = rest[s_start..].find("</string>")?;
    let raw = rest[s_start..s_start + s_end].trim();
    crate::constants::parse_version(raw)
}

/// 从官方 `language_server` 二进制流式扫描提取 Blaze 构建 CL 号
///
/// 原理：官方 Go 二进制编译时内嵌了 Piper 源码树路径：
/// `/google_src/files/{buildChangelist}/depot/branches/antigravity_web_release_branch/`
/// 使用 64KB 滑动窗口流式匹配该特征前缀，无需将整个 190MB 二进制载入内存。
pub fn extract_cl_from_language_server(binary_path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(binary_path).ok()?;
    const NEEDLE: &[u8] = b"/google_src/files/";
    const DEPOT_SUFFIX: &[u8] = b"/depot/branches/antigravity_web_release_branch/";
    const CHUNK_SIZE: usize = 64 * 1024;
    const OVERLAP: usize = 128;

    let mut buffer = vec![0u8; CHUNK_SIZE + OVERLAP];
    let mut carry = 0usize;

    loop {
        let bytes_read = file.read(&mut buffer[carry..]).ok()?;
        if bytes_read == 0 {
            break;
        }
        let valid_len = carry + bytes_read;
        let window = &buffer[..valid_len];

        if let Some(cl) = scan_bytes_for_cl(window, NEEDLE, DEPOT_SUFFIX) {
            return Some(cl);
        }

        if valid_len > OVERLAP {
            buffer.copy_within(valid_len - OVERLAP..valid_len, 0);
            carry = OVERLAP;
        } else {
            carry = valid_len;
        }
    }
    None
}

fn scan_bytes_for_cl(window: &[u8], needle: &[u8], suffix: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + needle.len() + suffix.len() + 8 <= window.len() {
        if &window[i..i + needle.len()] == needle {
            let digits_start = i + needle.len();
            let mut digits_end = digits_start;
            while digits_end < window.len() && window[digits_end].is_ascii_digit() {
                digits_end += 1;
            }
            let digit_len = digits_end - digits_start;
            if (8..=12).contains(&digit_len)
                && digits_end + suffix.len() <= window.len()
                && &window[digits_end..digits_end + suffix.len()] == suffix
            {
                if let Ok(cl_str) = std::str::from_utf8(&window[digits_start..digits_end]) {
                    return Some(cl_str.to_string());
                }
            }
            i = digits_end.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

// ============================================================================
// Gemini 4 Argon 模型识别、路由与配额桶隔离
// ============================================================================

/// 判定是否为 Gemini 4 Argon 系列模型（含裸模型及 -low / -medium / -high 变体）
#[inline]
pub fn is_argon_model(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower.contains("gemini-4-argon") || (lower.contains("gemini") && lower.contains("argon"))
}

/// 判定是否为不带显式档位后缀的 `gemini-4-argon` 裸模型
#[inline]
pub fn is_bare_argon_model(model: &str) -> bool {
    let lower = model
        .trim()
        .strip_prefix("models/")
        .unwrap_or(model.trim())
        .to_ascii_lowercase();
    lower == "gemini-4-argon"
}

/// Argon 系列标准公开模型列表
pub const ARGON_SUPPORTED_MODELS: &[&str] = &[
    "gemini-4-argon",
    "gemini-4-argon-high",
    "gemini-4-argon-medium",
    "gemini-4-argon-low",
];

/// 根据模型名称或档位获取 Argon 的权威数值型思考预算：
/// - `low`: 1024（严禁 < 1024，否则上游映射为 THINKING_LEVEL_MINIMAL 报 400）
/// - `medium` (或裸模型默认): 4000
/// - `high` / `thinking`: 10001
pub fn argon_tier_budget(model_or_tier: &str) -> u64 {
    let lower = model_or_tier.trim().to_ascii_lowercase();
    if lower.ends_with("-low")
        || lower.ends_with("-extra-low")
        || lower == "low"
        || lower == "extra-low"
        || lower == "minimal"
        || lower == "min"
    {
        ARGON_MIN_THINKING_BUDGET as u64
    } else if lower.ends_with("-high")
        || lower.ends_with("-thinking")
        || lower.ends_with("-max")
        || lower == "high"
        || lower == "max"
        || lower == "xhigh"
    {
        ARGON_HIGH_THINKING_BUDGET as u64
    } else {
        ARGON_MEDIUM_THINKING_BUDGET as u64
    }
}

/// 根据模型名称或档位获取官方原生 `thinkingLevel` 枚举字符串 (`"LOW"` / `"MEDIUM"` / `"HIGH"`)
pub fn argon_thinking_level(model: &str, client_effort: Option<&str>) -> &'static str {
    let lower = model.trim().to_ascii_lowercase();
    if lower.ends_with("-low") || lower.ends_with("-extra-low") {
        return "LOW";
    }
    if lower.ends_with("-high") || lower.ends_with("-thinking") || lower.ends_with("-max") {
        return "HIGH";
    }
    if lower.ends_with("-medium") {
        return "MEDIUM";
    }
    if let Some(eff) = client_effort {
        let e = eff.trim().to_ascii_lowercase();
        if matches!(
            e.as_str(),
            "low" | "extra-low" | "minimal" | "min" | "lite"
        ) {
            return "LOW";
        }
        if matches!(e.as_str(), "high" | "max" | "xhigh" | "x-high" | "extreme") {
            return "HIGH";
        }
    }
    "MEDIUM"
}

/// 配额汇总桶（`retrieveUserQuotaSummary`）精准匹配器：
/// 将 `gemini-4-argon*` 与普通 `gemini-3*` 严格隔离在不同的配额组，防止互相覆盖污染
pub fn matches_quota_summary_group(model_name_lower: &str, group_display_name_lower: &str) -> bool {
    let is_claude_or_gpt =
        model_name_lower.starts_with("claude") || model_name_lower.starts_with("gpt");
    let is_argon = is_argon_model(model_name_lower);
    let is_gemini = model_name_lower.starts_with("gemini") && !is_argon;

    if is_claude_or_gpt {
        group_display_name_lower.contains("claude")
            || group_display_name_lower.contains("gpt")
            || group_display_name_lower.contains("3p")
    } else if is_argon {
        group_display_name_lower.contains("argon")
    } else if is_gemini {
        (group_display_name_lower.contains("gemini") && !group_display_name_lower.contains("argon"))
            || (!group_display_name_lower.contains("claude")
                && !group_display_name_lower.contains("gpt")
                && !group_display_name_lower.contains("3p")
                && !group_display_name_lower.contains("argon"))
    } else {
        false
    }
}

// ============================================================================
// Gemini 4 Argon 第一性原理 ThinkingConfig 终审守门员 (Gatekeeper)
// ============================================================================

/// 对 `gemini-4-argon` 系列模型的 `generationConfig` 执行第一性原理不变量终审：
///
/// 1. **禁止关闭思考**：Argon 是强制思考模型，`includeThoughts` 恒为 `true`。
/// 2. **禁止 `THINKING_LEVEL_MINIMAL`**：
///    - 若 `thinkingLevel` 为 `"MINIMAL"` / `"NONE"` / `"DISABLED"` / `"0"`，强制抬升为 `"LOW"`；
///    - 若 `thinkingBudget` 存在且 `< 1024`（包含 `0`、`32`、`1000` 或被 `6x Intent` 压缩后的 `< 1024` 值），
///      强制钳制到 `>= 1024`（或按模型档位恢复为 `1024` / `4000` / `10001`）。
/// 3. **修复网关默认模式下 `-1` 覆盖档位预算的缺陷**：
///    - 当网关处于权威控制模式且 `thinkingBudget` 为缺失或 `-1` 时，按目标变体（`-low` / `-medium` / `-high`）
///      赋予对应的权威预算（`1024` / `4000` / `10001`）。
/// 4. **保证 `maxOutputTokens > thinkingBudget` 且不超过官方上限 `65535`**：
///    - 当 `thinkingBudget` 被钳制抬升后，若 `maxOutputTokens <= thinkingBudget`，自动扩充至 `thinkingBudget + 1024`。
pub fn enforce_argon_thinking_invariants(
    generation_config: &mut Value,
    target_model: &str,
    client_effort: Option<&str>,
    is_client_control: bool,
) {
    if !is_argon_model(target_model) {
        return;
    }

    let gc_obj = match generation_config.as_object_mut() {
        Some(obj) => obj,
        None => return,
    };

    let tier_default_budget = if is_bare_argon_model(target_model) {
        if let Some(eff) = client_effort {
            argon_tier_budget(eff) as i64
        } else {
            ARGON_MEDIUM_THINKING_BUDGET
        }
    } else {
        argon_tier_budget(target_model) as i64
    };

    let tc_val = gc_obj
        .entry("thinkingConfig".to_string())
        .or_insert_with(|| json!({ "includeThoughts": true }));

    if !tc_val.is_object() {
        *tc_val = json!({ "includeThoughts": true });
    }

    let tc_obj = tc_val.as_object_mut().unwrap();
    tc_obj.insert("includeThoughts".to_string(), json!(true));

    // 1. 規範化 thinkingLevel（若存在）
    if let Some(level_str) = tc_obj
        .get("thinkingLevel")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_uppercase())
    {
        let sanitized_level = match level_str.as_str() {
            "MINIMAL" | "MIN" | "EXTRA-LOW" | "EXTRA_LOW" | "NONE" | "DISABLED" | "OFF" | "0"
            | "1" | "LOW" => "LOW",
            "2" | "MEDIUM" | "DEFAULT" | "NORMAL" => "MEDIUM",
            "3" | "HIGH" | "MAX" | "XHIGH" | "EXTREME" => "HIGH",
            _ => argon_thinking_level(target_model, client_effort),
        };
        tc_obj.insert("thinkingLevel".to_string(), json!(sanitized_level));

        // Protobuf oneof 互斥原则：若客户端控制模式下保留了 thinkingLevel 且未传有效 budget，移除冲突字段
        if is_client_control {
            if let Some(b) = tc_obj.get("thinkingBudget").and_then(|v| v.as_i64()) {
                if b <= 0 {
                    tc_obj.remove("thinkingBudget");
                } else {
                    // 客户端显式传了正数 budget，优先使用钳制后的 budget，移除 thinkingLevel 防 oneof 冲突
                    tc_obj.remove("thinkingLevel");
                }
            }
        } else {
            tc_obj.remove("thinkingLevel");
        }
    }

    // 2. 規範化 thinkingBudget
    let current_budget = tc_obj.get("thinkingBudget").and_then(|v| v.as_i64());
    let has_thinking_level = tc_obj.contains_key("thinkingLevel");

    if !has_thinking_level {
        let enforced_budget = match current_budget {
            Some(b) if b >= ARGON_MIN_THINKING_BUDGET => b,
            Some(b) if b > 0 && b < ARGON_MIN_THINKING_BUDGET => {
                // 任何 1..1023 的预算（如 1000 或 6x 压缩后的碎片值）在 Argon 上必报 400 Minimal 错误，强制抬升至 1024
                ARGON_MIN_THINKING_BUDGET
            }
            Some(0) => {
                // Argon 不允许 budget=0（含 6x 极速意图压缩出的 0），强制保底 1024
                ARGON_MIN_THINKING_BUDGET
            }
            Some(-1) | None => {
                if is_client_control {
                    // 客户端直接控制且未传 budget/level 时，赋予具名变体对应的官方 thinkingLevel
                    let level = argon_thinking_level(target_model, client_effort);
                    tc_obj.remove("thinkingBudget");
                    tc_obj.insert("thinkingLevel".to_string(), json!(level));
                    -1
                } else {
                    // 网关权威控制模式下，锁定到变体对应的权威预算 (1024 / 4000 / 10001)
                    tier_default_budget
                }
            }
            Some(_) => tier_default_budget,
        };

        if enforced_budget >= ARGON_MIN_THINKING_BUDGET {
            let capped_budget = enforced_budget.min(ARGON_MAX_OUTPUT_TOKENS - 1024);
            tc_obj.insert("thinkingBudget".to_string(), json!(capped_budget));
        }
    }

    // 3. 协调 maxOutputTokens 与 thinkingBudget 的硬约束 (maxOutputTokens > thinkingBudget 且 <= 65535)
    let final_budget = gc_obj
        .get("thinkingConfig")
        .and_then(|tc| tc.get("thinkingBudget"))
        .and_then(|v| v.as_i64());

    let current_max = gc_obj
        .get("maxOutputTokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(ARGON_MAX_OUTPUT_TOKENS);

    let mut adjusted_max = current_max.min(ARGON_MAX_OUTPUT_TOKENS);
    if let Some(b) = final_budget {
        if b > 0 && adjusted_max <= b {
            adjusted_max = (b + 1024).min(ARGON_MAX_OUTPUT_TOKENS);
        }
    }
    gc_obj.insert("maxOutputTokens".to_string(), json!(adjusted_max));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_bytes_for_cl_extracts_exact_changelist() {
        let fake_binary = b"random_bytes\x00/google_src/files/1000335682/depot/branches/antigravity_web_release_branch/997602692.1/google3/java/com/google/devtools\x00trailing";
        let cl = scan_bytes_for_cl(
            fake_binary,
            b"/google_src/files/",
            b"/depot/branches/antigravity_web_release_branch/",
        );
        assert_eq!(cl.as_deref(), Some("1000335682"));
    }

    #[test]
    fn test_hub_user_agent_format_is_valid() {
        let ua = build_hub_user_agent();
        assert!(
            ua.starts_with("antigravity/hub/"),
            "UA must start with antigravity/hub/: {ua}"
        );
        assert!(ua.contains("aidev_client; os_type="));
        assert!(ua.contains("; cl="));
    }

    #[test]
    fn test_argon_identification_and_tier_budgets() {
        assert!(is_argon_model("gemini-4-argon"));
        assert!(is_argon_model("gemini-4-argon-low"));
        assert!(is_argon_model("gemini-4-argon-medium"));
        assert!(is_argon_model("gemini-4-argon-high"));
        assert!(!is_argon_model("gemini-3.8-flash-high"));

        assert!(is_bare_argon_model("gemini-4-argon"));
        assert!(is_bare_argon_model("models/gemini-4-argon"));
        assert!(!is_bare_argon_model("gemini-4-argon-low"));

        assert_eq!(argon_tier_budget("gemini-4-argon-low"), 1024);
        assert_eq!(argon_tier_budget("gemini-4-argon-medium"), 4000);
        assert_eq!(argon_tier_budget("gemini-4-argon-high"), 10001);
        assert_eq!(argon_tier_budget("low"), 1024);
    }

    #[test]
    fn test_quota_summary_group_isolation() {
        // Argon 只匹配 Argon 桶，绝不匹配普通 Gemini 桶
        assert!(matches_quota_summary_group(
            "gemini-4-argon-high",
            "gemini 4 argon"
        ));
        assert!(!matches_quota_summary_group(
            "gemini-4-argon-high",
            "gemini"
        ));
        assert!(!matches_quota_summary_group(
            "gemini-4-argon-high",
            "claude & gpt"
        ));

        // 普通 Gemini 匹配 Gemini 桶，绝不匹配 Argon 桶
        assert!(matches_quota_summary_group(
            "gemini-3.8-flash-high",
            "gemini"
        ));
        assert!(!matches_quota_summary_group(
            "gemini-3.8-flash-high",
            "gemini 4 argon"
        ));
    }

    #[test]
    fn test_enforce_argon_invariants_prevents_minimal_and_6x_zero_bug() {
        // 场景 1：1000 (< 1024) 被钳制到 1024
        let mut gc = json!({
            "maxOutputTokens": 65535,
            "thinkingConfig": {
                "includeThoughts": true,
                "thinkingBudget": 1000
            }
        });
        enforce_argon_thinking_invariants(&mut gc, "gemini-4-argon-low", None, false);
        assert_eq!(gc["thinkingConfig"]["thinkingBudget"], json!(1024));

        // 场景 2：6x Intent 压缩后变成 0 且 maxOutputTokens=1024，必须抬升回 1024 且扩充 maxOutputTokens=2048
        let mut gc_6x = json!({
            "maxOutputTokens": 1024,
            "thinkingConfig": {
                "includeThoughts": true,
                "thinkingBudget": 0
            }
        });
        enforce_argon_thinking_invariants(&mut gc_6x, "gemini-4-argon-high", None, false);
        assert_eq!(gc_6x["thinkingConfig"]["thinkingBudget"], json!(1024));
        assert_eq!(gc_6x["maxOutputTokens"], json!(2048));

        // 场景 3：网关默认模式下 official_models.json 的 -1 被还原为具名变体对应的权威预算
        let mut gc_neg1 = json!({
            "maxOutputTokens": 65535,
            "thinkingConfig": {
                "includeThoughts": true,
                "thinkingBudget": -1
            }
        });
        enforce_argon_thinking_invariants(&mut gc_neg1, "gemini-4-argon-medium", None, false);
        assert_eq!(gc_neg1["thinkingConfig"]["thinkingBudget"], json!(4000));

        // 场景 4：客户端控制模式下传入 MINIMAL 等级，自动抬升为 LOW 且不带冲突的 thinkingBudget
        let mut gc_client = json!({
            "maxOutputTokens": 65535,
            "thinkingConfig": {
                "includeThoughts": true,
                "thinkingLevel": "MINIMAL"
            }
        });
        enforce_argon_thinking_invariants(
            &mut gc_client,
            "gemini-4-argon",
            Some("minimal"),
            true,
        );
        assert_eq!(gc_client["thinkingConfig"]["thinkingLevel"], json!("LOW"));
        assert!(gc_client["thinkingConfig"].get("thinkingBudget").is_none());
    }

    /// 模拟 Google `v1internal` 上游核心校验器（复现真实门控与 Argon 参数校验规则）
    fn simulate_google_v1internal_generate_content(
        user_agent: &str,
        envelope: &Value,
    ) -> Result<Value, (u16, String)> {
        let model = envelope.get("model").and_then(|v| v.as_str()).unwrap_or("");

        // 1. 校验上游 User-Agent 门控：非 antigravity/hub/ 请求 Argon 模型一律返回 404
        if is_argon_model(model) && !user_agent.starts_with("antigravity/hub/") {
            return Err((
                404,
                format!(
                    "Requested entity was not found: model '{}' requires official antigravity/hub identity, got UA '{}'",
                    model, user_agent
                ),
            ));
        }

        // 2. 校验裸模型名称：上游 v1internal 只认识 gemini-4-argon-low / -medium / -high，不认识裸名 gemini-4-argon
        if model == "gemini-4-argon" {
            return Err((
                404,
                "Requested entity was not found: bare 'gemini-4-argon' must be routed to -low/-medium/-high".to_string(),
            ));
        }

        let inner_req = envelope
            .get("request")
            .ok_or_else(|| (400, "Missing inner 'request' object".to_string()))?;
        let gen_cfg = inner_req
            .get("generationConfig")
            .ok_or_else(|| (400, "Missing 'generationConfig'".to_string()))?;

        // 3. 针对 Gemini 4 Argon 的专属 ThinkingConfig 第一性原理校验
        if is_argon_model(model) {
            let tc = gen_cfg.get("thinkingConfig").ok_or_else(|| {
                (
                    400,
                    "INVALID_ARGUMENT: Gemini 4 Argon requires thinkingConfig".to_string(),
                )
            })?;

            if tc.get("includeThoughts").and_then(|v| v.as_bool()) != Some(true) {
                return Err((
                    400,
                    "INVALID_ARGUMENT: includeThoughts must be true for Gemini 4 Argon".to_string(),
                ));
            }

            let has_budget = tc.get("thinkingBudget").is_some();
            let has_level = tc.get("thinkingLevel").is_some();
            if has_budget && has_level {
                return Err((
                    400,
                    "INVALID_ARGUMENT: thinking_budget and thinking_level are mutually exclusive in oneof".to_string(),
                ));
            }

            if let Some(level) = tc.get("thinkingLevel").and_then(|v| v.as_str()) {
                if level.eq_ignore_ascii_case("MINIMAL") {
                    return Err((
                        400,
                        "INVALID_ARGUMENT: Thinking level is unsupported: THINKING_LEVEL_MINIMAL"
                            .to_string(),
                    ));
                }
                if !matches!(level, "LOW" | "MEDIUM" | "HIGH") {
                    return Err((
                        400,
                        format!("INVALID_ARGUMENT: Unsupported thinkingLevel '{}'", level),
                    ));
                }
            } else if let Some(budget) = tc.get("thinkingBudget").and_then(|v| v.as_i64()) {
                if budget < 1024 {
                    return Err((
                        400,
                        format!(
                            "INVALID_ARGUMENT: Thinking level is unsupported: THINKING_LEVEL_MINIMAL (budget {} < 1024)",
                            budget
                        ),
                    ));
                }
            } else {
                return Err((
                    400,
                    "INVALID_ARGUMENT: Either thinkingBudget (>=1024) or thinkingLevel (LOW/MEDIUM/HIGH) is required".to_string(),
                ));
            }

            let max_out = gen_cfg
                .get("maxOutputTokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if max_out <= 0 || max_out > 65535 {
                return Err((
                    400,
                    format!(
                        "INVALID_ARGUMENT: maxOutputTokens ({}) out of range 1..=65535",
                        max_out
                    ),
                ));
            }
            if let Some(budget) = tc.get("thinkingBudget").and_then(|v| v.as_i64()) {
                if max_out <= budget {
                    return Err((
                        400,
                        format!(
                            "INVALID_ARGUMENT: maxOutputTokens ({}) must be > thinkingBudget ({})",
                            max_out, budget
                        ),
                    ));
                }
            }

            let model_enum = inner_req
                .get("labels")
                .and_then(|l| l.get("model_enum"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if model_enum != ARGON_MODEL_PLACEHOLDER {
                return Err((
                    400,
                    format!(
                        "INVALID_ARGUMENT: Expected model_enum '{}', got '{}'",
                        ARGON_MODEL_PLACEHOLDER, model_enum
                    ),
                ));
            }
        }

        Ok(json!({
            "response": {
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [
                            { "thought": true, "text": "Analyzing request with Gemini 4 Argon..." },
                            { "text": " Gemini 4 Argon response verified successfully." }
                        ]
                    },
                    "finishReason": "STOP"
                }],
                "modelVersion": model
            }
        }))
    }

    #[test]
    fn test_end_to_end_sandbox_simulation_for_gemini_4_argon_and_non_regression() {
        use crate::proxy::common::model_mapping::{
            is_model_compliant_with_baseline, normalize_to_standard_id,
            resolve_model_route_with_effort,
        };
        use crate::proxy::mappers::gemini::wrapper::wrap_request;
        use std::collections::HashMap;

        let custom_mapping = HashMap::new();
        let egress_ua = crate::constants::USER_AGENT.as_str();

        // 1. 验证旧版 NATIVE_OAUTH_USER_AGENT 会被上游门控 404 拒绝，而新 Hub UA 通过
        let sample_body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "Hello Argon" }] }]
        });
        let mapped_high =
            resolve_model_route_with_effort("gemini-4-argon", &custom_mapping, Some("high"));
        assert_eq!(mapped_high, "gemini-4-argon-high");

        let wrapped_high = wrap_request(
            &sample_body,
            "test-project",
            &mapped_high,
            None,
            None,
            None,
        );

        // 旧 UA 触发 404
        let old_ua_res = simulate_google_v1internal_generate_content(
            crate::constants::NATIVE_OAUTH_USER_AGENT.as_str(),
            &wrapped_high,
        );
        assert!(old_ua_res.is_err());
        assert_eq!(old_ua_res.unwrap_err().0, 404);

        // 新 Ponytail Hub UA 返回 200 OK
        let new_ua_res = simulate_google_v1internal_generate_content(egress_ua, &wrapped_high);
        assert!(
            new_ua_res.is_ok(),
            "Expected 200 OK with Hub UA, got: {:?}",
            new_ua_res
        );

        // 2. 验证裸模型 gemini-4-argon 在不同 effort (None, low, medium, high, minimal) 下全链路通过上游校验
        for (effort, expected_model, expected_budget) in [
            (None, "gemini-4-argon-medium", 4000),
            (Some("low"), "gemini-4-argon-low", 1024),
            (Some("minimal"), "gemini-4-argon-low", 1024),
            (Some("medium"), "gemini-4-argon-medium", 4000),
            (Some("high"), "gemini-4-argon-high", 10001),
        ] {
            let routed = resolve_model_route_with_effort("gemini-4-argon", &custom_mapping, effort);
            assert_eq!(routed, expected_model, "Failed routing for effort={:?}", effort);

            let body = json!({
                "contents": [{ "role": "user", "parts": [{ "text": "Test tier" }] }],
                "generationConfig": {
                    "thinkingConfig": {
                        "thinkingLevel": effort.unwrap_or("medium")
                    }
                }
            });
            let envelope = wrap_request(
                &body,
                "test-project",
                &routed,
                None,
                None,
                None,
            );
            let budget = envelope["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"]
                .as_i64()
                .expect("thinkingBudget must be present in gateway mode");
            assert_eq!(
                budget, expected_budget,
                "Unexpected budget for routed={}",
                routed
            );
            assert!(
                simulate_google_v1internal_generate_content(egress_ua, &envelope).is_ok(),
                "Upstream simulation failed for effort={:?}, envelope={}",
                effort,
                envelope
            );
        }

        // 3. 验证极端边界：客户端传入 maxOutputTokens=1024 触发 6x Intent 极速压缩时，
        // Ponytail 终审门禁防止 budget 归零，确保不触发 400 THINKING_LEVEL_MINIMAL
        let body_6x = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "Short answer" }] }],
            "generationConfig": {
                "maxOutputTokens": 1024
            }
        });
        let envelope_6x = wrap_request(
            &body_6x,
            "test-project",
            "gemini-4-argon-high",
            None,
            None,
            None,
        );
        assert!(
            simulate_google_v1internal_generate_content(egress_ua, &envelope_6x).is_ok(),
            "6x Intent boundary must not fail upstream validation: {}",
            envelope_6x
        );

        // 4. 验证零回归：原有 Gemini 3.8 Flash Low 依然保持 1000 预算，配额桶隔离正常
        assert!(is_model_compliant_with_baseline("gemini-4-argon-low"));
        assert_eq!(
            normalize_to_standard_id("gemini-4-argon-high").as_deref(),
            Some("gemini-4-argon")
        );
        assert_eq!(
            normalize_to_standard_id("gemini-3.8-flash-low").as_deref(),
            Some("gemini-3-pro-high")
        );
        let flash_envelope = wrap_request(
            &sample_body,
            "test-project",
            "gemini-3.8-flash-low",
            None,
            None,
            None,
        );
        assert_eq!(
            flash_envelope["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(1000)
        );
    }
}
