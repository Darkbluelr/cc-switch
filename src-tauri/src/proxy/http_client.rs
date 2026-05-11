//! 全局 HTTP 客户端模块
//!
//! 提供支持全局代理配置的 HTTP 客户端。
//! 所有需要发送 HTTP 请求的模块都应使用此模块提供的客户端。

use http::header::HeaderValue;
use once_cell::sync::OnceCell;
use reqwest::Client;
use std::env;
use std::net::IpAddr;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// 全局 HTTP 客户端实例
static GLOBAL_CLIENT: OnceCell<RwLock<Client>> = OnceCell::new();

/// 当前代理 URL（用于日志和状态查询）
static CURRENT_PROXY_URL: OnceCell<RwLock<Option<String>>> = OnceCell::new();

/// 当前系统代理指纹（仅在未配置显式代理时使用，用于检测运行时系统代理变化）
static SYSTEM_PROXY_FINGERPRINT: OnceCell<RwLock<String>> = OnceCell::new();

/// 上次检查系统代理变化时间（节流用）
static LAST_SYSTEM_PROXY_CHECK_AT: OnceCell<RwLock<Instant>> = OnceCell::new();

/// CC Switch 代理服务器当前监听的端口
static CC_SWITCH_PROXY_PORT: OnceCell<RwLock<u16>> = OnceCell::new();

const SYSTEM_PROXY_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// 设置 CC Switch 代理服务器的监听端口
///
/// 应在代理服务器启动时调用，以便系统代理检测能正确识别自己的端口
pub fn set_proxy_port(port: u16) {
    if let Some(lock) = CC_SWITCH_PROXY_PORT.get() {
        if let Ok(mut current_port) = lock.write() {
            *current_port = port;
            log::debug!("[GlobalProxy] Updated CC Switch proxy port to {port}");
        }
    } else {
        let _ = CC_SWITCH_PROXY_PORT.set(RwLock::new(port));
        log::debug!("[GlobalProxy] Initialized CC Switch proxy port to {port}");
    }
}

/// 获取 CC Switch 代理服务器的监听端口
fn get_proxy_port() -> u16 {
    CC_SWITCH_PROXY_PORT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|port| *port)
        .unwrap_or(15721) // 默认端口作为回退
}

/// 初始化全局 HTTP 客户端
///
/// 应在应用启动时调用一次。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，如 `http://127.0.0.1:7890` 或 `socks5://127.0.0.1:1080`
///   传入 None 或空字符串表示直连
pub fn init(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let client = build_client(effective_url)?;

    // 尝试初始化全局客户端，如果已存在则记录警告并使用 apply_proxy 更新
    if GLOBAL_CLIENT.set(RwLock::new(client.clone())).is_err() {
        log::warn!(
            "[GlobalProxy] [GP-003] Already initialized, updating instead: {}",
            effective_url
                .map(mask_url)
                .unwrap_or_else(|| "direct connection".to_string())
        );
        // 已初始化，改用 apply_proxy 更新
        return apply_proxy(proxy_url);
    }

    // 初始化代理 URL 记录
    let _ = CURRENT_PROXY_URL.set(RwLock::new(effective_url.map(|s| s.to_string())));
    init_system_proxy_fingerprint(effective_url.is_none());

    log::info!(
        "[GlobalProxy] Initialized: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 验证代理配置（不应用）
///
/// 只验证代理 URL 是否有效，不实际更新全局客户端。
/// 用于在持久化之前验证配置的有效性。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
///
/// # Returns
/// 验证成功返回 Ok(())，失败返回错误信息
pub fn validate_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    // 只调用 build_client 来验证，但不应用
    build_client(effective_url)?;
    Ok(())
}

/// 应用代理配置（假设已验证）
///
/// 直接应用代理配置到全局客户端，不做额外验证。
/// 应在 validate_proxy 成功后调用。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
pub fn apply_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }
    init_system_proxy_fingerprint(effective_url.is_none());

    log::info!(
        "[GlobalProxy] Applied: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 更新代理配置（热更新）
///
/// 可在运行时调用以更改代理设置，无需重启应用。
/// 注意：此函数同时验证和应用，如果需要先验证后持久化再应用，
/// 请使用 validate_proxy + apply_proxy 组合。
///
/// # Arguments
/// * `proxy_url` - 新的代理 URL，None 或空字符串表示直连
#[allow(dead_code)]
pub fn update_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }

    log::info!(
        "[GlobalProxy] Updated: {}",
        effective_url
            .map(mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 获取全局 HTTP 客户端
///
/// 返回配置了代理的客户端（如果已配置代理），否则返回跟随系统代理的客户端。
pub fn get() -> Client {
    maybe_refresh_for_system_proxy_change();
    GLOBAL_CLIENT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|c| c.clone())
        .unwrap_or_else(|| {
            log::warn!("[GlobalProxy] [GP-004] Client not initialized, using fallback");
            build_client(None).unwrap_or_default()
        })
}

/// 获取当前代理 URL
///
/// 返回当前配置的代理 URL，None 表示直连。
pub fn get_current_proxy_url() -> Option<String> {
    CURRENT_PROXY_URL
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|url| url.clone())
}

