use std::net::IpAddr;

use crate::error::{Result, invalid_argument};

/// A connection target for one adb server. Two spellings of the same local
/// server must produce the same `canonical` string so the device lock cannot
/// be bypassed by renaming the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbEndpoint {
    host: String,
    port: u16,
}

impl AdbEndpoint {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(invalid_argument("adb server address is empty"));
        }
        let without_scheme = trimmed
            .strip_prefix("tcp://")
            .or_else(|| trimmed.strip_prefix("tcp:"))
            .unwrap_or(trimmed);

        let (host, port_text) = split_host_port(without_scheme)?;
        let host = canonical_ip(host)?;
        let port: u16 = port_text.parse().map_err(|_| {
            invalid_argument("adb server port must be a number between 1 and 65535")
        })?;
        if port == 0 {
            return Err(invalid_argument("adb server port must be at least 1"));
        }

        Ok(Self { host, port })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn canonical(&self) -> String {
        if self.host.contains(':') {
            format!("tcp://[{}]:{}", self.host, self.port)
        } else {
            format!("tcp://{}:{}", self.host, self.port)
        }
    }

    pub fn port_text(&self) -> String {
        self.port.to_string()
    }
}

fn split_host_port(input: &str) -> Result<(&str, &str)> {
    if let Some(rest) = input.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err(invalid_argument("adb server host has an unclosed bracket"));
        };
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        let Some(port) = tail.strip_prefix(':') else {
            return Err(invalid_argument(
                "adb server address must include a port, e.g. [::1]:5037",
            ));
        };
        if port.is_empty() {
            return Err(invalid_argument("adb server address is missing a port"));
        }
        return Ok((host, port));
    }
    if input.matches(':').count() > 1 {
        return Err(invalid_argument(
            "IPv6 adb server hosts must be bracketed, e.g. [::1]:5037",
        ));
    }
    let Some((host, port)) = input.rsplit_once(':') else {
        return Err(invalid_argument(
            "adb server address must be HOST:PORT, e.g. 127.0.0.1:5037",
        ));
    };
    if port.is_empty() {
        return Err(invalid_argument("adb server address is missing a port"));
    }
    Ok((host, port))
}

fn canonical_ip(host: &str) -> Result<String> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok("127.0.0.1".to_owned());
    }
    let address = host
        .parse::<IpAddr>()
        .map_err(|_| invalid_argument("adb server host must be an IP address or localhost"))?;
    if address.is_unspecified() {
        return Err(invalid_argument(
            "adb server address must not be an unspecified IP address",
        ));
    }
    match address {
        IpAddr::V4(address) => Ok(address.to_string()),
        IpAddr::V6(address) if address.to_ipv4_mapped().is_some() => Ok(address
            .to_ipv4_mapped()
            .expect("checked mapped IPv4")
            .to_string()),
        IpAddr::V6(address) if address.is_loopback() => Ok("127.0.0.1".to_owned()),
        IpAddr::V6(address) => Ok(address.to_string()),
    }
}

impl std::fmt::Display for AdbEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.canonical())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn loopback_aliases_canonicalize_together() {
        let expected = "tcp://127.0.0.1:5037";
        for spelling in [
            "127.0.0.1:5037",
            "localhost:5037",
            "LOCALHOST:5037",
            "tcp:localhost:5037",
            "tcp://127.0.0.1:5037",
            "[::1]:5037",
        ] {
            let endpoint = AdbEndpoint::parse(spelling).expect(spelling);
            assert_eq!(endpoint.canonical(), expected, "{spelling}");
        }
    }

    #[test]
    fn non_loopback_ipv6_is_bracketed() {
        let endpoint = AdbEndpoint::parse("[2001:db8::1]:5038").unwrap();
        assert_eq!(endpoint.canonical(), "tcp://[2001:db8::1]:5038");
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_spellings() {
        for spelling in [
            "",
            "localhost",
            "127.0.0.1:",
            "127.0.0.1:0",
            "127.0.0.1:70000",
            "0.0.0.0:5037",
            "::1:5037",
            "host.example:5037",
            "tcp://host/../x:5037",
            "user@host:5037",
        ] {
            let error = AdbEndpoint::parse(spelling).expect_err(spelling);
            assert_eq!(error.code, ErrorCode::InvalidArgument, "{spelling}");
        }
    }
}
