use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use iroh::EndpointId;

#[derive(Debug, Clone)]
pub struct RemotePath {
    pub endpoint_id: EndpointId,
    pub service: String,
}

impl FromStr for RemotePath {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut parts = value.split('/');
        let endpoint_raw = parts
            .next()
            .ok_or_else(|| anyhow!("missing endpoint id in remote path"))?;
        let protocol = parts
            .next()
            .ok_or_else(|| anyhow!("missing protocol segment in remote path"))?;
        let service = parts
            .next()
            .ok_or_else(|| anyhow!("missing service segment in remote path"))?;

        if parts.next().is_some() {
            bail!("remote path must be exactly <node-id>/tcp/<name>");
        }
        if protocol != "tcp" {
            bail!("unsupported protocol '{protocol}', expected 'tcp'");
        }

        let endpoint_id = EndpointId::from_str(endpoint_raw)
            .with_context(|| format!("invalid endpoint id in remote path: {endpoint_raw}"))?;

        Ok(Self {
            endpoint_id,
            service: service.to_string(),
        })
    }
}

pub fn alpn_for_service(name: &str) -> Vec<u8> {
    format!("iroh-proxy/tcp/{name}").into_bytes()
}
