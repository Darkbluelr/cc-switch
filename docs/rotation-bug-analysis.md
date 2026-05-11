# 代理 Key 轮换问题分析与修复建议

> 日期: 2026-04-22
> 场景: 开启三个 Claude provider，通过系统代理走 `http://127.0.0.1:15721/v1`，并发较高时遭遇 429，且日志显示 429 后仍持续命中**同一个** key，而非切到其他两个。
> 目标: 定位根因 → 对比参考项目（`/Users/ozbombor/Projects/AI全自动编程系统`）→ 给出可落地的修复方案。

---

## 1. TL;DR（执行摘要）

- **cc-switch 已实现"请求内逐个 provider 尝试 + 熔断器"的故障转移**，代码主入口：
  - `src-tauri/src/proxy/provider_router.rs:37` `select_providers()`
  - `src-tauri/src/proxy/forwarder.rs:110` `forward_with_retry()`
  - `src-tauri/src/proxy/circuit_breaker.rs`
  - 错误分类：`src-tauri/src/proxy/forwarder.rs:1501` `categorize_proxy_error()`——429 被归为 `Retryable`，理论上会切到下一个 provider。

- **但默认配置下不会轮换**，这是本次"一直命中同一个 key"最可能的根因：
  - `auto_failover_enabled` 默认 `0`（见 `database/schema.rs:126`、`dao/proxy.rs:234`）。
  - 关闭时 `select_providers()` 只返回"当前 provider"一条记录（`provider_router.rs:79-96`），forwarder 的 for 循环只有一个候选，根本没有"下一个"可以切。
  - 只有当 UI 里为该应用（claude/codex/gemini）显式打开"自动故障转移"且**把三个 provider 都加入了 failover 队列**时，`select_providers()` 才会返回一个多元素列表，`forward_with_retry()` 才会在 429 时 `continue` 到下一个（`forwarder.rs:670-690`）。

- **对比参考项目（Python, `desktop-shell/sidecar/observability/local_proxy_runtime.py`）**，它在"请求内故障转移"的基础上还做了 cc-switch 当前缺失的东西：
  - **429 专属冷却**：`CircuitBreakerRegistry.record_rate_limited()`（局部代理运行时 L1429-1481），结合 `Retry-After` 头 / 带抖动的指数退避，独立于通用熔断。
  - **短路阈值更低**（默认 3 次连续失败），cc-switch claude 默认 8 次（`schema.rs:148`），意味着即使触发了 429 短路换路，也要很久才会把"坏 key"从池子里剔除。
  - **SSE 流内错误检测**：上游返回 200 但在流体里塞 `{"error":{"type":"rate_limit_error"}}` 时，参考项目会识别并退路；cc-switch 目前只看 HTTP 状态码（`forwarder.rs:1426`）。

- **推荐修复优先级**：
  1. **立刻**：在界面上打开"自动故障转移"并把全部 3 个 key 加入 failover 队列（用户配置层修复，无需改码）。
  2. **短期补丁（代码）**：为 429 增加 `Retry-After` 感知与每 key 短冷却；把 Claude 的 `circuit_failure_threshold` 默认值从 8 下调到 3~4；把流式响应里的 `error` 事件映射到 `UpstreamError{429}`。
  3. **长期**：补齐参考项目的 429 专属状态机（open/half-open + jitter 指数退避），与通用熔断解耦。

---

## 2. 现状与代码路径回溯

### 2.1 一次请求的处理链

```
Client → :15721/v1
  → handler_context.rs:81-168  RequestContext::new
      - 读取 proxy_config[app_type].auto_failover_enabled
      - 调 provider_router.select_providers(app_type)  →  Vec<Provider>
  → forwarder.rs:110           forward_with_retry(providers)
      for provider in providers:
         1) allow_provider_request()  熔断器许可
         2) forward(provider)          单次转发（不重试同一 key）
         3) 成功 → record_result(ok) → 返回
         4) 失败 → categorize → Retryable ? continue : return Err
```

