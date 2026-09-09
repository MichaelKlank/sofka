use std::net::IpAddr;

use http::Uri;
use kube::{Config, config::Kubeconfig};

pub(super) fn configure(config: &mut Config, kubeconfig: &Kubeconfig, context: Option<&str>) {
    let context = context.or(kubeconfig.current_context.as_deref());
    let cluster = kubeconfig
        .contexts
        .iter()
        .find(|entry| Some(entry.name.as_str()) == context)
        .and_then(|entry| entry.context.as_ref())
        .and_then(|context| {
            kubeconfig
                .clusters
                .iter()
                .find(|entry| entry.name == context.cluster)
        })
        .and_then(|entry| entry.cluster.as_ref());
    // An explicit kubeconfig proxy takes priority over environment exclusions.
    let Some(cluster) = cluster else { return };
    if cluster
        .proxy_url
        .as_ref()
        .is_some_and(|url| !url.is_empty())
    {
        return;
    }
    let exclusions = first_nonempty(
        std::env::var("NO_PROXY").ok(),
        std::env::var("no_proxy").ok(),
    );
    if exclusions.is_some_and(|value| matches(&config.cluster_url, &value)) {
        config.proxy_url = None;
    }
}

fn first_nonempty(upper: Option<String>, lower: Option<String>) -> Option<String> {
    upper
        .filter(|value| !value.is_empty())
        .or(lower.filter(|value| !value.is_empty()))
}

fn matches(server: &Uri, exclusions: &str) -> bool {
    let Some(host) = server.host() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let address = host.parse::<IpAddr>().ok();
    let port = server.port_u16().or_else(|| match server.scheme_str() {
        Some("http") => Some(80),
        Some("https") => Some(443),
        _ => None,
    });
    exclusions
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            if let Ok(network) = entry.parse::<ipnet::IpNet>() {
                return address.is_some_and(|address| network.contains(&address));
            }
            if let Ok(ip) = entry.parse::<IpAddr>() {
                return address == Some(ip);
            }
            let (entry_host, entry_port) = if let Some(rest) = entry.strip_prefix('[') {
                let Some((host, suffix)) = rest.split_once(']') else {
                    return false;
                };
                if suffix.is_empty() {
                    (host, None)
                } else if let Some(port) = suffix.strip_prefix(':') {
                    (host, Some(port))
                } else {
                    return false;
                }
            } else if let Some((host, port)) = entry.split_once(':') {
                (host, Some(port))
            } else {
                (entry, None)
            };
            if let Some(entry_port) = entry_port
                && (entry_port.is_empty() || entry_port.parse::<u16>().ok() != port)
            {
                return false;
            }
            if let Ok(ip) = entry_host.parse::<IpAddr>() {
                return address == Some(ip);
            }
            if address.is_some() {
                return false;
            }
            let entry_host = entry_host.to_ascii_lowercase();
            let domain = if entry_host.starts_with("*.") {
                &entry_host[1..]
            } else {
                &entry_host
            };
            if domain.is_empty() || domain == "." {
                return false;
            }
            if domain.starts_with('.') {
                host.ends_with(domain)
            } else {
                host == domain
                    || host
                        .strip_suffix(domain)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_match_hosts_addresses_networks_and_ports() {
        for (server, exclusions, expected) in [
            ("https://rke2-server:6443", "rke2-server", true),
            ("https://rke2-server:6443", " other, RKE2-SERVER , ", true),
            ("https://rke2-server:6443", "other", false),
            ("https://rke2-server:6443", "", false),
            ("https://rke2-server:6443", " , , ", false),
            ("https://api.example.com:6443", "example.com", true),
            ("https://example.com", "example.com", true),
            ("https://notexample.com", "example.com", false),
            ("https://example.com.evil", "example.com", false),
            ("https://api.example.com", ".example.com", true),
            ("https://example.com", ".example.com", false),
            ("https://api.example.com", "*.example.com", true),
            ("https://example.com", "*.example.com", false),
            ("https://api.example.com", "*example.com", false),
            ("https://rke2-server:6443", "rke2-server:6443", true),
            ("https://rke2-server:6443", "rke2-server:443", false),
            ("https://rke2-server", "rke2-server:443", true),
            ("http://rke2-server", "rke2-server:80", true),
            ("https://rke2-server", "rke2-server:", false),
            ("https://rke2-server", "rke2-server:invalid", false),
            ("https://10.20.30.40:6443", "10.20.30.40", true),
            ("https://10.20.30.40:6443", "10.20.30.40:6443", true),
            ("https://10.20.30.40:6443", "10.20.30.40:443", false),
            ("https://10.20.30.40", "10.20.0.0/16", true),
            ("https://10.21.30.40", "10.20.0.0/16", false),
            ("https://10.20.30.40", "10.20.0.0/99", false),
            ("https://10.20.30.40", "30.40", false),
            ("https://rke2-server", "10.20.0.0/16", false),
            ("https://[2001:db8::1]:6443", "2001:db8::1", true),
            ("https://[2001:db8::1]:6443", "[2001:db8::1]", true),
            ("https://[2001:db8::1]:6443", "[2001:db8::1]:6443", true),
            ("https://[2001:db8::1]:6443", "[2001:db8::1]:443", false),
            ("https://[2001:db8::1]", "2001:db8::/32", true),
            ("https://[2001:db9::1]", "2001:db8::/32", false),
            ("https://[2001:db8::1]", "[2001:db8::1", false),
            ("https://[2001:db8::1]", "[2001:db8::1]invalid", false),
            ("https://rke2-server", "*", true),
            ("https://10.20.30.40", "*", true),
            ("https://[2001:db8::1]", "*", true),
            ("https://rke2-server", "https://rke2-server", false),
        ] {
            assert_eq!(
                matches(&server.parse().unwrap(), exclusions),
                expected,
                "{server}, {exclusions}"
            );
        }
    }

    #[test]
    fn uppercase_exclusions_take_priority_unless_empty() {
        for (upper, lower, expected) in [
            (Some("upper"), Some("lower"), Some("upper")),
            (Some(""), Some("lower"), Some("lower")),
            (None, Some("lower"), Some("lower")),
            (Some("upper"), None, Some("upper")),
            (None, None, None),
            (Some(""), Some(""), None),
        ] {
            assert_eq!(
                first_nonempty(upper.map(str::to_owned), lower.map(str::to_owned)).as_deref(),
                expected
            );
        }
    }
}
