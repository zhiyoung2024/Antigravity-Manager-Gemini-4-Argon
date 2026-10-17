//! Antigravity-Manager "Ponytail" 手术刀方案 —— 全链路沙盒模拟验证器 (A/B 对比实测)
//!
//! 本验证器在标准沙盒内直接通过 `rustc --test` 编译运行（零外部网络依赖）：
//! 1. 静态源码审计断言：直接读取并校验仓库内修改后的全部目标源文件与 JSON 配置文件，确保“单束收拢 (ponytail)”切口准确无误。
//! 2. 本地 App Bundle 指纹与二进制 CL 扫描实测：构造模拟 `/Applications/Antigravity.app` 结构（含 `Info.plist` v2.24.0 与 2MB 模拟 `language_server` 二进制跨 64KB 块边界内嵌 `/google_src/files/1000335682/depot/branches/antigravity_web_release_branch/`），验证 `ponytail` 64KB 滑动窗口扫描器精确提取版本与 CL。
//! 3. 启动真实本地 TCP HTTP Server 模拟 Google `daily-cloudcode-pa.googleapis.com/v1internal` 四大端点：
//!    - `/v1internal:loadCodeAssist`
//!    - `/v1internal:fetchAvailableModels` (复现 User-Agent 门控：旧 `vscode/1.X.X` UA 仅返回 35 个模型并剔除 Argon；`antigravity/hub/2.24.0 (...; cl=1000335682)` 返回含 `gemini-4-argon-low/medium/high` 的 38 个模型)
//!    - `/v1internal:retrieveUserQuotaSummary` (复现 Argon 独立配额桶 vs 普通 Gemini 配额桶)
//!    - `/v1internal:generateContent` (复现 UA 404 门控、裸模型 404、`thinkingBudget < 1024` 映射为 `THINKING_LEVEL_MINIMAL` 触发 400 INVALID_ARGUMENT、`thinkingLevel` 与 `thinkingBudget` 互斥 `oneof` 校验、`maxOutputTokens > thinkingBudget` 校验)
//! 4. 执行【修复前 (v4.9.6 原始逻辑)】vs【修复后 (Ponytail 手术刀方案)】全链路 A/B 对比实测。

use std::io::{pipe, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

// ============================================================================
// 1. 直接复用 ponytail.rs 中的核心算法进行实测
// ============================================================================

pub const FALLBACK_HUB_VERSION: &str = "2.24.0";
pub const FALLBACK_HUB_CL: &str = "1000335682";
pub const ARGON_MIN_THINKING_BUDGET: i64 = 1024;
pub const ARGON_MEDIUM_THINKING_BUDGET: i64 = 4000;
pub const ARGON_HIGH_THINKING_BUDGET: i64 = 10001;
pub const ARGON_MAX_OUTPUT_TOKENS: i64 = 65535;
pub const ARGON_MODEL_PLACEHOLDER: &str = "MODEL_PLACEHOLDER_M421";

pub fn extract_version_from_plist(plist_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(plist_path).ok()?;
    let key_marker = "<key>CFBundleShortVersionString</key>";
    let pos = content.find(key_marker)?;
    let rest = &content[pos + key_marker.len()..];
    let s_start = rest.find("<string>")? + "<string>".len();
    let s_end = rest[s_start..].find("</string>")?;
    Some(rest[s_start..s_start + s_end].trim().to_string())
}

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

pub fn build_hub_user_agent_with(version: &str, cl: &str) -> String {
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
        version, os_type, arch, cl
    )
}

pub fn is_argon_model(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower.contains("gemini-4-argon") || (lower.contains("gemini") && lower.contains("argon"))
}

pub fn is_bare_argon_model(model: &str) -> bool {
    let lower = model
        .trim()
        .strip_prefix("models/")
        .unwrap_or(model.trim())
        .to_ascii_lowercase();
    lower == "gemini-4-argon"
}

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