关键点：**for 循环的长度完全取决于 `select_providers()` 返回的列表**。

### 2.2 `select_providers()` 两条分支

`src-tauri/src/proxy/provider_router.rs:37-109`：

```rust
if auto_failover_enabled {
    // 按 failover_queue（带 sort_index）顺序给出全部候选
    let ordered_ids = self.db.get_failover_queue(app_type)?
        .into_iter().map(|item| item.provider_id).collect();
    for id in ordered_ids {
        if breaker.is_available().await { result.push(...); }
    }
} else {
    // 只返回当前 provider  ← 单元素列表
    if let Some(current) = db.get_provider_by_id(&current_id, app_type)? {
        result.push(current);
    }
}
```

默认值：

```
schema.rs:126  proxy_config.auto_failover_enabled INTEGER NOT NULL DEFAULT 0
dao/proxy.rs:234  fallback → auto_failover_enabled: false
```

**因此**：如果用户没在 UI 上主动开启"自动故障转移"（每个应用一份独立开关 `commands/failover.rs:79 set_auto_failover_enabled`），`select_providers()` 永远只返回一个 provider，`forward_with_retry()` 的 for 循环只跑一次，429 直接透传回客户端——完美重现用户观察到的现象。

### 2.3 429 → `UpstreamError` 的映射

`forwarder.rs:1423-1436`：

```rust
let status = response.status();
if status.is_success() {
    Ok((response, resolved_claude_api_format))
} else {
    let status_code = status.as_u16();
    let body_text = String::from_utf8(response.bytes().await?.to_vec()).ok();
    Err(ProxyError::UpstreamError { status: status_code, body: body_text })
}
```

`categorize_proxy_error()`（`forwarder.rs:1501-1521`）：

```rust
ProxyError::UpstreamError { .. } => ErrorCategory::Retryable,
```

也就是 **429 在 HTTP 层被判定为 `Retryable`**，条件满足时会 `continue` 到下一个 provider（`forwarder.rs:687-690`）。这一段设计本身没问题，**问题在于触发条件（failover 启用 + 多元素列表）默认不满足**。

### 2.4 熔断器默认阈值（Claude 应用特定值）

`schema.rs:148`：

```sql
VALUES ('claude', 6, 90, 180, 600, 8, 3, 90, 0.7, 15)
--                        ^  ^       ^    ^
--                        |  +-- success_threshold=3
--                        +-- failure_threshold=8
--                                    timeout_seconds=90  error_rate=0.7  min_requests=15
```

Claude 默认需要**连续 8 次失败**或**15 次以上且错误率 ≥ 70%** 才短路该 provider 60–90s。在高并发 429 下这个门槛很宽松：同一个 key 可能要连续吞 8 个 429 才会被标记"坏"。即便 failover 开启，用户仍会看到大量 429 打到第一个 provider 上，"看起来一直是同一个 key"。

---

## 3. 根因假设（按可能性排序）

| # | 假设 | 证据 | 验证方法 |
|---|------|------|----------|
| 1 | **`auto_failover_enabled = false`**：用户没打开该应用的自动故障转移 | 默认 0；关闭时只返回 1 个 provider | 打开 SQLite 查 `SELECT app_type, auto_failover_enabled FROM proxy_config`；或 UI 里看对应应用的"自动故障转移"开关 |
| 2 | **三个 key 不全在 failover 队列**：只有被勾入队列的 provider 才会参与轮换 | `provider_router.rs:56-61` 只按 `get_failover_queue()` 拉候选 | `SELECT id, name, in_failover_queue FROM providers WHERE app_type='claude'` |
| 3 | **三个"key"其实是同一个 provider 的三份 API key**：cc-switch 每个 Provider 只持有单一 settings_config，不支持"同一 provider 内 key 池" | `provider.rs:10` `Provider` 里只有一个 `settings_config: Value`；`settings_config.env.ANTHROPIC_AUTH_TOKEN` 是单值 | 打开 UI 看三个 key 是否建成了 3 个 Provider 条目，还是同一个 Provider 的多行 |
| 4 | **Claude 熔断阈值过高**：即便开启了 failover，单次请求确实会切下一个，但"下一个请求又从第一个开始"（`select_providers` 每次按 sort_index 排序，不做轮询），导致第一个 key 被主要流量反复命中 | `provider_router.rs:56-77` 无 round-robin 状态；熔断阈值 8 次 | 看日志里 `[FWD-001]` 的出现频率——如果频繁出现说明确实在切，只是第一条始终是主角 |
| 5 | **流式 SSE 内的 429**：上游回 HTTP 200 + SSE 中塞 `rate_limit_error`，cc-switch 不识别 | `forwarder.rs:1426` 只看 `status.is_success()`；`response_processor.rs` 下游处理时才发现错误，但彼时已不在 forwarder 的重试循环里 | 看抓包：429 是 HTTP 层还是 SSE data 层 |

