//! Shared syntactic target validation; does not resolve names or open sockets.

use std::net::Ipv6Addr;

use anyhow::{Context, Result, ensure};
use url::Url;

use super::ProbeKind;

pub(crate) fn control_plane(value: &str) -> Result<()> {
    let url = web_url(value)?;
    ensure!(url.scheme() == "https", "control-plane URL must use HTTPS");
    ensure!(
        url.query().is_none(),
        "control-plane URL cannot contain a query"
    );
    ensure!(url.path() == "/", "control-plane URL must use a root path");
    Ok(())
}

pub(crate) fn probe(kind: ProbeKind, value: &str) -> Result<()> {
    ensure!(value.len() <= 2048, "probe target exceeds 2048 bytes");
    match kind {
        ProbeKind::Http => {
            web_url(value)?;
        }
        ProbeKind::Dns => dns_name(value)?,
        ProbeKind::Tcp | ProbeKind::Tls => socket(value)?,
    }
    Ok(())
}

pub(crate) fn web_url(value: &str) -> Result<Url> {
    ensure!(
        value.len() <= 2048 && !value.chars().any(char::is_whitespace) && !value.contains('\\'),
        "URL must be at most 2048 bytes without whitespace or backslashes"
    );
    ensure!(
        value.starts_with("http://") || value.starts_with("https://"),
        "URL must start with http:// or https://"
    );
    let url = Url::parse(value).context("invalid URL")?;
    ensure!(url.has_host(), "URL requires a host");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are not allowed"
    );
    ensure!(url.fragment().is_none(), "URL fragments are not allowed");
    ensure!(url.port() != Some(0), "URL port must be nonzero");
    Ok(url)
}

fn socket(value: &str) -> Result<()> {
    let (host, port) = value
        .rsplit_once(':')
        .context("target requires HOST:PORT")?;
    let port: u16 = port.parse().context("target port must be in 1..=65535")?;
    ensure!(port != 0, "target port must be in 1..=65535");
    if let Some(ipv6) = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        ipv6.parse::<Ipv6Addr>()
            .context("invalid bracketed IPv6 address")?;
    } else {
        dns_name(host)?;
    }
    Ok(())
}

fn dns_name(value: &str) -> Result<()> {
    let name = value.strip_suffix('.').unwrap_or(value);
    ensure!(
        !name.is_empty() && name.len() <= 253,
        "DNS name must contain 1..=253 bytes excluding a trailing dot"
    );
    ensure!(
        name.split('.').all(|label| !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
        "DNS names require ASCII labels of 1..=63 letters, digits, or internal hyphens"
    );
    Ok(())
}
