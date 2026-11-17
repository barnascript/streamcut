//! Downloads a video from a URL someone pasted.
//!
//! A URL from a user is untrusted. Without care, "http://169.254.169.254/"
//! would make my worker read the cloud metadata service, or a redirect would
//! walk into the internal network. So:
//!   - http and https only
//!   - every address the host resolves to must be public
//!   - I connect to the exact address I checked (no second DNS lookup that
//!     could give a different answer)
//!   - redirects are followed by hand, each one checked the same way
//!   - the download stops at a size limit

use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::media::BadInput;

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64/10 carrier NAT
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18/15 benchmarking
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        }
    }
}

async fn checked_addr(url: &Url, allow_private: bool) -> Result<SocketAddr> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BadInput(format!(
            "only http and https links work, not {}",
            url.scheme()
        ))
        .into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| BadInput("the link has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| BadInput(format!("could not find {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(BadInput(format!("could not find {host}")).into());
    }
    if !allow_private && addrs.iter().any(|a| !is_public(a.ip())) {
        return Err(BadInput(format!(
            "{host} points to a private address, which is not allowed"
        ))
        .into());
    }
    Ok(addrs[0])
}

/// Returns a file name to show for the download.
pub async fn download(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    allow_private: bool,
) -> Result<String> {
    let mut url =
        Url::parse(url).map_err(|e| BadInput(format!("that is not a valid link: {e}")))?;
    for _ in 0..5 {
        let addr = checked_addr(&url, allow_private).await?;
        let host = url.host_str().unwrap_or_default().to_string();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve(&host, addr)
            .user_agent(concat!(
                "streamcut/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/barnascript/streamcut)"
            ))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        let resp = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| BadInput(format!("download failed: {e}")))?;
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("redirect without a Location header"))?;
            url = url.join(loc)?;
            continue;
        }
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            // Their side is busy or broken: worth another try later.
            bail!("the link answered {status}, will try again");
        }
        if !status.is_success() {
            return Err(BadInput(format!("the link answered {status}")).into());
        }
        if let Some(len) = resp.content_length()
            && len > max_bytes
        {
            return Err(BadInput(format!(
                "the file is {} MB, the limit is {} MB",
                len >> 20,
                max_bytes >> 20
            ))
            .into());
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut got: u64 = 0;
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            got += chunk.len() as u64;
            if got > max_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                return Err(BadInput(format!(
                    "the file is bigger than the {} MB limit",
                    max_bytes >> 20
                ))
                .into());
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        let name = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .unwrap_or_else(|| host.clone());
        return Ok(name);
    }
    bail!(BadInput("too many redirects".into()))
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_private_and_special_addresses() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!is_public(ip(bad)), "{bad} should be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for good in ["8.8.8.8", "104.16.0.1", "2606:4700::1111"] {
            assert!(is_public(ip(good)), "{good} should be allowed");
        }
    }

    #[tokio::test]
    async fn refuses_other_schemes() {
        let err = download("file:///etc/passwd", Path::new("/tmp/x"), 10, false)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<BadInput>().is_some());
    }

    #[tokio::test]
    async fn refuses_localhost() {
        let err = download("http://localhost:6379/", Path::new("/tmp/x"), 10, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("private address"), "{err}");
    }

    #[test]
    fn decodes_names() {
        assert_eq!(percent_decode("STS-131%20launch.ogv"), "STS-131 launch.ogv");
    }
}
