use crate::{BuildProxy, Controller, connection};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    net::{SocketAddr, TcpListener},
    sync::Arc,
};

/// Persisted operational settings. Edits become active on the next restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    pub tcp_listen: SocketAddr,
    pub public_url: String,
    pub agent_endpoint: String,
    pub max_concurrent_builds: usize,
    pub build_proxy: BuildProxy,
}
impl RuntimeSettings {
    pub(crate) fn validate(&self, initialized: bool) -> Result<()> {
        ensure!(
            self.tcp_listen.port() != 0,
            "agent listen port must be 1..65535"
        );
        ensure!(
            (1..=64).contains(&self.max_concurrent_builds),
            "max_concurrent_builds must be 1..64"
        );
        if initialized || !self.public_url.is_empty() {
            ensure!(
                pier_protocol::enrollment::origin(&self.public_url)? == self.public_url,
                "public_url must be a canonical HTTP or HTTPS origin"
            );
        }
        if initialized || !self.agent_endpoint.is_empty() {
            pier_protocol::enrollment::endpoint(&self.agent_endpoint)?;
        }
        for value in [&self.build_proxy.http_proxy, &self.build_proxy.https_proxy]
            .into_iter()
            .flatten()
        {
            // Do not include user-supplied URLs (which may contain credentials) in errors.
            let valid = url::Url::parse(value).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.host_str().is_some()
                    && u.path() == "/"
                    && u.query().is_none()
                    && u.fragment().is_none()
            });
            ensure!(
                valid && value.len() <= 4096 && !value.chars().any(char::is_control),
                "invalid HTTP(S) proxy URL"
            );
        }
        if let Some(value) = &self.build_proxy.no_proxy {
            ensure!(
                value.len() <= 4096 && !value.chars().any(char::is_control),
                "invalid no_proxy list"
            );
        }
        Ok(())
    }
    pub(crate) fn setup_view(&self) -> Value {
        json!({"tcp_listen":self.tcp_listen, "public_url":self.public_url,
            "agent_endpoint":self.agent_endpoint, "max_concurrent_builds":self.max_concurrent_builds,
            "proxy_configured":self.build_proxy != BuildProxy::default()})
    }
}

/// Omitted fields preserve the saved value. An explicit empty proxy object clears it.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RuntimePatch {
    tcp_listen: Option<SocketAddr>,
    public_url: Option<String>,
    agent_endpoint: Option<String>,
    max_concurrent_builds: Option<usize>,
    build_proxy: Option<BuildProxy>,
}
impl RuntimePatch {
    pub(crate) fn apply(
        self,
        mut value: RuntimeSettings,
        origin: Option<&str>,
    ) -> Result<RuntimeSettings> {
        if let Some(v) = self.tcp_listen {
            value.tcp_listen = v;
        }
        if let Some(v) = self.public_url {
            value.public_url = v;
        }
        if let Some(v) = self.agent_endpoint {
            value.agent_endpoint = v;
        }
        if let Some(v) = self.max_concurrent_builds {
            value.max_concurrent_builds = v;
        }
        if let Some(v) = self.build_proxy {
            value.build_proxy = v;
        }
        if let Some(origin) = origin {
            if value.public_url.is_empty() {
                value.public_url = origin.into();
            }
            ensure!(
                value.public_url == origin,
                "public_url must match the initialization origin"
            );
            if value.agent_endpoint.is_empty() {
                let url = url::Url::parse(origin)?;
                value.agent_endpoint = format!(
                    "{}:{}",
                    url.host().context("missing host")?,
                    value.tcp_listen.port()
                );
            }
        }
        value.validate(true)?;
        Ok(value)
    }
}

pub(crate) struct Runtime {
    pub settings: RuntimeSettings,
    pub build_slots: Arc<tokio::sync::Semaphore>,
}
impl Runtime {
    pub fn new(settings: RuntimeSettings) -> Result<Self> {
        settings.validate(true)?;
        Ok(Self {
            build_slots: Arc::new(tokio::sync::Semaphore::new(settings.max_concurrent_builds)),
            settings,
        })
    }
}
#[derive(Default, Serialize)]
pub(crate) struct ListenerStatus {
    pub listening: bool,
    pub error: Option<String>,
}
impl Controller {
    pub(crate) fn active_runtime(&self) -> Result<Arc<Runtime>> {
        self.runtime
            .read()
            .unwrap()
            .clone()
            .context("controller not initialized")
    }
    pub(crate) fn agent_ready(&self) -> bool {
        self.listener_status.read().unwrap().listening
    }
    pub(crate) fn public_url(&self) -> Option<String> {
        if let Some(runtime) = self.runtime.read().unwrap().as_ref() {
            return Some(runtime.settings.public_url.clone());
        }
        self.settings
            .read()
            .unwrap()
            .runtime
            .as_ref()
            .map(|s| s.public_url.clone())
            .filter(|s| !s.is_empty())
    }
    pub(crate) fn agent_endpoint(&self) -> Result<String> {
        Ok(self.active_runtime()?.settings.agent_endpoint.clone())
    }
    pub(crate) fn runtime_view(&self) -> Value {
        let saved = self.settings.read().unwrap().runtime.clone().unwrap();
        let active = self
            .runtime
            .read()
            .unwrap()
            .as_ref()
            .map(|r| r.settings.clone());
        json!({"restart_required":active.as_ref() != Some(&saved), "active":active,
            "saved":saved, "agent_listener":*self.listener_status.read().unwrap()})
    }
    pub(crate) fn save_runtime(&self, patch: RuntimePatch) -> Result<(), crate::api::ApiError> {
        let _guard = self.mutation_lock.lock().unwrap();
        let mut saved = self.settings.write().unwrap();
        let mut next = saved.clone();
        next.runtime = Some(
            patch
                .apply(next.runtime.unwrap(), None)
                .map_err(|e| crate::api::bad(&e.to_string()))?,
        );
        self.store.put("settings", "controller", &next)?;
        *saved = next;
        Ok(())
    }
    pub(crate) fn prepare_listener(settings: &RuntimeSettings) -> Result<TcpListener> {
        let listener = TcpListener::bind(settings.tcp_listen)
            .context("agent listener unavailable; check its address, port and permissions")?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }
    pub(crate) fn install_listener(&self, listener: TcpListener) {
        *self.listener.lock().unwrap() = Some(listener);
        *self.listener_status.write().unwrap() = ListenerStatus {
            listening: true,
            error: None,
        };
        self.listener_ready.notify_one();
    }
    pub(crate) fn start_saved_listener(&self) {
        if let Ok(runtime) = self.active_runtime() {
            match Self::prepare_listener(&runtime.settings) {
                Ok(listener) => self.install_listener(listener),
                Err(_) => self.listener_failed(),
            }
        }
    }
    fn listener_failed(&self) {
        *self.listener_status.write().unwrap() = ListenerStatus {
            listening: false,
            error: Some(
                "agent listener unavailable; correct settings and restart controller".into(),
            ),
        };
        tracing::warn!("agent listener unavailable; web settings remain accessible");
    }
    pub(crate) async fn serve_agent_listener(self: Arc<Self>) {
        loop {
            self.listener_ready.notified().await;
            let listener = self.listener.lock().unwrap().take();
            if let Some(listener) = listener {
                let result = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => connection::listen(self.clone(), listener).await,
                    Err(error) => Err(error.into()),
                };
                if result.is_err() {
                    self.listener_failed();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
