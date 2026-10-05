use crate::{Config, Controller, catalog};
use anyhow::{Context, Result, ensure};
use pier_protocol::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Repository {
    pub url: String,
    #[serde(default = "crate::default_ref")]
    pub reference: String,
}
impl Repository {
    pub fn validate(&self) -> Result<()> {
        for (value, limit, field) in [
            (&self.url, 4096, "repository URL"),
            (&self.reference, 256, "repository reference"),
        ] {
            ensure!(
                !value.is_empty()
                    && value.len() <= limit
                    && value.trim() == value
                    && !value.starts_with('-')
                    && !value.chars().any(char::is_control),
                "invalid {field}"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub repository: Option<Repository>,
    pub catalog_repository: Option<Repository>,
    pub public_url: Option<String>,
    pub agent_endpoint: Option<String>,
    pub sync_error: Option<String>,
    #[serde(default)]
    pub runtime: Option<crate::RuntimeSettings>,
}
impl Settings {
    pub fn load(store: &Store, config: &Config, has_catalog: bool) -> Result<Self> {
        let saved: Option<Self> = store.get("settings", "controller")?;
        let legacy = saved.is_none();
        let mut settings = saved.unwrap_or_default();
        // YAML repository settings are a one-time import, never an override of
        // settings subsequently saved in the web console.
        if legacy && !config.repository.url.is_empty() {
            let repository = Repository {
                url: config.repository.url.clone(),
                reference: config.repository.reference.clone(),
            };
            repository.validate()?;
            if has_catalog {
                settings.catalog_repository = Some(repository.clone());
            }
            settings.repository = Some(repository);
        }
        // Version 2 imports the formerly live YAML overrides exactly once.
        if settings.runtime.is_none() {
            let runtime = crate::RuntimeSettings {
                tcp_listen: config.tcp_listen,
                public_url: if config.public_url.is_empty() {
                    settings.public_url.take().unwrap_or_default()
                } else {
                    config.public_url.clone()
                },
                agent_endpoint: if config.agent_endpoint.is_empty() {
                    settings.agent_endpoint.take().unwrap_or_default()
                } else {
                    config.agent_endpoint.clone()
                },
                max_concurrent_builds: config.max_concurrent_builds,
                build_proxy: config.build_proxy.clone(),
            };
            runtime.validate(store.get::<Value>("auth", "admin")?.is_some())?;
            settings.runtime = Some(runtime);
            settings.public_url = None;
            settings.agent_endpoint = None;
        }
        store.put("settings", "controller", &settings)?;
        Ok(settings)
    }
}
impl Controller {
    // Callers creating deployments hold mutation_lock while checking this and
    // cloning the catalog so a repository change cannot race the deployment.
    pub(crate) fn repository_needs_sync(&self) -> bool {
        let settings = self.settings.read().unwrap();
        settings.repository.is_none()
            || settings.repository != settings.catalog_repository
            || self.catalog.read().unwrap().is_none()
    }
    pub(crate) fn repository_view(&self) -> Value {
        let settings = self.settings.read().unwrap();
        let catalog = self.catalog.read().unwrap();
        json!({
            "repository": settings.repository,
            "commit": catalog.as_ref().map(|c| &c.commit),
            "error": settings.sync_error,
            "needs_sync": settings.repository.is_none()
                || settings.repository != settings.catalog_repository || catalog.is_none(),
        })
    }
    pub(crate) fn save_repository(&self, repository: Repository) -> Result<()> {
        repository.validate()?;
        let _sync = self.sync_lock.lock().unwrap();
        let _mutation = self.mutation_lock.lock().unwrap();
        let mut settings = self.settings.write().unwrap();
        let mut updated = settings.clone();
        if updated.repository.as_ref() != Some(&repository) {
            updated.repository = Some(repository);
            updated.sync_error = None;
        }
        self.store.put("settings", "controller", &updated)?;
        *settings = updated;
        Ok(())
    }
    /// Fetch and validate the definition repository only on an explicit request.
    pub fn sync(&self) -> Result<String> {
        let _sync = self.sync_lock.lock().unwrap();
        let repository = self
            .settings
            .read()
            .unwrap()
            .repository
            .clone()
            .context("repository not configured")?;
        // Saved settings take effect only after restart, just as for builds.
        // Internal callers may sync before initialization; those connect directly.
        let proxy = self
            .runtime
            .read()
            .unwrap()
            .as_ref()
            .map(|runtime| runtime.settings.build_proxy.clone())
            .unwrap_or_default();
        let result = catalog::sync_with_proxy(
            &repository.url,
            &repository.reference,
            &self.config.state_dir,
            &proxy,
        );
        let _mutation = self.mutation_lock.lock().unwrap();
        let mut settings = self.settings.write().unwrap();
        let mut updated = settings.clone();
        match result {
            Ok(catalog) => {
                updated.catalog_repository = Some(repository);
                updated.sync_error = None;
                self.store.put_pair(
                    ("settings", "controller", &updated),
                    ("catalog", "current", &catalog),
                )?;
                let commit = catalog.commit.clone();
                *self.catalog.write().unwrap() = Some(catalog);
                *settings = updated;
                Ok(commit)
            }
            Err(error) => {
                updated.sync_error = Some(
                    "repository sync or catalog validation failed; previous catalog retained"
                        .into(),
                );
                self.store.put("settings", "controller", &updated)?;
                *settings = updated;
                tracing::warn!("repository sync or catalog validation failed");
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests;
