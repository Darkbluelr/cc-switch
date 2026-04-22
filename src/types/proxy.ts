export interface ProxyConfig {
  listen_address: string;
  listen_port: number;
  max_retries: number;
  request_timeout: number;
  enable_logging: boolean;
  live_takeover_active?: boolean;
  // 超时配置
  streaming_first_byte_timeout: number;
  streaming_idle_timeout: number;
  non_streaming_timeout: number;
}

export interface ProxyStatus {
  running: boolean;
  address: string;
  port: number;
  active_connections: number;
  total_requests: number;
  success_requests: number;
  failed_requests: number;
  success_rate: number;
  uptime_seconds: number;
  current_provider: string | null;
  current_provider_id: string | null;
  last_request_at: string | null;
  last_error: string | null;
  failover_count: number;
  active_targets?: ActiveTarget[];
}

export interface ActiveTarget {
  app_type: string;
  provider_name: string;
  provider_id: string;
}

export interface ProxyServerInfo {
  address: string;
  port: number;
  started_at: string;
}

export interface ProxyTakeoverStatus {
  claude: boolean;
  codex: boolean;
  gemini: boolean;
  opencode: boolean;
  openclaw: boolean;
}

export interface ProviderHealth {
  provider_id: string;
  app_type: string;
  is_healthy: boolean;
  consecutive_failures: number;
  last_success_at: string | null;
  last_failure_at: string | null;
  last_error: string | null;
  updated_at: string;
}

// 熔断器相关类型
export interface CircuitBreakerConfig {
  failureThreshold: number;
  successThreshold: number;
  timeoutSeconds: number;
  errorRateThreshold: number;
  minRequests: number;
}

export type CircuitState = "closed" | "open" | "half_open";

export interface CircuitBreakerStats {
  state: CircuitState;
  consecutiveFailures: number;
  consecutiveSuccesses: number;
  totalRequests: number;
  failedRequests: number;
}

// 供应商健康状态枚举
export enum ProviderHealthStatus {
  Healthy = "healthy",
  Degraded = "degraded",
  Failed = "failed",
  Unknown = "unknown",
}

// 扩展 ProviderHealth 以包含前端计算的状态
export interface ProviderHealthWithStatus extends ProviderHealth {
  status: ProviderHealthStatus;
  circuitState?: CircuitState;
}

export interface ProxyUsageRecord {
  provider_id: string;
  app_type: string;
  endpoint: string;
  request_tokens: number | null;
  response_tokens: number | null;
  status_code: number;
  latency_ms: number;
  error: string | null;
  timestamp: string;
}

// 故障转移队列条目
export interface FailoverQueueItem {
  providerId: string;
  providerName: string;
  providerNotes?: string;
  sortIndex?: number;
}

/**
 * Per-provider 健康度指标（最近 N 秒的聚合）
 *
 * 缓存命中率分母按 app_type 语义计算：
 * - Codex 流式：input_tokens（已包含 cached）
 * - Codex 非流式：input_tokens + cache_read_tokens
 * - Claude：input_tokens + cache_read_tokens + cache_creation_tokens
 */
export interface ProviderHealthMetricsView {
  providerId: string;
  appType: string;
  /** 采样窗口（秒）*/
  windowSeconds: number;
  totalRequests: number;
  successCount: number;
  errorCount: number;
  /** 假 200：流式 200 + 三类 token 全 0 */
  fake200Count: number;
  streaming200Count: number;
  cacheReadTokens: number;
  promptTotalTokens: number;
  /** [0, 1] 或 null（无样本）*/
  successRate: number | null;
  /** [0, 1] 或 null（无流式 200 样本）*/
  fake200Rate: number | null;
  /** [0, 1] 或 null（无 prompt token）*/
  cacheHitRate: number | null;
  avgFirstTokenMs: number | null;
  maxFirstTokenMs: number | null;
}

// 全局代理配置（统一字段，三行镜像）
export interface GlobalProxyConfig {
  proxyEnabled: boolean;
  listenAddress: string;
  listenPort: number;
  enableLogging: boolean;
}

// 应用级代理配置（每个 app 独立）
export interface AppProxyConfig {
  appType: string;
  enabled: boolean;
  autoFailoverEnabled: boolean;
  maxRetries: number;
  streamingFirstByteTimeout: number;
  streamingIdleTimeout: number;
  nonStreamingTimeout: number;
  circuitFailureThreshold: number;
  circuitSuccessThreshold: number;
  circuitTimeoutSeconds: number;
  circuitErrorRateThreshold: number;
  circuitMinRequests: number;
}
