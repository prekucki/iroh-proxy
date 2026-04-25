use std::error::Error;
use std::fmt;
use std::str::FromStr;

use anyhow::bail;
use iroh::EndpointId;

const MAX_SERVICE_NAME_LEN: usize = 128;

#[derive(Debug, Clone)]
pub struct RemotePath {
    pub endpoint_id: EndpointId,
    pub service: Box<str>,
}

#[derive(Debug)]
struct ParseError(Box<str>);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for ParseError {}

pub fn validate_service_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("service name cannot be empty");
    }
    if name.len() > MAX_SERVICE_NAME_LEN {
        bail!("service name cannot be longer than {MAX_SERVICE_NAME_LEN} bytes");
    }
    if name
        .chars()
        .any(|ch| ch == '/' || ch.is_whitespace() || ch.is_control())
    {
        bail!("service name cannot contain '/', whitespace, or control characters");
    }
    Ok(())
}

/// Generate ALPN identifier for a service name
pub fn service_to_alpn(name: &str) -> Vec<u8> {
    format!("iroh-proxy/tcp/{name}").into_bytes()
}

impl RemotePath {
    /// Generate ALPN identifier for this remote path
    pub fn to_alpn(&self) -> Vec<u8> {
        service_to_alpn(&self.service)
    }
}

impl FromStr for RemotePath {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('/');
        let endpoint_raw = parts
            .next()
            .ok_or_else(|| ParseError("missing endpoint id in remote path".into()))?;
        let protocol = parts
            .next()
            .ok_or_else(|| ParseError("missing protocol segment in remote path".into()))?;
        let service = parts
            .next()
            .ok_or_else(|| ParseError("missing service segment in remote path".into()))?;

        if parts.next().is_some() {
            return Err(
                ParseError("remote path must be exactly <node-id>/tcp/<name>".into()).into(),
            );
        }
        if protocol != "tcp" {
            return Err(ParseError(
                format!("unsupported protocol '{protocol}', expected 'tcp'").into(),
            )
            .into());
        }
        validate_service_name(service)
            .map_err(|e| ParseError(format!("invalid service name '{service}': {e}").into()))?;

        let endpoint_id = EndpointId::from_str(endpoint_raw).map_err(|e| {
            ParseError(format!("invalid endpoint id '{endpoint_raw}' in remote path: {e}").into())
        })?;

        Ok(Self {
            endpoint_id,
            service: service.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPOINT_ID: &str = "74f3645e8016bb34970c516acde5240e85ed4387dbe3aeb9189f50db5525bd76";

    #[test]
    fn parses_valid_tcp_remote_path() {
        let path = format!("{ENDPOINT_ID}/tcp/ollama");
        let parsed = path.parse::<RemotePath>().expect("valid remote path");

        assert_eq!(parsed.endpoint_id.to_string(), ENDPOINT_ID);
        assert_eq!(parsed.service.as_ref(), "ollama");
        assert_eq!(parsed.to_alpn(), b"iroh-proxy/tcp/ollama");
    }

    #[test]
    fn rejects_non_tcp_or_extra_segments() {
        assert!(
            format!("{ENDPOINT_ID}/udp/ollama")
                .parse::<RemotePath>()
                .is_err()
        );
        assert!(
            format!("{ENDPOINT_ID}/tcp/ollama/extra")
                .parse::<RemotePath>()
                .is_err()
        );
    }

    #[test]
    fn rejects_invalid_service_names() {
        for service in ["", "bad/name", "bad name", "bad\nname"] {
            assert!(
                format!("{ENDPOINT_ID}/tcp/{service}")
                    .parse::<RemotePath>()
                    .is_err(),
                "service should be rejected: {service:?}"
            );
        }
    }
}