> 注：假设 1 是**首要根因**，几乎必然命中；其余是**叠加放大**因素。

---

## 4. 与参考项目对比

参考实现（Python，`/Users/ozbombor/Projects/AI全自动编程系统/desktop-shell/sidecar/observability/local_proxy_runtime.py`）。两者都采用"请求内顺序故障转移 + 熔断器"，差异集中在 **429 专属处理** 与 **退避策略**：

| 能力 | cc-switch（Rust） | 参考项目（Python） |
|------|-------------------|--------------------|
| 请求内逐个 provider 尝试 | ✅ `forwarder.rs:142` | ✅ `local_proxy_runtime.py:1373` |
| 熔断器状态机（closed/open/half-open） | ✅ `circuit_breaker.rs` | ✅ `CircuitState` dataclass L661-804 |
| 429 → 触发换路 | ✅ 归为 Retryable | ✅ L1429-1481 |
| **`Retry-After` 头感知** | ❌ 丢弃 | ✅ `_retry_after_ms_from_headers` |
| **429 专属冷却（与通用熔断解耦）** | ❌ | ✅ `record_rate_limited(retry_after_ms=...)` |
| **退避指数 + 抖动** | ❌ 固定 `circuit_timeout_seconds` | ✅ `ideal_delay * (0.5 + 0.5*rand())` L789-798 |
| SSE 流内 429 检测 | ❌ 只看 HTTP 状态码 | ✅ （下游有）尽管本项目也是主要关注 HTTP 层，但会把 `error_excerpt` 记到熔断器 |
| Claude 默认短路阈值 | 8 次连续失败 | 3 次（`_failure_threshold = 3`） |
| 故障转移默认值 | **关** | 配置里 `failover_order` 存在即启用 |
| 轮询（Round-Robin） | ❌ 永远按 sort_index | ❌（同样是顺序）两边都没有 RR |

**可借鉴的关键实现** (参考文件 `local_proxy_runtime.py`)：

1. `_retry_after_ms_from_headers()`: 解析 `Retry-After`（秒数或 HTTP date），得到 `retry_after_ms`。
2. `record_rate_limited(provider_id, retry_after_ms=...)`: 把该 provider 标记为"至 `now + retry_after_ms` 前不可选"，独立于通用 failure counter。
3. `should_allow(provider_id, now_ms)`: select 候选前剔除仍在冷却的 key。
4. 抖动退避（L789-798）：`base=1000ms, max=60000ms, exponent=max(0, failures-1)`，再乘 `0.5~1.0` 的 jitter 防止多实例同步重试。

---

## 5. 修复建议

### 5.1 用户侧（立即可行，不改代码）

1. 打开对应应用的"自动故障转移"开关：
   - UI 路径：设置 → 该应用的代理设置 → 启用自动故障转移
   - 或 SQL：`UPDATE proxy_config SET auto_failover_enabled = 1 WHERE app_type = 'claude';`