/// 当前请求应使用的“有效上游代理”配置。
///
/// - 若用户在 CC Switch 内显式配置了代理，则优先使用显式代理。
/// - 否则跟随系统代理（环境变量 + OS 系统代理），并在检测到系统代理指向
///   CC Switch 自身端口时自动忽略，避免递归代理导致请求卡死/失败。
#[derive(Clone, Debug)]
pub struct EffectiveUpstreamProxy {
    pub url: String,
    pub basic_auth: Option<HeaderValue>,
}

/// 获取当前请求的“有效上游代理”。
///
/// 该函数用于转发链路（hyper raw write）选择上游代理：
/// - 显式代理（UI 配置）优先
/// - 否则跟随系统代理（例如 Clash/VPN 全局代理模式）
pub fn get_effective_upstream_proxy(dst: &http::Uri) -> Option<EffectiveUpstreamProxy> {
    if let Some(explicit) = get_current_proxy_url() {
        return Some(EffectiveUpstreamProxy {
            url: explicit,
            basic_auth: None,
        });
    }

    // Keep runtime system-proxy hot-reload working even when the forwarding
    // path doesn't use reqwest. This also refreshes TLS roots on proxy changes.
    maybe_refresh_for_system_proxy_change();

    let matcher = hyper_util::client::proxy::matcher::Matcher::from_system();
    let intercept = matcher.intercept(dst)?;

    let proxy_url = intercept.uri().to_string();
    if proxy_points_to_loopback(&proxy_url) {
        log::warn!(
            "[GlobalProxy] System proxy points to CC Switch itself ({}), ignoring to avoid recursion",
            mask_url(&proxy_url)
        );
        return None;
    }

    Some(EffectiveUpstreamProxy {
        url: proxy_url,
        basic_auth: intercept.basic_auth().cloned(),
    })
}

/// 检查是否正在使用代理
#[allow(dead_code)]
pub fn is_proxy_enabled() -> bool {
    get_current_proxy_url().is_some()
}

