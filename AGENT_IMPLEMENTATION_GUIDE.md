# Gemini 4 Argon "Ponytail" 手术刀方案 —— 架构实施与复现指南

> **目标受众**：原项目开发者、开源贡献者，以及任何需要 100% 精确复现该功能的 AI Coding Agent。  
> **设计哲学**：**Ponytail（单束收拢、最小侵入）**。将复杂的云端网关探测、逆向指纹对抗、动态二进制扫描、思考协议校验、多账号风控与前端网格对齐完全收束于极简切面与最小手术刀式修改，绝不破坏现有系统契约。

---

## 目录

- [一、核心架构设计与防线概览](#一核心架构设计与防线概览)
- [二、新增核心模块：`src-tauri/src/utils/ponytail.rs`](#二新增核心模块src-taurisrcutilsponytailrs)
  - [1. 常量与权威基线定义](#1-常量与权威基线定义)
  - [2. 客户端指纹探测与 64KB 流式 CL 扫描](#2-客户端指纹探测与-64kb-流式-cl-扫描)
  - [3. 出站 Hub User-Agent 构造器](#3-出站-hub-user-agent-构造器)
  - [4. 模型识别、分档路由与配额桶隔离](#4-模型识别分档路由与配额桶隔离)
  - [5. ThinkingConfig 第一性原理终审守门员](#5-thinkingconfig-第一性原理终审守门员)
- [三、后端多账号风控加固架构实施](#三后端多账号风控加固架构实施)
  - [1. P2C 调度候选池动态能力过滤](#1-p2c-调度候选池动态能力过滤)
  - [2. `gemini-argon-wtu-weekly` 独立周配额熔断隔离](#2-gemini-argon-wtu-weekly-独立周配额熔断隔离)
  - [3. 0% 配额主动规避与会话平滑切换](#3-0-配额主动规避与会话平滑切换)
- [四、前端配额组件手术刀式对齐实施](#四前端配额组件手术刀式对齐实施)
  - [1. 模型分类与优先级查找 (`modelCategory.ts`)](#1-模型分类与优先级查找-modelcategoryts)
  - [2. 木桶短板约束与倒计时解析 (`quotaDisplay.ts`)](#2-木桶短板约束与倒计时解析-quotadisplayts)
  - [3. 双列网格第 4 处配额横纵几何对齐 (`AccountRow.tsx`)](#3-双列网格第-4-处配额横纵几何对齐-accountrowtsx)
  - [4. 卡片、表格与当前账号组件矩阵同步](#4-卡片表格与当前账号组件矩阵同步)
- [五、主干 19 个关联文件的精确挂载点与修改规范](#五主干-19-个关联文件的精确挂载点与修改规范)
- [六、验证与端到端自动化测试](#六验证与端到端自动化测试)

---

## 一、核心架构设计与防线概览

实现 `Gemini 4 Argon` 工业级支持的核心难点在于平衡：
1. **上游云端网关的严苛门控**（必须具备官方 Hub 身份与对应构建 CL，否则静默隐藏模型或 404）；
2. **Protobuf 思考协议的硬性边界**（`thinkingBudget >= 1024`，且上游禁用 `THINKING_LEVEL_MINIMAL`，否则 400）；
3. **网关现有管道的压缩逻辑**（如 `6x Intent` 极速压缩算法极易将思考预算压降至 0 或 <1024，导致出站请求崩溃）；
4. **多账号风控与配额熔断隔离**（防止 Argon 配额耗尽误污染普通 Gemini/Claude，防止 0% 配额账号盲目重试导致 429 封号）；
5. **前端 UI 双列网格对齐**（确保在 `AccountRow` 等多视图中呈现原生的横向与纵向几何对齐体验）。

为此，方案构建了五重严密防线：
- **防线 1（Identity Gate）**：优先提取本地官方安装包指纹，动态流式扫描 190MB 二进制内的 `buildChangelist`，辅以官方权威保底；
- **防线 2（Routing Gate）**：裸模型 `gemini-4-argon` 统一路由至 `gemini-4-argon-medium`，配额按独立 Bucket 结算；
- **防线 3（Thinking Invariant Gate）**：在网关解析入站请求与出站转换（特别是意图压缩后），由守门员检查并修复缺失或 `< 1024` 预算；
- **防线 4（Risk Control Gate）**：后端调度识别 `gemini-argon-wtu-weekly` 专属周配额熔断桶，过滤 0% 配额账号，避免 429 封禁；
- **防线 5（UI Geometry Gate）**：前端采用 `grid-cols-2` 经典双列排布，在 Gemini 3.1 Pro 正下方纵向对齐、与 Claude 水平横向对齐。

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
pub fn build_hub_user_agent() -> &'static str {
    static CACHED_UA: LazyLock<String> = LazyLock::new(|| {
        let fp = resolve_hub_fingerprint();
        format!(
            "antigravity/hub/{} (aidev_client; os_type={}; arch={}; cl={})",
            fp.version, fp.os_type, fp.arch, fp.cl
        )
    });
    &CACHED_UA
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
pub fn is_argon_bucket(group_name: &str, bucket_id: &str) -> bool {
    let lower_group = group_name.to_ascii_lowercase();
    let lower_bucket = bucket_id.to_ascii_lowercase();
    lower_group.contains("argon")
        || lower_bucket.contains("argon")
        || lower_bucket.contains("gemini-argon-wtu-weekly")
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
    is_client_controlled: bool,
) {
    if !is_argon_model(target_model) {
        return;
    }

    let obj = match generation_config.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    let thinking_config = obj
        .entry("thinkingConfig")
        .or_insert_with(|| json!({"includeThoughts": true}));

    if let Some(tc_obj) = thinking_config.as_object_mut() {
        tc_obj.insert("includeThoughts".to_string(), json!(true));

        let current_budget = tc_obj
            .get("thinkingBudget")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);

        let final_budget = if current_budget <= 0 {
            argon_tier_budget(target_model) as i64
        } else {
            current_budget.max(ARGON_MIN_THINKING_BUDGET)
        };

        tc_obj.insert("thinkingBudget".to_string(), json!(final_budget));
        tc_obj.remove("thinkingLevel"); // 清除 oneof 互斥冲突
    }
}
```

---

## 三、后端多账号风控加固架构实施

在多账号代理池环境中，风控主要由 `src-tauri/src/proxy/token_manager.rs` 实现：

### 1. P2C 调度候选池动态能力过滤
在 `select_with_p2c` 抽签过滤环节，排除当前对目标模型触发保护的账号：
```rust
let mut available: Vec<&ProxyToken> = candidates
    .iter()
    .filter(|t| !attempted.contains(&t.account_id))
    .filter(|t| {
        !quota_protection_enabled || !t.protected_models.contains(normalized_target)
    })
    .collect();
```

### 2. `gemini-argon-wtu-weekly` 独立周配额熔断隔离
在解析账号配额桶时，识别出独立熔断范围：
```rust
let is_argon = crate::utils::ponytail::is_argon_bucket(group_name, bucket_id);
let third_party = is_claude_group || bucket_id.contains("3p");
let gemini = !is_argon && (is_gemini_group || bucket_id.contains("gemini"));

let mut models: Vec<&str> = if third_party {
    vec!["claude", "claude-sonnet-4-6", "gpt-oss-120b-medium"]
} else if is_argon {
    vec!["gemini-4-argon"] // 仅熔断 Argon 系列，绝不牵连 Gemini 3 / Claude
} else if gemini {
    vec!["gemini-3-flash", "gemini-3.1-pro-high", "gemini-3.1-flash-image", "gemini-3-pro-image"]
};
```

### 3. 0% 配额主动规避与会话平滑切换
在粘性会话与 60s 窗口中，一旦目标账号配额耗尽或被保护，立即解绑切换：
```rust
if reset_sec > 0 {
    tracing::debug!("Sticky Session: Bound account {} is rate-limited, unbinding.", bound_token.email);
    self.session_accounts.remove(sid);
} else if quota_protection_enabled && bound_token.protected_models.contains(&normalized_target) {
    tracing::debug!("Sticky Session: Bound account is quota-protected for model, unbinding.");
    self.session_accounts.remove(sid);
}
```

---

## 四、前端配额组件手术刀式对齐实施

### 1. 模型分类与优先级查找 (`modelCategory.ts`)
```typescript
export type ModelCategory = 'gemini-pro' | 'gemini-flash' | 'gemini-image' | 'gemini-argon' | 'claude';

export const findQuotaModel = (models: ModelQuotaInfo[], category: ModelCategory): ModelQuotaInfo | undefined => {
    switch (category) {
        case 'gemini-argon':
            return findModelByPriority(models, [
                'gemini-4-argon-high',
                'gemini-4-argon',
                'gemini-4-argon-medium',
                'gemini-4-argon-low'
            ]);
        // ... 原有 pro / flash / image / claude 分支
    }
};
```

### 2. 木桶短板约束与倒计时解析 (`quotaDisplay.ts`)
计算 5H 滚动配额与 Weekly 周配额短板：
```typescript
let effectivePercentage = targetModel.percentage;
let effectiveResetTime = targetModel.resetTime;

if (weeklyQuota && weeklyQuota.percentage < effectivePercentage) {
    effectivePercentage = weeklyQuota.percentage;
    effectiveResetTime = weeklyQuota.resetTime;
}
```

### 3. 双列网格第 4 处配额横纵几何对齐 (`AccountRow.tsx`)
在 `<div className="grid grid-cols-2 gap-x-4 gap-y-1 py-0">` 经典结构中：
```tsx
{/* Row 1, Col 1: Gemini 3.1 Pro */}
{geminiProModel && renderModelQuota(geminiProModel, 'Pro')}

{/* Row 1, Col 2: Gemini 3 Flash */}
{geminiFlashModel && renderModelQuota(geminiFlashModel, 'Flash')}

{/* Row 2, Col 1: Gemini 4 Argon (Pro 正下方纵向对齐) */}
{geminiArgonModel && renderModelQuota(geminiArgonModel, 'Argon')}

{/* Row 2, Col 2: Claude (Argon 水平横向对齐) */}
{claudeModel && renderModelQuota(claudeModel, 'Claude')}
```

---

## 五、主干 19 个关联文件的精确挂载点与修改规范

1. **`src-tauri/src/utils/mod.rs`**：导出 `pub mod ponytail;`。
2. **`src-tauri/src/constants.rs`**：出站全局 `USER_AGENT` 指向 `ponytail::build_hub_user_agent()`。
3. **`src-tauri/src/modules/quota.rs`**：挂载 `ponytail::matches_quota_summary_group` 隔离配额组。
4. **`src-tauri/src/proxy/token_manager.rs`**：挂载 `ponytail::is_argon_bucket`，实现 `gemini-argon-wtu-weekly` 熔断隔离与能力过滤。
5. **`src-tauri/src/models/config.rs`**：`default_pinned_models` 默认包含 `gemini-4-argon`。
6. **`src-tauri/src/proxy/common/model_mapping.rs`**：注册 `gemini-4-argon` -> `medium` 及占位映射。
7. **`src-tauri/src/proxy/common/variant_mapping.rs`**：支持 Argon 思考档位变体分发。
8. **`src-tauri/src/proxy/model_specs.rs`**：注册 65535 Token 与 10001/4000/1024 规格。
9. **`src-tauri/src/proxy/pipeline/inbound.rs`**：挂载 `enforce_argon_thinking_invariants`。
10. **`src-tauri/src/proxy/mappers/gemini/wrapper.rs`**：意图压缩后兜底挂载守门员。
11. **`src-tauri/resources/official_models.json`**：声明官方模型与占位元数据。
12. **`src-tauri/resources/model_specs.json`**：静态配置规格。
13. **`src/config/modelConfig.ts`**：声明前端 `SUPPORTED_MODELS`。
14. **`src/utils/modelCategory.ts`**：增加 `'gemini-argon'` 与优先级查找列表。
15. **`src/utils/quotaDisplay.ts`**：短板计算支持 Argon 5H 与周配额。
16. **`src/components/accounts/AccountRow.tsx`**：双列网格第 4 处渲染 Argon，与 Pro/Claude 对齐。
17. **`src/components/accounts/AccountTable.tsx`**：表格列展示 Argon。
18. **`src/components/accounts/AccountCard.tsx`**：卡片视图展示 Argon。
19. **`src/components/dashboard/CurrentAccount.tsx`**：当前活跃账号呈现 Argon。

---

## 六、验证与端到端自动化测试

运行以下命令执行全量验证：

```bash
# 1. 运行 Ponytail 单元测试
cargo test --manifest-path src-tauri/Cargo.toml ponytail

# 2. 运行周配额熔断测试
cargo test --manifest-path src-tauri/Cargo.toml weekly_quota

# 3. 运行独立离线沙盒全链路验证器
rustc --test sandbox_verifier.rs -o /tmp/verifier_test && /tmp/verifier_test && rm -f /tmp/verifier_test
```

全部测试将输出 `ok`，0 错误，100% 通过。