2. 确认三个 provider 都在 failover 队列里，且 `sort_index` 合理：
   - `SELECT id, name, in_failover_queue, sort_index FROM providers WHERE app_type='claude' ORDER BY COALESCE(sort_index, 999999);`
   - 如有缺失：UI 里把三个 key 都"加入故障转移队列"。
3. 观察日志：出现 `[FWD-001]` / `[FWD-002]` 说明换路确实在发生。

### 5.2 短期代码补丁（建议 1~2 个 PR 内完成）

#### 补丁 A：降低 Claude 默认熔断门槛

**文件**：`src-tauri/src/database/schema.rs:148`

把 claude 的默认三元组从 `(failure=8, error_rate=0.7, min_requests=15)` 调整为 `(4, 0.5, 8)` 或接近参考项目的 `3` 连续失败：

```sql
-- 旧
VALUES ('claude', 6, 90, 180, 600, 8, 3, 90, 0.7, 15)
-- 新（示例）
VALUES ('claude', 6, 90, 180, 600, 4, 2, 60, 0.5, 8)
```

> 注意：这是 `INSERT OR IGNORE`，只影响首次安装；存量用户需提供迁移或 `proxy_config` 的"恢复默认值"按钮。

#### 补丁 B：429 专属冷却 + `Retry-After` 感知

**改动点 1**：`forwarder.rs:1501 categorize_proxy_error()` 细分 429

```rust
pub enum ErrorCategory {
    Retryable,
    RetryableRateLimited { retry_after_ms: Option<u64> },  // 新增
    NonRetryable,
    ClientAbort,
}

fn categorize_proxy_error(&self, error: &ProxyError) -> ErrorCategory {
    match error {
        ProxyError::UpstreamError { status, body } if *status == 429 => {
            ErrorCategory::RetryableRateLimited {
                retry_after_ms: parse_retry_after_from_body_or_headers(body),
            }
        }
        ProxyError::UpstreamError { .. } => ErrorCategory::Retryable,
        // ... 其他保持原样
    }
}
```

**改动点 2**：`forward()` 需要把 response headers 暴露出来（当前只在 `is_success` 分支返回 `ProxyResponse`，失败分支只留下 body）。建议把 `UpstreamError` 扩展为：

```rust
UpstreamError { status: u16, body: Option<String>, retry_after_ms: Option<u64> }
```

在 `forwarder.rs:1423-1436` 读取 `response.headers().get("retry-after")` 填充。

**改动点 3**：`provider_router.rs` 增加"冷却直到时间戳"字段

```rust
struct ProviderCooldown {
    until_ms: u64,
    reason: String,
}
// HashMap<(app_type, provider_id), ProviderCooldown>
```

- `select_providers()` 过滤掉 `now < until_ms` 的 provider。
- 新增 `record_rate_limited(&self, provider_id, app_type, retry_after_ms)`：写入该表，**不**增加 `consecutive_failures`（避免 429 把 provider 永久短路）。
- `forwarder.rs:670` 的 Retryable 分支里，如果是 `RetryableRateLimited`，调 `record_rate_limited` 而不是 `record_result(success=false)`。

#### 补丁 C：带抖动的退避

借鉴 `local_proxy_runtime.py:789`：

```rust
fn compute_rate_limit_cooldown(consecutive_429: u32, server_hint: Option<u64>) -> u64 {
    if let Some(ms) = server_hint { return ms; }
    let base_ms = 1000u64;
    let max_ms = 60_000u64;
    let exp = consecutive_429.saturating_sub(1).min(10);
    let ideal = base_ms.saturating_mul(1u64 << exp).min(max_ms);
    let jitter = 0.5 + 0.5 * rand::random::<f64>();
    ((ideal as f64) * jitter).max(base_ms as f64).min(max_ms as f64) as u64
}
```

### 5.3 长期：把"轮换"与"故障转移"语义分开

当前 `failover_queue` 是"按 sort_index 顺序逐个尝试"，语义更像 fallback chain，而不是 load-balance。建议新增一种可选策略：