/// 构建 HTTP 客户端
fn build_client(proxy_url: Option<&str>) -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Duration::from_secs(60))
        // 禁用 reqwest 自动解压：防止 reqwest 覆盖客户端原始 accept-encoding header。
        // 响应解压由 response_processor 根据 content-encoding 手动处理。
        .no_gzip()
        .no_brotli()
        .no_deflate();

    // 有代理地址则使用代理，否则跟随系统代理
    if let Some(url) = proxy_url {
        // 先验证 URL 格式和 scheme
        let parsed = url::Url::parse(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;

        let scheme = parsed.scheme();
        if !["http", "https", "socks5", "socks5h"].contains(&scheme) {
            return Err(format!(
                "Invalid proxy scheme '{}' in URL '{}'. Supported: http, https, socks5, socks5h",
                scheme,
                mask_url(url)
            ));
        }

        let proxy = reqwest::Proxy::all(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;
        builder = builder.proxy(proxy);
        log::debug!("[GlobalProxy] Proxy configured: {}", mask_url(url));
    } else {
        // 未设置全局代理时，让 reqwest 自动检测系统代理（环境变量）
        // 若系统代理指向本机，禁用系统代理避免自环
        if system_proxy_points_to_loopback() {
            builder = builder.no_proxy();
            log::warn!(
                "[GlobalProxy] System proxy points to localhost, bypassing to avoid recursion"
            );
        } else {
            log::debug!("[GlobalProxy] Following system proxy (no explicit proxy configured)");
        }
    }

    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

fn init_system_proxy_fingerprint(following_system_proxy: bool) {
    let fingerprint = if following_system_proxy {
        compute_system_proxy_fingerprint()
    } else {
        String::new()
    };

    if let Some(lock) = SYSTEM_PROXY_FINGERPRINT.get() {
        if let Ok(mut fp) = lock.write() {
            *fp = fingerprint;
        }
    } else {
        let _ = SYSTEM_PROXY_FINGERPRINT.set(RwLock::new(fingerprint));
    }

    if LAST_SYSTEM_PROXY_CHECK_AT.get().is_none() {
        let _ = LAST_SYSTEM_PROXY_CHECK_AT.set(RwLock::new(Instant::now()));
    }
}

fn compute_system_proxy_fingerprint() -> String {
    // NOTE: This reads both env proxy variables and OS-level system proxy settings
    // (on supported platforms) via hyper-util.
    let matcher = hyper_util::client::proxy::matcher::Matcher::from_system();

    let http_dst = http::Uri::from_static("http://example.com/");
    let https_dst = http::Uri::from_static("https://example.com/");

    let http_proxy = matcher
        .intercept(&http_dst)
        .map(|i| mask_url(i.uri().to_string().as_str()))
        .unwrap_or_default();
    let https_proxy = matcher
        .intercept(&https_dst)
        .map(|i| mask_url(i.uri().to_string().as_str()))
        .unwrap_or_default();

    format!("http={http_proxy};https={https_proxy}")
}

fn maybe_refresh_for_system_proxy_change() {
    // Only meaningful when we're in "follow system proxy" mode (no explicit proxy URL configured).
    if get_current_proxy_url().is_some() {
        return;
    }

    let now = Instant::now();
    let should_check = match LAST_SYSTEM_PROXY_CHECK_AT.get() {
        Some(lock) => match lock.read() {
            Ok(last) => now.duration_since(*last) >= SYSTEM_PROXY_CHECK_INTERVAL,
            Err(_) => false,
        },
        None => true,
    };
    if !should_check {
        return;
    }

    // Update last check timestamp early to avoid stampedes under high concurrency.
    if let Some(lock) = LAST_SYSTEM_PROXY_CHECK_AT.get() {
        if let Ok(mut last) = lock.write() {
            *last = now;
        }
    } else {
        let _ = LAST_SYSTEM_PROXY_CHECK_AT.set(RwLock::new(now));
    }

    let new_fp = compute_system_proxy_fingerprint();
    let old_fp = SYSTEM_PROXY_FINGERPRINT
        .get()
        .and_then(|lock| lock.read().ok().map(|fp| fp.clone()))
        .unwrap_or_default();

    if new_fp == old_fp {
        return;
    }

    log::info!(
        "[GlobalProxy] System proxy changed, rebuilding HTTP client (old='{}', new='{}')",
        old_fp,
        new_fp
    );

    match build_client(None) {
        Ok(new_client) => {
            if let Some(lock) = GLOBAL_CLIENT.get() {
                if let Ok(mut client) = lock.write() {
                    *client = new_client;
                }
            }
            init_system_proxy_fingerprint(true);
            // Also refresh TLS connector so newly-installed proxy CAs are picked up without restart.
            let _ = super::hyper_client::refresh_tls_connector("system proxy changed");
        }
        Err(e) => {
            log::warn!("[GlobalProxy] Failed to rebuild client after system proxy change: {e}");
        }
    }
}

fn system_proxy_points_to_loopback() -> bool {
    const KEYS: [&str; 6] = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];

    KEYS.iter()
        .filter_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .any(|value| proxy_points_to_loopback(&value))
}

fn proxy_points_to_loopback(value: &str) -> bool {
    fn host_is_loopback(host: &str) -> bool {
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        host.parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
    }

    // 检查是否指向 CC Switch 自己的代理端口
    // 只有指向自己的代理才需要跳过，避免递归
    fn is_cc_switch_proxy_port(port: Option<u16>) -> bool {
        let cc_switch_port = get_proxy_port();
        port == Some(cc_switch_port)
    }

    if let Ok(parsed) = url::Url::parse(value) {
        if let Some(host) = parsed.host_str() {
            // 只有当主机是 loopback 且端口是 CC Switch 的端口时才返回 true
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
        return false;
    }

    let with_scheme = format!("http://{value}");
    if let Ok(parsed) = url::Url::parse(&with_scheme) {
        if let Some(host) = parsed.host_str() {
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
    }

    false
}

/// 隐藏 URL 中的敏感信息（用于日志）
pub fn mask_url(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        // 隐藏用户名和密码，保留 scheme、host 和端口
        let host = parsed.host_str().unwrap_or("?");
        match parsed.port() {
            Some(port) => format!("{}://{}:{}", parsed.scheme(), host, port),
            None => format!("{}://{}", parsed.scheme(), host),
        }
    } else {
        // URL 解析失败，返回部分内容
        if url.len() > 20 {
            format!("{}...", &url[..20])
        } else {
            url.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_mask_url() {
        assert_eq!(mask_url("http://127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(
            mask_url("http://user:pass@127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            mask_url("socks5://admin:secret@proxy.example.com:1080"),
            "socks5://proxy.example.com:1080"
        );
        // 无端口的 URL 不应显示 ":?"
        assert_eq!(
            mask_url("http://proxy.example.com"),
            "http://proxy.example.com"
        );
        assert_eq!(
            mask_url("https://user:pass@proxy.example.com"),
            "https://proxy.example.com"
        );
    }

    #[test]
    fn test_build_client_direct() {
        let result = build_client(None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_http_proxy() {
        let result = build_client(Some("http://127.0.0.1:7890"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_socks5_proxy() {
        let result = build_client(Some("socks5://127.0.0.1:1080"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_invalid_url() {
        // reqwest::Proxy::all 对某些无效 URL 不会立即报错
        // 使用明确无效的 scheme 来触发错误
        let result = build_client(Some("invalid-scheme://127.0.0.1:7890"));
        assert!(result.is_err(), "Should reject invalid proxy scheme");
    }

    #[test]
    fn test_proxy_points_to_loopback() {
        // 设置 CC Switch 代理端口为 15721（默认值）
        set_proxy_port(15721);

        // 只有指向 CC Switch 自己端口的 loopback 地址才返回 true
        assert!(proxy_points_to_loopback("http://127.0.0.1:15721"));
        assert!(proxy_points_to_loopback("socks5://localhost:15721"));
        assert!(proxy_points_to_loopback("127.0.0.1:15721"));

        // 其他 loopback 端口不应该被跳过（允许使用其他本地代理工具）
        assert!(!proxy_points_to_loopback("http://127.0.0.1:7890"));
        assert!(!proxy_points_to_loopback("socks5://localhost:1080"));

        // 非 loopback 地址不应该被跳过
        assert!(!proxy_points_to_loopback("http://192.168.1.10:7890"));
        assert!(!proxy_points_to_loopback("http://192.168.1.10:15721"));
    }

    #[test]
    fn test_system_proxy_points_to_loopback() {
        let _guard = env_lock().lock().unwrap();

        // 设置 CC Switch 代理端口
        set_proxy_port(15721);

        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];

        for key in &keys {
            std::env::remove_var(key);
        }

        // 指向 CC Switch 端口的代理应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:15721");
        assert!(system_proxy_points_to_loopback());

        // 指向其他端口的本地代理不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:7890");
        assert!(!system_proxy_points_to_loopback());

        // 非 loopback 地址不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://10.0.0.2:7890");
        assert!(!system_proxy_points_to_loopback());

        for key in &keys {
            std::env::remove_var(key);
        }
    }
}