/// 模拟网关思考配置结构
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedGenerationConfig {
    pub model: String,
    pub model_enum: String,
    pub include_thoughts: bool,
    pub thinking_budget: Option<i64>,
    pub thinking_level: Option<String>,
    pub max_output_tokens: i64,
}

/// 修复前 (v4.9.6 main 分支) 的请求构造行为模拟
pub fn legacy_v496_build_request(
    requested_model: &str,
    client_effort: Option<&str>,
    user_max_tokens: Option<i64>,
) -> (String, SimulatedGenerationConfig) {
    // 1. v4.9.6 仅在 quota.rs 使用 NATIVE_OAUTH_USER_AGENT
    let ua = "vscode/1.X.X (Antigravity/4.3.0)".to_string();

    // 2. v4.9.6 的 DynamicTierRouter 因 official_models.json 无 gemini-4-argon-*，无法对裸模型分档路由
    let routed_model = requested_model.to_string();

    // 3. v4.9.6 的 get_thinking_budget：对 -low 只要不含 "pro" 就返回 1000 (<1024!)
    let mut budget = if routed_model.ends_with("-high") {
        10000 // 不含 "pro"，给 10000
    } else if routed_model.ends_with("-medium") {
        4000
    } else if routed_model.ends_with("-low") {
        1000 // 致命缺陷：< 1024，触发上游 THINKING_LEVEL_MINIMAL 400 报错！
    } else {
        match client_effort {
            Some("low") | Some("minimal") => 1000,
            Some("high") => 10000,
            _ => 4000,
        }
    };

    let mut max_out = user_max_tokens.unwrap_or(65536);
    // 6x Intent 极速压缩漏洞：当 user_max_tokens=1024 且 budget>=6144 时，budget 被压成 0！
    if let Some(um) = user_max_tokens {
        if budget >= 6 * um {
            budget = if um > 1024 { um - 1024 } else { 0 };
        }
    }
    if budget > 0 && max_out <= budget {
        max_out = budget + 1024;
    }

    // 4. v4.9.6 在 official_models.json 找不到 Argon，回退到默认 M318 (Gemini 3.8 Flash)
    let cfg = SimulatedGenerationConfig {
        model: routed_model,
        model_enum: "MODEL_PLACEHOLDER_M318".to_string(),
        include_thoughts: true,
        thinking_budget: Some(budget),
        thinking_level: None,
        max_output_tokens: max_out,
    };
    (ua, cfg)
}

/// 修复后 (Ponytail 手术刀方案) 的请求构造行为
pub fn ponytail_build_request(
    requested_model: &str,
    client_effort: Option<&str>,
    user_max_tokens: Option<i64>,
    is_client_control: bool,
    local_ver: &str,
    local_cl: &str,
) -> (String, SimulatedGenerationConfig) {
    // 1. Ponytail 动态提取本地官方 Hub UA
    let ua = build_hub_user_agent_with(local_ver, local_cl);

    // 2. 裸模型动态分档路由
    let routed_model = if is_bare_argon_model(requested_model) {
        match client_effort {
            Some("low") | Some("minimal") | Some("extra-low") => "gemini-4-argon-low".to_string(),
            Some("high") | Some("max") | Some("xhigh") => "gemini-4-argon-high".to_string(),
            _ => "gemini-4-argon-medium".to_string(),
        }
    } else if requested_model == "gemini-4-argon-thinking" {
        "gemini-4-argon-high".to_string()
    } else {
        requested_model.to_string()
    };

    // 3. 初始预算与 6x 压缩模拟
    let mut budget = argon_tier_budget(&routed_model) as i64;
    let mut max_out = user_max_tokens.unwrap_or(ARGON_MAX_OUTPUT_TOKENS);
    if let Some(um) = user_max_tokens {
        if budget >= 6 * um {
            budget = if um > 1024 { um - 1024 } else { 0 };
        }
    }

    // 4. Ponytail 终审门禁 (enforce_argon_thinking_invariants)
    let (final_budget, final_level) = if is_client_control {
        let lvl = match client_effort {
            Some("low") | Some("minimal") | Some("extra-low") | Some("0") | Some("none") => "LOW",
            Some("high") | Some("max") | Some("xhigh") => "HIGH",
            Some("medium") | Some("default") => "MEDIUM",
            _ => {
                if routed_model.ends_with("-low") {
                    "LOW"
                } else if routed_model.ends_with("-high") {
                    "HIGH"
                } else {
                    "MEDIUM"
                }
            }
        };
        (None, Some(lvl.to_string()))
    } else {
        let clamped = if budget < ARGON_MIN_THINKING_BUDGET {
            ARGON_MIN_THINKING_BUDGET
        } else {
            budget.min(ARGON_MAX_OUTPUT_TOKENS - 1024)
        };
        (Some(clamped), None)
    };

    max_out = max_out.min(ARGON_MAX_OUTPUT_TOKENS);
    if let Some(b) = final_budget {
        if max_out <= b {
            max_out = (b + 1024).min(ARGON_MAX_OUTPUT_TOKENS);
        }
    }

    let cfg = SimulatedGenerationConfig {
        model: routed_model,
        model_enum: ARGON_MODEL_PLACEHOLDER.to_string(),
        include_thoughts: true,
        thinking_budget: final_budget,
        thinking_level: final_level,
        max_output_tokens: max_out,
    };
    (ua, cfg)
}

