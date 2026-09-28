use crate::{Error, ProxyOptions, Result, Stage};
use std::{collections::BTreeMap, net::IpAddr, process::Command, time::Duration};
use url::Url;

pub(crate) const PROXY_KEYS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "all_proxy",
];
pub(crate) fn is_proxy_key(key: &str) -> bool {
    PROXY_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k))
}
pub(crate) fn http_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| Error::new(Stage::Proxy, "invalid HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || raw.contains(['\0', '\n', '\r'])
    {
        return Err(Error::new(Stage::Proxy, "invalid HTTP(S) URL"));
    }
    Ok(url)
}

#[derive(Clone, Default)]
pub(crate) struct Proxy {
    http: Option<String>,
    https: Option<String>,
    bypass: String,
}
impl Proxy {
    pub fn new(enabled: bool, options: &ProxyOptions, build: bool) -> Result<Self> {
        if !enabled {
            return Ok(Self::default());
        }
        if options.http_proxy.is_none() && options.https_proxy.is_none() {
            return Err(Error::new(
                Stage::Proxy,
                "proxy.enabled requires a proxy address from the caller",
            ));
        }
        for address in [&options.http_proxy, &options.https_proxy]
            .into_iter()
            .flatten()
        {
            let url = http_url(address)?;
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                return Err(Error::new(
                    Stage::Proxy,
                    "proxy URL cannot contain a path, query or fragment",
                ));
            }
            let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
            if build
                && (host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
                    || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
            {
                return Err(Error::new(
                    Stage::Proxy,
                    "build proxy must be reachable from the container default network; loopback addresses are not allowed",
                ));
            }
        }
        let bypass = options.no_proxy.clone().unwrap_or_default();
        if bypass.contains(['\0', '\n', '\r']) {
            return Err(Error::new(Stage::Proxy, "invalid no_proxy list"));
        }
        Ok(Self {
            http: options.http_proxy.clone(),
            https: options.https_proxy.clone(),
            bypass,
        })
    }
    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut vars: BTreeMap<_, _> = PROXY_KEYS
            .iter()
            .map(|k| (k.to_string(), String::new()))
            .collect();
        for (keys, value) in [
            (
                ["HTTP_PROXY", "http_proxy"],
                self.http.as_deref().unwrap_or(""),
            ),
            (
                ["HTTPS_PROXY", "https_proxy"],
                self.https.as_deref().unwrap_or(""),
            ),
            (["NO_PROXY", "no_proxy"], self.bypass.as_str()),
        ] {
            for k in keys {
                vars.insert(k.into(), value.into());
            }
        }
        vars
    }
    pub fn command(&self, command: &mut Command) {
        for key in PROXY_KEYS {
            command.env_remove(key);
        }
        for (key, value) in self.environment() {
            if !value.is_empty() {
                command.env(key, value);
            }
        }
    }
    pub fn client(&self) -> Result<reqwest::blocking::Client> {
        let mut builder = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(1800))
            .redirect(reqwest::redirect::Policy::limited(10));
        let bypass = reqwest::NoProxy::from_string(&self.bypass);
        for (is_https, value) in [(false, &self.http), (true, &self.https)] {
            if let Some(value) = value {
                let p = if is_https {
                    reqwest::Proxy::https(value)
                } else {
                    reqwest::Proxy::http(value)
                }
                .map_err(|_| Error::new(Stage::Proxy, "cannot configure proxy"))?
                .no_proxy(bypass.clone());
                builder = builder.proxy(p);
            }
        }
        builder
            .build()
            .map_err(|_| Error::new(Stage::Download, "cannot initialize HTTP client"))
    }
    pub fn git_proxy(&self, repo: &str) -> &str {
        if repo.starts_with("https://") {
            self.https.as_deref().unwrap_or("")
        } else {
            self.http.as_deref().unwrap_or("")
        }
    }
    pub fn redactions(&self) -> Vec<String> {
        let mut values = Vec::new();
        for raw in [&self.http, &self.https].into_iter().flatten() {
            values.push(raw.clone());
            if let Ok(url) = Url::parse(raw) {
                if !url.username().is_empty() {
                    values.push(url.username().into());
                }
                if let Some(p) = url.password() {
                    values.push(p.into());
                }
            }
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_environment_is_explicit_and_scoped() {
        let options = ProxyOptions {
            http_proxy: Some("http://user:password@proxy.example:8080".into()),
            https_proxy: Some("http://proxy.example:8081".into()),
            no_proxy: Some("internal.example".into()),
        };
        let proxy = Proxy::new(true, &options, true).unwrap();
        let env = proxy.environment();
        assert_eq!(env["HTTP_PROXY"], env["http_proxy"]);
        assert_eq!(env["HTTPS_PROXY"], env["https_proxy"]);
        assert_eq!(env["NO_PROXY"], "internal.example");
        assert_eq!(env["ALL_PROXY"], "");
        let disabled = Proxy::new(false, &options, true).unwrap();
        let mut command = Command::new("git");
        disabled.command(&mut command);
        let overrides: BTreeMap<_, _> = command
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().to_string(), v))
            .collect();
        for key in PROXY_KEYS {
            assert_eq!(overrides[key], None);
        }
        assert!(!format!("{options:?}").contains("password"));
    }
    #[test]
    fn loopback_is_only_valid_for_host_downloads() {
        for address in [
            "http://127.0.0.1:7890",
            "http://localhost:7890",
            "http://[::1]:7890",
        ] {
            let options = ProxyOptions {
                http_proxy: Some(address.into()),
                ..Default::default()
            };
            assert!(Proxy::new(true, &options, true).is_err());
            assert!(Proxy::new(true, &options, false).is_ok());
        }
    }
}
