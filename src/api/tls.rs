//! TLS 信任源装配。
//!
//! 背景：ureq 打开 `native-certs` 后只信任操作系统的根证书库（见 ureq `rtls.rs`）。
//! 系统库读不全（受限令牌、精简镜像、部分精简版 Windows）或代理/校园网自签 CA
//! 没进库时，就会报：
//! `Connection Failed: tls connection init failed: invalid peer certificate: UnknownIssuer`。
//!
//! 这里把信任源做成「系统根 ∪ 内置 Mozilla 根 ∪ `NEKO_CA_FILE` 指定的 PEM」，
//! 并把加载数量记进 [`summary`]，启动时写进日志页——一眼能看出是「系统库没读到」
//! 还是「对方证书链本身有问题」。
//!
//! 内置 Mozilla 根来自 `webpki-roots`（纯数据，无 IO），比 Windows 库新且全，
//! 能兜住「系统库缺某些新根」的情况；反过来系统库能兜住企业代理 CA。

use std::sync::{Arc, OnceLock};

use rustls::RootCertStore;

/// 额外信任的 PEM 文件路径；多个路径用系统分隔符（Windows 下为 `;`）。
pub const CA_FILE_ENV: &str = "NEKO_CA_FILE";

static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
static SUMMARY: OnceLock<String> = OnceLock::new();
static ROOT_COUNT: OnceLock<usize> = OnceLock::new();

/// 全局复用的 rustls 配置：根证书只读一次，避免每个 `ApiClient` 重新加载系统库。
pub fn client_config() -> Arc<rustls::ClientConfig> {
    CONFIG.get_or_init(build).clone()
}

/// 已加载的根证书总数。
pub fn root_count() -> usize {
    let _ = client_config();
    ROOT_COUNT.get().copied().unwrap_or(0)
}

/// 信任源摘要，例如 `系统 137 张 + 内置 147 张 + 额外 0 张`。
pub fn summary() -> String {
    let _ = client_config();
    SUMMARY.get().cloned().unwrap_or_else(|| "未初始化".into())
}

fn build() -> Arc<rustls::ClientConfig> {
    let mut roots = RootCertStore::empty();
    let mut notes: Vec<String> = Vec::new();

    // ① 系统根证书（Windows 证书库 / macOS 钥匙串 / Linux ca-certificates）
    let mut native = 0usize;
    match rustls_native_certs::load_native_certs() {
        Ok(certs) => {
            let (ok, bad) = roots.add_parsable_certificates(certs);
            native = ok;
            if bad > 0 {
                notes.push(format!("系统库有 {bad} 张无法解析"));
            }
        }
        Err(e) => notes.push(format!("系统库读取失败: {e}")),
    }

    // ② 内置 Mozilla 根：补系统库缺失的新根
    let before = roots.len();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let bundled = roots.len() - before;

    // ③ 额外 PEM：企业代理 / 私有 CA（可选）
    let mut extra = 0usize;
    if let Some(paths) = std::env::var_os(CA_FILE_ENV) {
        for path in std::env::split_paths(&paths) {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let mut reader = std::io::BufReader::new(bytes.as_slice());
                    let mut added = 0usize;
                    for cert in rustls_pemfile::certs(&mut reader).flatten() {
                        if roots.add(cert).is_ok() {
                            added += 1;
                        }
                    }
                    extra += added;
                    if added == 0 {
                        notes.push(format!("{} 里没有可用的 PEM 证书", path.display()));
                    }
                }
                Err(e) => notes.push(format!("读取 {} 失败: {e}", path.display())),
            }
        }
    }

    let total = roots.len();
    ROOT_COUNT.set(total).ok();
    SUMMARY.set(
        if notes.is_empty() {
            format!("系统 {native} 张 + 内置 {bundled} 张 + 额外 {extra} 张")
        } else {
            format!(
                "系统 {native} 张 + 内置 {bundled} 张 + 额外 {extra} 张（{}）",
                notes.join("；")
            )
        },
    )
    .ok();

    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("rustls 默认协议版本可用")
    .with_root_certificates(roots)
    .with_no_client_auth();
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_system_and_bundled_roots() {
        assert!(root_count() > 0, "至少应加载到一张根证书");
        let text = summary();
        println!("TLS 信任源：{text}");
        assert!(text.contains("内置"), "{text}");
    }

    #[test]
    fn summary_is_stable_across_calls() {
        assert_eq!(summary(), summary());
        assert_eq!(root_count(), root_count());
    }

    /// 真实网络检查：本机信任链能否握上 Geetest 验证码域名。
    ///
    /// `cargo test live_tls_handshake -- --ignored --nocapture`
    /// 只验证 TLS/HTTP 是否握上（4xx 也算成功，账号密码不参与）。
    #[test]
    #[ignore]
    fn live_tls_handshake() {
        let agent = crate::api::client::make_agent();
        let url = "https://gcaptcha4.geetest.com/load\
                   ?captcha_id=8c065103d81f5fd3efec8ad3e3a84c30&challenge=probe&client_type=web&lang=zh";
        match agent.get(url).call() {
            Ok(resp) => println!("TLS OK, HTTP {}", resp.status()),
            Err(ureq::Error::Status(code, _)) => println!("TLS OK（HTTP {code}，握手已通过）"),
            Err(ureq::Error::Transport(e)) => panic!("TLS/传输失败: {e}"),
        }
    }
}
