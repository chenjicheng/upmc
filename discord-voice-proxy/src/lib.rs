pub mod discord;
pub mod installer;

use std::fmt;

/// SOCKS5 proxy configuration.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub address: String,
    pub port: u16,
    pub login: Option<String>,
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Serialize to proxy.txt format (key=value lines).
    pub fn to_proxy_txt(&self) -> String {
        format!(
            "SOCKS5_PROXY_ADDRESS={}\n\
             SOCKS5_PROXY_PORT={}\n\
             SOCKS5_PROXY_LOGIN={}\n\
             SOCKS5_PROXY_PASSWORD={}",
            self.address,
            self.port,
            self.login.as_deref().unwrap_or("empty"),
            self.password.as_deref().unwrap_or("empty"),
        )
    }

    /// Parse from proxy.txt format.
    pub fn from_proxy_txt(content: &str) -> anyhow::Result<Self> {
        let mut address = String::new();
        let mut port = 0u16;
        let mut login = None;
        let mut password = None;

        for line in content.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "SOCKS5_PROXY_ADDRESS" => address = value.trim().to_string(),
                "SOCKS5_PROXY_PORT" => port = value.trim().parse()?,
                "SOCKS5_PROXY_LOGIN" => {
                    let v = value.trim();
                    if v != "empty" && !v.is_empty() {
                        login = Some(v.to_string());
                    }
                }
                "SOCKS5_PROXY_PASSWORD" => {
                    let v = value.trim();
                    if v != "empty" && !v.is_empty() {
                        password = Some(v.to_string());
                    }
                }
                _ => {}
            }
        }

        anyhow::ensure!(!address.is_empty(), "SOCKS5_PROXY_ADDRESS not found");
        anyhow::ensure!(port > 0, "SOCKS5_PROXY_PORT not found or invalid");

        Ok(Self {
            address,
            port,
            login,
            password,
        })
    }
}

impl fmt::Display for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "socks5://{}:{}", self.address, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_proxy_config() {
        let config = ProxyConfig {
            address: "127.0.0.1".into(),
            port: 10808,
            login: Some("user".into()),
            password: Some("pass".into()),
        };
        let txt = config.to_proxy_txt();
        let parsed = ProxyConfig::from_proxy_txt(&txt).unwrap();
        assert_eq!(parsed.address, "127.0.0.1");
        assert_eq!(parsed.port, 10808);
        assert_eq!(parsed.login.as_deref(), Some("user"));
        assert_eq!(parsed.password.as_deref(), Some("pass"));
    }

    #[test]
    fn proxy_config_empty_credentials() {
        let config = ProxyConfig {
            address: "127.0.0.1".into(),
            port: 2080,
            login: None,
            password: None,
        };
        let txt = config.to_proxy_txt();
        let parsed = ProxyConfig::from_proxy_txt(&txt).unwrap();
        assert!(parsed.login.is_none());
        assert!(parsed.password.is_none());
    }
}