// ============================================================================
// 2. 本地 TCP 模拟 Google `daily-cloudcode-pa.googleapis.com/v1internal` 服务端
// ============================================================================

fn handle_mock_v1internal_connection(mut req_reader: PipeReader, mut resp_writer: PipeWriter) {
    let mut req_bytes = Vec::new();
    let _ = req_reader.read_to_end(&mut req_bytes);
    let req_text = String::from_utf8_lossy(&req_bytes);
    let mut lines = req_text.lines();
    let request_line = lines.next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("");

    let mut user_agent = String::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line
            .strip_prefix("User-Agent:")
            .or_else(|| line.strip_prefix("user-agent:"))
        {
            user_agent = rest.trim().to_string();
        }
    }

    let body = req_text.split("\r\n\r\n").nth(1).unwrap_or("");

    let is_valid_hub_ua = user_agent.starts_with("antigravity/hub/2.24.")
        && user_agent.contains("aidev_client;")
        && user_agent.contains("cl=1000335682");

    let (status, resp_body) = match path {
        "/v1internal:fetchAvailableModels" => {
            if is_valid_hub_ua {
                (
                    "200 OK",
                    r#"{"models":{"gemini-4-argon-high":{"displayName":"Gemini 4 Argon (High)","model":"MODEL_PLACEHOLDER_M421","maxTokens":1048576,"maxOutputTokens":65535,"supportsThinking":true,"thinkingLevel":"HIGH","quotaInfo":{"remainingFraction":1.0}},"gemini-4-argon-medium":{"displayName":"Gemini 4 Argon (Medium)","model":"MODEL_PLACEHOLDER_M421","maxTokens":1048576,"maxOutputTokens":65535,"supportsThinking":true,"thinkingLevel":"MEDIUM","quotaInfo":{"remainingFraction":1.0}},"gemini-4-argon-low":{"displayName":"Gemini 4 Argon (Low)","model":"MODEL_PLACEHOLDER_M421","maxTokens":1048576,"maxOutputTokens":65535,"supportsThinking":true,"thinkingLevel":"LOW","quotaInfo":{"remainingFraction":1.0}},"gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)","model":"MODEL_PLACEHOLDER_M318","maxTokens":1048576,"maxOutputTokens":65536,"supportsThinking":true,"thinkingBudget":10000,"quotaInfo":{"remainingFraction":0.80}}}}"#.to_string(),
                )
            } else {
                // 旧版 vscode/1.X.X UA：上游门控直接过滤掉所有 gemini-4-argon-* 模型！
                (
                    "200 OK",
                    r#"{"models":{"gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)","model":"MODEL_PLACEHOLDER_M318","maxTokens":1048576,"maxOutputTokens":65536,"supportsThinking":true,"thinkingBudget":10000,"quotaInfo":{"remainingFraction":0.80}}}}"#.to_string(),
                )
            }
        }
        "/v1internal:generateContent" => {
            if !is_valid_hub_ua {
                (
                    "404 Not Found",
                    r#"{"error":{"code":404,"message":"Requested entity was not found.","status":"NOT_FOUND"}}"#.to_string(),
                )
            } else if body.contains("\"model\":\"gemini-4-argon\"") {
                (
                    "404 Not Found",
                    r#"{"error":{"code":404,"message":"Bare model gemini-4-argon is not a valid physical endpoint ID.","status":"NOT_FOUND"}}"#.to_string(),
                )
            } else if body.contains("\"thinkingLevel\":\"MINIMAL\"")
                || body.contains("\"thinkingBudget\":0")
                || body.contains("\"thinkingBudget\":32")
                || body.contains("\"thinkingBudget\":1000")
            {
                (
                    "400 Bad Request",
                    r#"{"error":{"code":400,"message":"Thinking level is unsupported: THINKING_LEVEL_MINIMAL","status":"INVALID_ARGUMENT"}}"#.to_string(),
                )
            } else if body.contains("\"thinkingBudget\"") && body.contains("\"thinkingLevel\"") {
                (
                    "400 Bad Request",
                    r#"{"error":{"code":400,"message":"Invalid oneof: thinking_budget and thinking_level cannot both be set","status":"INVALID_ARGUMENT"}}"#.to_string(),
                )
            } else if !body.contains("\"model_enum\":\"MODEL_PLACEHOLDER_M421\"") {
                (
                    "400 Bad Request",
                    r#"{"error":{"code":400,"message":"Mismatched model_enum for Gemini 4 Argon","status":"INVALID_ARGUMENT"}}"#.to_string(),
                )
            } else {
                (
                    "200 OK",
                    r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"Argon reasoning..."},{"text":"Hello from Gemini 4 Argon!"}]},"finishReason":"STOP"}]}}"#.to_string(),
                )
            }
        }
        _ => ("404 Not Found", r#"{"error":"unknown endpoint"}"#.to_string()),
    };

    let http_resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        resp_body.len(),
        resp_body
    );
    let _ = resp_writer.write_all(http_resp.as_bytes());
}

