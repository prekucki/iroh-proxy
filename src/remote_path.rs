use std::error::Error;
use std::fmt;
use std::str::FromStr;

use iroh::EndpointId;

#[derive(Debug, Clone)]
pub struct RemotePath {
    pub endpoint_id: EndpointId,
    pub service: String,
}

#[derive(Debug)]
struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for ParseError {}

impl RemotePath {
    pub fn to_alpn(&self) -> Vec<u8> {
        format!("iroh-proxy/tcp/{}", self.service).into_bytes()
    }
}

impl FromStr for RemotePath {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('/');
        let endpoint_raw = parts
            .next()
            .ok_or_else(|| ParseError("missing endpoint id in remote path".to_string()))?;
        let protocol = parts
            .next()
            .ok_or_else(|| ParseError("missing protocol segment in remote path".to_string()))?;
        let service = parts
            .next()
            .ok_or_else(|| ParseError("missing service segment in remote path".to_string()))?;

        if parts.next().is_some() {
            return Err(
                ParseError("remote path must be exactly <node-id>/tcp/<name>".to_string()).into(),
            );
        }
        if protocol != "tcp" {
            return Err(
                ParseError(format!("unsupported protocol '{protocol}', expected 'tcp'")).into(),
            );
        }

        let endpoint_id = EndpointId::from_str(endpoint_raw).map_err(|e| {
            ParseError(format!(
                "invalid endpoint id in remote path: {endpoint_raw}: {e}"
            ))
        })?;

        Ok(Self {
            endpoint_id,
            service: service.to_string(),
        })
    }
}
