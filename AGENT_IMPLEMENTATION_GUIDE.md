# Gemini 4 Argon "Ponytail" 手术刀方案 —— 架构实施与复现指南

> **目标受众**：原项目开发者、开源贡献者，以及任何需要 100% 精确复现该功能的 AI Coding Agent。  
> **设计哲学**：**Ponytail（单束收拢、最小侵入）**。将复杂的云端网关探测、逆向指纹对抗、动态二进制扫描与思考协议校验完全封装于单一 Rust 核心切面模块（`ponytail.rs`），对外暴露极简纯函数接口；对主干代码的侵入仅限于单行挂载，绝不破坏现有架构契约。

---

## 目录

- [一、核心架构设计与防线概览](#一核心架构设计与防线概览)
- [二、新增核心模块：`src-tauri/src/utils/ponytail.rs`](#二新增核心模块src-taurisrcutilsponytailrs)
  - [1. 常量与权威基线定义](#1-常量与权威基线定义)
  - [2. 客户端指纹探测与 64KB 流式 CL 扫描](#2-客户端指纹探测与-64kb-流式-cl-扫描)
  - [3. 出站 Hub User-Agent 构造器](#3-出站-hub-user-agent-构造器)
  - [4. 模型识别、分档路由与配额桶隔离](#4-模型识别分档路由与配额桶隔离)
  - [5. ThinkingConfig 第一性原理终审守门员](#5-thinkingconfig-第一性原理终审守门员)
- [三、主干 11 个关联文件的精确挂载点与修改规范](#三主干-11-个关联文件的精确挂载点与修改规范)
  - [1. `src-tauri/src/utils/mod.rs`（模块导出）](#1-src-taurisrcutilsmodrs模块导出)
  - [2. `src-tauri/src/constants.rs`（全局 User-Agent）](#2-src-taurisrcconstantsrs全局-user-agent)
  - [3. `src-tauri/src/modules/quota.rs`（配额桶精准隔离）](#3-src-taurisrcmodulesquotars配额桶精准隔离)
  - [4. `src-tauri/src/proxy/common/model_mapping.rs`（上游模型映射与路由）](#4-src-taurisrcproxycommonmodel_mappingrs上游模型映射与路由)
  - [5. `src-tauri/src/proxy/common/variant_mapping.rs`（思考变体分发）](#5-src-taurisrcproxycommonvariant_mappingrs思考变体分发)
  - [6. `src-tauri/src/proxy/model_specs.rs`（模型规范配置）](#6-src-taurisrcproxymodel_specsrs模型规范配置)
  - [7. `src-tauri/src/proxy/pipeline/inbound.rs`（入站思考不变量终审）](#7-src-taurisrcproxypipelineinboundrs入站思考不变量终审)
  - [8. `src-tauri/src/proxy/mappers/gemini/wrapper.rs`（意图压缩后兜底防线）](#8-src-taurisrcproxymappersgeminiwrapperrs意图压缩后兜底防线)
  - [9. `src-tauri/resources/official_models.json`（官方模型元数据）](#9-src-tauriresourcesofficial_modelsjson官方模型元数据)
  - [10. `src-tauri/resources/model_specs.json`（静态 Token 与预算规范）](#10-src-tauriresourcesmodel_specsjson静态-token-与预算规范)
  - [11. `src/config/modelConfig.ts`（前端 UI 模型与档位映射）](#11-srcconfigmodelconfigts前端-ui-模型与档位映射)
- [四、验证与端到端自动化测试](#四验证与端到端自动化测试)

---

## 一、核心架构设计与防线概览

实现 `Gemini 4 Argon` 完美支持的核心难点在于平衡：
1. **上游云端网关的严苛门控**（必须具备官方 Hub 身份与对应构建 CL，否则静默隐藏模型或 404）；
2. **Protobuf 思考协议的硬性边界**（`thinkingBudget >= 1024`，且上游禁用 `THINKING_LEVEL_MINIMAL`，否则 400）；
3. **网关现有管道的压缩逻辑**（如 `6x Intent` 极速压缩算法极易将思考预算压降至 0 或 <1024，导致出站请求崩溃）。

为此，Ponytail 方案构建了四道防线：
- **防线 1（Identity Gate）**：优先提取本地官方安装包指纹，动态流式扫描 190MB 二进制内的 `buildChangelist`，辅以官方权威保底；
- **防线 2（Routing Gate）**：裸模型 `gemini-4-argon` 统一路由至 `gemini-4-argon-medium`，配额按独立 Bucket 结算；
- **防线 3（Pipeline First Inbound Gate）**：在网关解析入站请求后，由守门员检查并修复缺失或 `-1` 预算；
- **防线 4（Egress Wrapper Defense）**：在出站转换层、特别是各类意图压缩算法执行完毕后，执行最终不变量守门，保障绝对不出格。

---

## 二、新增核心模块：`src-tauri/src/utils/ponytail.rs`

在 `src-tauri/src/utils/` 下新建 `ponytail.rs`。本模块依赖标准库与 `serde_json`，零多余第三方依赖。

### 1. 常量与权威基线定义

```rust
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// 官方 Antigravity.app (v2.24.0) 逆向验证的权威保底 Hub 版本号
pub const FALLBACK_HUB_VERSION: &str = "2.24.0";

/// 官方 language_server (0x1093eb0e0 buildChangelist) 逆向验证的权威保底 CL
pub const FALLBACK_HUB_CL: &str = "1000335682";

/// Argon 最小合法思考预算（Google v1internal 会将 < 1024 映射为 THINKING_LEVEL_MINIMAL 并返回 400）
pub const ARGON_MIN_THINKING_BUDGET: i64 = 1024;
pub const ARGON_MEDIUM_THINKING_BUDGET: i64 = 4000;
pub const ARGON_HIGH_THINKING_BUDGET: i64 = 10001;
pub const ARGON_MAX_OUTPUT_TOKENS: i64 = 65535;
pub const ARGON_MODEL_PLACEHOLDER: &str = "MODEL_PLACEHOLDER_M421";
```

### 2. 客户端指纹探测与 64KB 流式 CL 扫描

无需使用外部 `strings` 或将数百兆的 `language_server` 整体读入内存。采用带 128 字节重叠缓冲的 64KB 块滑动窗口算法，高效查找 Google Piper 编译分支标记：
- 前缀：`b"/google_src/files/"`
- 变量：8-12 位数字 CL 号
- 后缀：`b"/depot/branches/antigravity_web_release_branch/"`

```rust
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
```

### 3. 出站 Hub User-Agent 构造器

```rust
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
```

### 4. 模型识别、分档路由与配额桶隔离

```rust
#[inline]
pub fn is_argon_model(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower.contains("gemini-4-argon") || (lower.contains("gemini") && lower.contains("argon"))
}

#[inline]
pub fn is_bare_argon_model(model: &str) -> bool {
    let lower = model
        .trim()
        .strip_prefix("models/")
        .unwrap_or(model.trim())
        .to_ascii_lowercase();
    lower == "gemini-4-argon"
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
```

### 5. ThinkingConfig 第一性原理终审守门员

```rust
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

    // 1. 规范化 thinkingLevel：禁止 MINIMAL
    if let Some(level_str) = tc_obj.get("thinkingLevel").and_then(Value::as_str) {
        let l_upper = level_str.to_ascii_uppercase();
        if l_upper == "THINKING_LEVEL_MINIMAL"
            || l_upper == "MINIMAL"
            || l_upper == "NONE"
            || l_upper == "DISABLED"
            || l_upper == "0"
        {
            tc_obj.insert("thinkingLevel".to_string(), json!("LOW"));
        }
    }

    // 2. 规范化 thinkingBudget：保证 >= 1024，解决 -1 或被 6x Intent 压缩归零问题
    let current_budget = tc_obj.get("thinkingBudget").and_then(Value::as_i64);
    let final_budget = match current_budget {
        Some(b) if b <= 0 => tier_default_budget,
        Some(b) if b < ARGON_MIN_THINKING_BUDGET => {
            if is_client_control {
                ARGON_MIN_THINKING_BUDGET
            } else {
                tier_default_budget
            }
        }
        Some(b) => b,
        None => tier_default_budget,
    };

    tc_obj.insert("thinkingBudget".to_string(), json!(final_budget));

    // 3. 避免 Protobuf oneof 互斥冲突：在存在明确数值型预算时清理冲突的 level
    if tc_obj.contains_key("thinkingBudget") {
        if let Some(lvl) = tc_obj.get("thinkingLevel").and_then(Value::as_str) {
            if lvl.eq_ignore_ascii_case("MINIMAL")
                || lvl.eq_ignore_ascii_case("THINKING_LEVEL_MINIMAL")
            {
                tc_obj.remove("thinkingLevel");
            }
        }
    }

    // 4. 联动扩充 maxOutputTokens：必须 > thinkingBudget 且 <= 65535
    let current_max = gc_obj.get("maxOutputTokens").and_then(Value::as_i64);
    let min_required_output = (final_budget + 1024).min(ARGON_MAX_OUTPUT_TOKENS);
    match current_max {
        Some(m) if m <= final_budget => {
            gc_obj.insert("maxOutputTokens".to_string(), json!(min_required_output));
        }
        None => {
            gc_obj.insert("maxOutputTokens".to_string(), json!(ARGON_MAX_OUTPUT_TOKENS));
        }
        _ => {}
    }
}
```

---

## 三、主干 11 个关联文件的精确挂载点与修改规范

### 1. `src-tauri/src/utils/mod.rs`（模块导出）
在文件末尾追加导出：
```rust
pub mod ponytail;
```

### 2. `src-tauri/src/constants.rs`（全局 User-Agent）
将出站 `USER_AGENT` 绑定为官方动态 Hub UA：
```rust
pub const OFFICIAL_HUB_VERSION: &str = crate::utils::ponytail::FALLBACK_HUB_VERSION;
pub const OFFICIAL_HUB_CL: &str = crate::utils::ponytail::FALLBACK_HUB_CL;

pub fn build_official_upstream_user_agent() -> String {
    crate::utils::ponytail::build_hub_user_agent()
}

pub static USER_AGENT: LazyLock<String> = LazyLock::new(build_official_upstream_user_agent);
```
并在 `sanitize_egress_user_agent` 中放行 `antigravity/hub/` 开头的官方合法标识。

### 3. `src-tauri/src/modules/quota.rs`（配额桶精准隔离）
1. 移除文件顶部的旧版硬编码 `NATIVE_OAUTH_USER_AGENT`，出站请求统一使用 `crate::constants::USER_AGENT.as_str()`；
2. 在 `retrieveUserQuotaSummary` 处理组匹配处（约第 433 行），将简单的字符串包含替换为单束匹配：
```rust
let matches_group = crate::utils::ponytail::matches_quota_summary_group(
    &model_name_lower,
    &group_name_lower,
);
```

### 4. `src-tauri/src/proxy/common/model_mapping.rs`（上游模型映射与路由）
在 `normalize_to_gemini` / `normalize_to_claude` 映射表中补充 Argon 映射规则：
```rust
// 裸模型 gemini-4-argon 默认指向基准档位 medium
"gemini-4-argon" => "gemini-4-argon-medium",
"gemini-4-argon-high" => "gemini-4-argon-high",
"gemini-4-argon-medium" => "gemini-4-argon-medium",
"gemini-4-argon-low" => "gemini-4-argon-low",
```
同时在 `MODEL_PLACEHOLDER_M421` 占位映射中返回 `gemini-4-argon-high`。

### 5. `src-tauri/src/proxy/common/variant_mapping.rs`（思考变体分发）
在根据客户端 temperature / thinking level 选择底层变体时，挂载 `is_argon_model` 逻辑：
- `high` / `max` / `extreme` -> `gemini-4-argon-high`
- `low` / `extra-low` -> `gemini-4-argon-low`
- 其它默认 -> `gemini-4-argon-medium`

### 6. `src-tauri/src/proxy/model_specs.rs`（模型规范配置）
在 `ModelSpecs` 的内置模型定义表中注册 Argon 模型规格：
```rust
"gemini-4-argon" => ModelSpec { max_output_tokens: 65535, thinking_budget: 4000, is_thinking: true },
"gemini-4-argon-high" => ModelSpec { max_output_tokens: 65535, thinking_budget: 10001, is_thinking: true },
"gemini-4-argon-medium" => ModelSpec { max_output_tokens: 65535, thinking_budget: 4000, is_thinking: true },
"gemini-4-argon-low" => ModelSpec { max_output_tokens: 65535, thinking_budget: 1024, is_thinking: true },
```

### 7. `src-tauri/src/proxy/pipeline/inbound.rs`（入站思考不变量终审）
在 `apply_thinking_budget` 函数末尾（约第 1288 行），加入 Ponytail 终审守门员调用：
```rust
// [Ponytail] Gemini 4 Argon 第一性原理不变量终审门禁（收口防止 <1024 / MINIMAL / 6x 归零 / -1 覆盖）
crate::utils::ponytail::enforce_argon_thinking_invariants(
    generation_config,
    target_model,
    client_effort,
    tb_config.control_source == crate::proxy::config::ThinkingControlSource::Client,
);
```

### 8. `src-tauri/src/proxy/mappers/gemini/wrapper.rs`（意图压缩后兜底防线）
在 `wrap_payload` 执行各类启发式意图压缩（如 `6x Intent`）之后（约第 650 行），添加守门员兜底：
```rust
} else if crate::utils::ponytail::is_argon_model(final_model_name) {
    let is_client_ctrl = crate::proxy::config::get_thinking_budget_config().control_source
        == crate::proxy::config::ThinkingControlSource::Client;
    let mut gc_val = serde_json::Value::Object(std::mem::take(gen_config));
    crate::utils::ponytail::enforce_argon_thinking_invariants(
        &mut gc_val,
        final_model_name,
        None,
        is_client_ctrl,
    );
    if let serde_json::Value::Object(map) = gc_val {
        *gen_config = map;
    }
}
```

### 9. `src-tauri/resources/official_models.json`（官方模型元数据）
在 `models` 数组中追加 `gemini-4-argon` 全系列模型与 `MODEL_PLACEHOLDER_M421` 声明。

### 10. `src-tauri/resources/model_specs.json`（静态 Token 与预算规范）
在 `"models"` 对象中增加：
```json
"gemini-4-argon": {
    "max_output_tokens": 65535,
    "thinking_budget": 10001,
    "is_thinking": true
},
"gemini-4-argon-high": {
    "max_output_tokens": 65535,
    "thinking_budget": 10001,
    "is_thinking": true
},
"gemini-4-argon-medium": {
    "max_output_tokens": 65535,
    "thinking_budget": 4000,
    "is_thinking": true
},
"gemini-4-argon-low": {
    "max_output_tokens": 65535,
    "thinking_budget": 1024,
    "is_thinking": true
}
```

### 11. `src/config/modelConfig.ts`（前端 UI 模型与档位映射）
在 `SUPPORTED_MODELS` 数组中加入：
- `gemini-4-argon` (Medium 思考，4000 tokens)
- `gemini-4-argon-high` (High 思考，10001 tokens)
- `gemini-4-argon-medium` (Medium 思考，4000 tokens)
- `gemini-4-argon-low` (Low 思考，1024 tokens)

---

## 四、验证与端到端自动化测试

在仓库根目录下运行以下命令，即可执行针对本方案的全部自动化断言：

```bash
rustc --test sandbox_verifier.rs -o /tmp/verifier_test && /tmp/verifier_test && rm -f /tmp/verifier_test
```

### 测试涵盖断言清单：
1. **静态代码切面审计**：检查上述 12 个文件所有关键字、函数签名与 JSON 配置；
2. **App Bundle 指纹与跨块流式扫描**：构建包含边界溢出的模拟二进制，验证从二进制中提取 `cl=1000335682` 并组装出合法 User-Agent；
3. **Google 云端网关 A/B 对比**：
   - 验证旧版 vscode UA 在 `fetchAvailableModels` 中被上游屏蔽，新版返回完整 38 模型；
   - 验证旧版 UA 在 `generateContent` 返回 404，新版通过；
   - 验证裸模型 `gemini-4-argon` 返回 404，经 Ponytail 路由后返回 200；
   - 验证 `thinkingBudget < 1024` 或 `THINKING_LEVEL_MINIMAL` 返回 400，经 Ponytail 守门后正常生成；
   - 验证 `6x Intent` 压缩导致 budget 变 0 场景被自动兜底保护；
4. **配额桶隔离与非回归**：验证多模型并发请求下，Argon 与普通 Gemini、Claude 配额统计互不干扰。