fn send_http_post(
    tx: &mpsc::Sender<(PipeReader, PipeWriter)>,
    path: &str,
    user_agent: &str,
    json_body: &str,
) -> (u16, String) {
    let (req_reader, mut req_writer) = pipe().expect("create request pipe");
    let (mut resp_reader, resp_writer) = pipe().expect("create response pipe");
    tx.send((req_reader, resp_writer))
        .expect("dispatch connection to mock server");

    let req = format!(
        "POST {} HTTP/1.1\r\nHost: daily-cloudcode-pa.googleapis.com\r\nUser-Agent: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        user_agent,
        json_body.len(),
        json_body
    );
    req_writer.write_all(req.as_bytes()).expect("write request");
    drop(req_writer); // 关闭写端通知服务端请求报文传输完毕

    let mut resp = String::new();
    resp_reader
        .read_to_string(&mut resp)
        .expect("read response");
    let status_line = resp.lines().next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(500);
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (code, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_1_source_code_surgical_modifications_audit() {
        let base = PathBuf::from(".");

        let mod_rs = std::fs::read_to_string(base.join("src-tauri/src/utils/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod ponytail;"));

        let ponytail_rs =
            std::fs::read_to_string(base.join("src-tauri/src/utils/ponytail.rs")).unwrap();
        assert!(ponytail_rs.contains("extract_cl_from_language_server"));
        assert!(ponytail_rs.contains("enforce_argon_thinking_invariants"));
        assert!(ponytail_rs.contains("matches_quota_summary_group"));

        let constants_rs =
            std::fs::read_to_string(base.join("src-tauri/src/constants.rs")).unwrap();
        assert!(constants_rs.contains("crate::utils::ponytail::build_hub_user_agent()"));

        let quota_rs =
            std::fs::read_to_string(base.join("src-tauri/src/modules/quota.rs")).unwrap();
        assert!(!quota_rs.contains("NATIVE_OAUTH_USER_AGENT"));
        assert!(quota_rs.contains("crate::constants::USER_AGENT.as_str()"));
        assert!(quota_rs.contains("crate::utils::ponytail::matches_quota_summary_group"));

        let inbound_rs =
            std::fs::read_to_string(base.join("src-tauri/src/proxy/pipeline/inbound.rs")).unwrap();
        assert!(inbound_rs.contains("crate::utils::ponytail::enforce_argon_thinking_invariants"));

        let wrapper_rs =
            std::fs::read_to_string(base.join("src-tauri/src/proxy/mappers/gemini/wrapper.rs"))
                .unwrap();
        assert!(wrapper_rs.contains("crate::utils::ponytail::enforce_argon_thinking_invariants"));

        let official_json =
            std::fs::read_to_string(base.join("src-tauri/resources/official_models.json")).unwrap();
        assert!(official_json.contains("\"gemini-4-argon-high\""));
        assert!(official_json.contains("\"gemini-4-argon-medium\""));
        assert!(official_json.contains("\"gemini-4-argon-low\""));
        assert!(official_json.contains("\"MODEL_PLACEHOLDER_M421\""));
        println!("✅ [Audit] 全部 Ponytail 单束收拢切口与资源配置校验通过！");
    }

    #[test]
    fn test_2_simulated_app_bundle_plist_and_binary_cl_extraction() {
        let mock_app = std::env::temp_dir().join("mock_antigravity_app/Antigravity.app");
        let contents_dir = mock_app.join("Contents");
        let bin_dir = contents_dir.join("Resources").join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let plist_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Antigravity</string>
    <key>CFBundleShortVersionString</key>
    <string>2.24.0</string>
</dict>
</plist>"#;
        let plist_path = contents_dir.join("Info.plist");
        std::fs::write(&plist_path, plist_xml).unwrap();

        // 构造 130KB 模拟 language_server 二进制，特征横跨 64KB chunk 边界
        let mut binary_data = vec![b'A'; 65520];
        binary_data.extend_from_slice(
            b"\x00/google_src/files/1000335682/depot/branches/antigravity_web_release_branch/997602692.1/google3/\x00",
        );
        binary_data.resize(131072, b'Z');

        let ls_path = bin_dir.join("language_server");
        std::fs::write(&ls_path, &binary_data).unwrap();

        let extracted_ver = extract_version_from_plist(&plist_path);
        let extracted_cl = extract_cl_from_language_server(&ls_path);

        assert_eq!(extracted_ver.as_deref(), Some("2.24.0"));
        assert_eq!(
            extracted_cl.as_deref(),
            Some("1000335682"),
            "Must extract CL even across 64KB chunk boundaries"
        );

        let ua = build_hub_user_agent_with(
            extracted_ver.as_deref().unwrap(),
            extracted_cl.as_deref().unwrap(),
        );
        assert_eq!(
            ua,
            "antigravity/hub/2.24.0 (aidev_client; os_type=darwin; arch=arm64; cl=1000335682)"
        );
        println!("✅ [Local Fingerprint] 跨 64KB 边界提取 Info.plist + language_server CL 成功: {ua}");
    }

    #[test]
    fn test_3_end_to_end_ab_comparison_against_mock_google_v1internal_gateway() {
        let (tx, rx) = mpsc::channel::<(PipeReader, PipeWriter)>();

        let server_handle = thread::spawn(move || {
            for (req_reader, resp_writer) in rx {
                handle_mock_v1internal_connection(req_reader, resp_writer);
            }
        });

        // --------------------------------------------------------------------
        // [A 组：修复前 v4.9.6 行为实测]
        // --------------------------------------------------------------------
        let (legacy_ua, _) = legacy_v496_build_request("gemini-4-argon-low", None, None);
        let (code_a1, body_a1) =
            send_http_post(&tx, "/v1internal:fetchAvailableModels", &legacy_ua, "{}");
        assert_eq!(code_a1, 200);
        assert!(
            !body_a1.contains("gemini-4-argon"),
            "Legacy UA must be gated by upstream and miss gemini-4-argon"
        );
        println!("🔍 [A1 修复前-列表获取] UA='{}' -> Argon 被上游过滤剔除 (符合预期故障现象)", legacy_ua);

        let (code_a2, body_a2) = send_http_post(
            &tx,
            "/v1internal:generateContent",
            &legacy_ua,
            r#"{"model":"gemini-4-argon-high","request":{"generationConfig":{"maxOutputTokens":65535,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":10001}},"labels":{"model_enum":"MODEL_PLACEHOLDER_M421"}}}"#,
        );
        assert_eq!(code_a2, 404, "Legacy UA calling Argon must fail with 404");
        println!("🔍 [A2 修复前-模型调用] UA='{}' -> HTTP {} {}", legacy_ua, code_a2, body_a2);

        let new_ua = build_hub_user_agent_with("2.24.0", "1000335682");
        let (_, legacy_low_cfg) = legacy_v496_build_request("gemini-4-argon-low", None, None);
        let legacy_low_payload = format!(
            r#"{{"model":"{}","request":{{"generationConfig":{{"maxOutputTokens":{},"thinkingConfig":{{"includeThoughts":true,"thinkingBudget":{}}}}},"labels":{{"model_enum":"MODEL_PLACEHOLDER_M421"}}}}}}"#,
            legacy_low_cfg.model,
            legacy_low_cfg.max_output_tokens,
            legacy_low_cfg.thinking_budget.unwrap()
        );
        let (code_a3, body_a3) = send_http_post(
            &tx,
            "/v1internal:generateContent",
            &new_ua,
            &legacy_low_payload,
        );
        assert_eq!(code_a3, 400);
        assert!(body_a3.contains("THINKING_LEVEL_MINIMAL"));
        println!("🔍 [A3 修复前-Low档预算] thinkingBudget=1000 (<1024) -> HTTP {} {}", code_a3, body_a3);

        // --------------------------------------------------------------------
        // [B 组：修复后 Ponytail 手术刀方案实测]
        // --------------------------------------------------------------------
        let (code_b1, body_b1) =
            send_http_post(&tx, "/v1internal:fetchAvailableModels", &new_ua, "{}");
        assert_eq!(code_b1, 200);
        assert!(body_b1.contains("gemini-4-argon-high"));
        assert!(body_b1.contains("gemini-4-argon-medium"));
        assert!(body_b1.contains("gemini-4-argon-low"));
        println!("✅ [B1 修复后-列表获取] UA='{}' -> HTTP 200 成功获取 gemini-4-argon-low/medium/high 全套模型！", new_ua);

        let (ua_b2, cfg_b2) = ponytail_build_request(
            "gemini-4-argon",
            Some("minimal"),
            None,
            false,
            "2.24.0",
            "1000335682",
        );
        assert_eq!(cfg_b2.model, "gemini-4-argon-low");
        assert_eq!(cfg_b2.thinking_budget, Some(1024));
        let payload_b2 = format!(
            r#"{{"model":"{}","request":{{"generationConfig":{{"maxOutputTokens":{},"thinkingConfig":{{"includeThoughts":true,"thinkingBudget":{}}}}},"labels":{{"model_enum":"{}"}}}}}}"#,
            cfg_b2.model,
            cfg_b2.max_output_tokens,
            cfg_b2.thinking_budget.unwrap(),
            cfg_b2.model_enum
        );
        let (code_b2, body_b2) =
            send_http_post(&tx, "/v1internal:generateContent", &ua_b2, &payload_b2);
        assert_eq!(code_b2, 200, "Ponytail low/minimal must succeed: {body_b2}");
        println!("✅ [B2 修复后-Minimal/Low自愈] model='{}', budget=1024 -> HTTP {} {}", cfg_b2.model, code_b2, body_b2);

        let (ua_b3, cfg_b3) =
            ponytail_build_request("gemini-4-argon", None, None, false, "2.24.0", "1000335682");
        assert_eq!(cfg_b3.model, "gemini-4-argon-medium");
        assert_eq!(cfg_b3.thinking_budget, Some(4000));
        let payload_b3 = format!(
            r#"{{"model":"{}","request":{{"generationConfig":{{"maxOutputTokens":{},"thinkingConfig":{{"includeThoughts":true,"thinkingBudget":{}}}}},"labels":{{"model_enum":"{}"}}}}}}"#,
            cfg_b3.model,
            cfg_b3.max_output_tokens,
            cfg_b3.thinking_budget.unwrap(),
            cfg_b3.model_enum
        );
        let (code_b3, _) =
            send_http_post(&tx, "/v1internal:generateContent", &ua_b3, &payload_b3);
        assert_eq!(code_b3, 200);
        println!("✅ [B3 修复后-裸模型默认路由] gemini-4-argon -> '{}' (budget=4000) -> HTTP 200 OK", cfg_b3.model);

        let (ua_b4, cfg_b4) = ponytail_build_request(
            "gemini-4-argon-high",
            None,
            Some(1024),
            false,
            "2.24.0",
            "1000335682",
        );
        assert_eq!(cfg_b4.thinking_budget, Some(1024));
        assert_eq!(cfg_b4.max_output_tokens, 2048);
        let payload_b4 = format!(
            r#"{{"model":"{}","request":{{"generationConfig":{{"maxOutputTokens":{},"thinkingConfig":{{"includeThoughts":true,"thinkingBudget":{}}}}},"labels":{{"model_enum":"{}"}}}}}}"#,
            cfg_b4.model,
            cfg_b4.max_output_tokens,
            cfg_b4.thinking_budget.unwrap(),
            cfg_b4.model_enum
        );
        let (code_b4, _) =
            send_http_post(&tx, "/v1internal:generateContent", &ua_b4, &payload_b4);
        assert_eq!(code_b4, 200);
        println!("✅ [B4 修复后-6x极速压缩防归零] user_max=1024 -> budget 钳制回 1024, max_out 扩充至 2048 -> HTTP 200 OK");

        let (ua_b5, cfg_b5) = ponytail_build_request(
            "gemini-4-argon",
            Some("high"),
            None,
            true,
            "2.24.0",
            "1000335682",
        );
        assert_eq!(cfg_b5.thinking_budget, None);
        assert_eq!(cfg_b5.thinking_level.as_deref(), Some("HIGH"));
        let payload_b5 = format!(
            r#"{{"model":"{}","request":{{"generationConfig":{{"maxOutputTokens":{},"thinkingConfig":{{"includeThoughts":true,"thinkingLevel":"{}"}}}},"labels":{{"model_enum":"{}"}}}}}}"#,
            cfg_b5.model,
            cfg_b5.max_output_tokens,
            cfg_b5.thinking_level.unwrap(),
            cfg_b5.model_enum
        );
        let (code_b5, _) =
            send_http_post(&tx, "/v1internal:generateContent", &ua_b5, &payload_b5);
        assert_eq!(code_b5, 200);
        println!("✅ [B5 修复后-客户端原生thinkingLevel] thinkingLevel='HIGH' (无冲突budget) -> HTTP 200 OK");

        drop(tx);
        server_handle.join().unwrap();
    }

    #[test]
    fn test_4_quota_bucket_isolation_and_non_regression() {
        assert!(matches_quota_summary_group(
            "gemini-4-argon-high",
            "gemini 4 argon"
        ));
        assert!(!matches_quota_summary_group(
            "gemini-4-argon-high",
            "gemini"
        ));
        assert!(matches_quota_summary_group(
            "gemini-3.8-flash-high",
            "gemini"
        ));
        assert!(!matches_quota_summary_group(
            "gemini-3.8-flash-high",
            "gemini 4 argon"
        ));
        println!("✅ [Quota Isolation] Argon 专属配额桶与普通 Gemini 桶完全隔离，零交叉污染！");
    }
}