- **Round-Robin**：为 `proxy_config` 增列 `selection_strategy TEXT CHECK IN ('priority','round_robin','least_cooldown')`。
- **Least-Cooldown**：优先挑冷却时间为 0 或最先解除的 provider。
- `provider_router.rs` 里以 `AtomicUsize` 维护 RR 游标。
- 这样即使没有出错，流量也能天然摊到三个 key，把"一个 key 扛全部并发"从架构上消除。

---

## 6. 验证步骤（修复后）

1. **单元测试**（已有 `provider_router.rs:318` 起的一批 serial 测试可复用），新增：
   - 模拟 provider A 回 429 且带 `Retry-After: 5`，断言 A 进入 5s 冷却、B/C 被选中；5s 后 A 重新可选。
   - 模拟 A 持续 429，断言不会把 A 误熔断成"永久离线"（429 不计入 `consecutive_failures`）。

2. **集成回放**：在本地并发打 20 次 `/v1/messages`，第一个 key 故意设低 quota，观察日志里出现 `[FWD-001]` 切换记录，以及成功响应分散到 2~3 个 provider。

3. **观察性**：建议在 tray 状态面板里把"每个 provider 当前冷却剩余时间"暴露出来——这个参考项目 UI 里也有类似展示。

---

## 7. 附录 A：关键文件索引

### cc-switch 侧

| 能力 | 文件:行 |
|------|---------|
| Provider 选择 | `src-tauri/src/proxy/provider_router.rs:37` |
| 请求重试循环 | `src-tauri/src/proxy/forwarder.rs:110` |
| 错误分类 | `src-tauri/src/proxy/forwarder.rs:1501` |
| 429 → UpstreamError | `src-tauri/src/proxy/forwarder.rs:1423-1436` |
| 熔断器状态机 | `src-tauri/src/proxy/circuit_breaker.rs` |
| Auto-Failover 默认值 | `src-tauri/src/database/schema.rs:126`, `src-tauri/src/database/dao/proxy.rs:234` |
| Claude 默认熔断阈值 | `src-tauri/src/database/schema.rs:148` |
| 故障转移队列 DAO | `src-tauri/src/database/dao/failover.rs:22` |
| Failover UI 切换 | `src-tauri/src/proxy/failover_switch.rs:41` |
| Tauri 命令：开关 | `src-tauri/src/commands/failover.rs:79` |

### 参考项目侧

| 能力 | 文件:行（相对 `/Users/ozbombor/Projects/AI全自动编程系统`）|
|------|---------|
| 顺序故障转移 | `desktop-shell/sidecar/observability/local_proxy_runtime.py:1373-1482` |
| 429 专属处理 | `local_proxy_runtime.py:1429-1481` |
| 熔断器状态机 | `local_proxy_runtime.py:661-804` |
| Retry-After 抖动退避 | `local_proxy_runtime.py:789-798` |
| Provider 选择 | `local_proxy_runtime.py:1159-1177` |
| 使用统计 & Failover 顺序 | `desktop-shell/sidecar/observability/llm_usage_store.py` |

---

## 8. 附录 B：给用户的一键自检清单

```sql
-- 1. 检查 auto_failover_enabled 是否打开
SELECT app_type, enabled, auto_failover_enabled FROM proxy_config;

-- 2. 检查 failover_queue 是否包含所有 key
SELECT id, name, app_type, in_failover_queue, sort_index
FROM providers
WHERE app_type = 'claude'
ORDER BY COALESCE(sort_index, 999999);

-- 3. 查看 provider 健康记录（是否已经被连续失败标记）
SELECT provider_id, consecutive_failures, is_healthy, last_error, last_failure_at
FROM provider_health
WHERE app_type = 'claude';
```

预期结果：
- `auto_failover_enabled = 1`
- 三个 key 的 `in_failover_queue = 1`
- `consecutive_failures` 都 < `circuit_failure_threshold`（当前默认 8）
