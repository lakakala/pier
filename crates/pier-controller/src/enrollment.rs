use crate::{
    AgentRecord, Controller,
    proxy::{Patch, Proxy, Route},
};
use anyhow::{Result, ensure};
use pier_protocol::{
    AgentReport,
    connection::{Connection, ConnectionMode},
    enrollment::{Credentials, InitRequest, Pairing},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Enrollment {
    request: InitRequest,
    #[serde(default)]
    connection: Connection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy: Option<Proxy>,
    pairing: Option<Pairing>,
    issued: Option<Credentials>,
    agent_id: Option<String>,
    expires_at: u64,
    completed: bool,
}
impl Controller {
    #[cfg(test)]
    pub(crate) fn approve_enrollment(&self, request: InitRequest) -> Result<Pairing> {
        self.approve_connection(request, None, Patch::Preserve)
    }
    pub(crate) fn approve_connection(
        &self,
        request: InitRequest,
        endpoint: Option<String>,
        proxy_patch: Patch,
    ) -> Result<Pairing> {
        let connection = Connection {
            mode: request.connection_mode,
            endpoint,
        };
        connection.validate()?;
        request.validate()?;
        ensure!(
            Some(request.public_url.clone()) == self.public_url(),
            "controller origin mismatch"
        );
        let _guard = self.mutation_lock.lock().unwrap();
        let old: Option<Enrollment> = self.store.get("enrollments", &request.request_id)?;
        let proxy = proxy_patch.apply(old.as_ref().and_then(|r| r.proxy.clone()));
        ensure!(
            connection.mode == ConnectionMode::ControllerToAgent || proxy.is_none(),
            "proxy requires controller-to-agent mode"
        );
        if let Some(old) = &old {
            ensure!(
                old.request == request && old.connection == connection && !old.completed,
                "enrollment already complete or changed"
            );
            if let Some(pairing) = &old.pairing {
                if pairing.expires_at > pier_protocol::now() {
                    let mut updated = old.clone();
                    updated.proxy = proxy.clone();
                    if let Some(agent_id) = &old.agent_id {
                        let mut agent: AgentRecord = self
                            .store
                            .get("agents", agent_id)?
                            .ok_or_else(|| anyhow::anyhow!("enrollment agent missing"))?;
                        // Credentials may already have been delivered. Never rotate them
                        // or extend the original grant while correcting a proxy.
                        ensure!(
                            !self.upgrade_busy(agent_id)?
                                && !self
                                    .store
                                    .list::<crate::Job>("jobs")?
                                    .iter()
                                    .any(|j| j.agent_id == *agent_id && j.active()),
                            "agent is deploying or upgrading"
                        );
                        agent.proxy = proxy;
                        self.store.put_pair(
                            ("enrollments", &request.request_id, &updated),
                            ("agents", agent_id, &agent),
                        )?;
                        if let Some(session) = self.sessions.lock().unwrap().get(agent_id) {
                            session.cancelled.cancel();
                        }
                        self.dialer.cancel(agent_id);
                    } else {
                        self.store
                            .put("enrollments", &request.request_id, &updated)?;
                    }
                    self.dialer
                        .cancel(&format!("enroll:{}", request.request_id));
                    return Ok(pairing.clone());
                }
            }
        }
        let pairing = Pairing {
            connection_mode: request.connection_mode,
            version: pier_protocol::VERSION,
            grant_id: request.request_id.clone(),
            request_id: request.request_id.clone(),
            public_url: self
                .public_url()
                .ok_or_else(|| anyhow::anyhow!("controller not initialized"))?,
            endpoint: match &connection.endpoint {
                Some(endpoint) => endpoint.clone(),
                None => self.agent_endpoint()?,
            },
            secret: pier_protocol::new_token(),
            expires_at: pier_protocol::now() + 600,
        };
        let record = Enrollment {
            request,
            connection,
            proxy,
            pairing: Some(pairing.clone()),
            issued: None,
            agent_id: old.and_then(|v| v.agent_id),
            expires_at: pairing.expires_at,
            completed: false,
        };
        self.store.put("enrollments", &pairing.grant_id, &record)?;
        tracing::info!(request_id=%pairing.request_id, "agent enrollment authorized");
        Ok(pairing)
    }
    pub(crate) fn enrollment_key(&self, id: &str) -> Result<[u8; 32]> {
        let record: Enrollment = self
            .store
            .get("enrollments", id)?
            .ok_or_else(|| anyhow::anyhow!("unknown enrollment"))?;
        ensure!(
            !record.completed && record.expires_at > pier_protocol::now(),
            "enrollment expired"
        );
        let pairing = record
            .pairing
            .ok_or_else(|| anyhow::anyhow!("enrollment expired"))?;
        pier_protocol::secure::decode_key(&pairing.secret)
    }
    pub(crate) fn issue_credentials(
        &self,
        id: &str,
        request: InitRequest,
        key: &[u8; 32],
    ) -> Result<Credentials> {
        let _guard = self.mutation_lock.lock().unwrap();
        let mut record: Enrollment = self
            .store
            .get("enrollments", id)?
            .ok_or_else(|| anyhow::anyhow!("unknown enrollment"))?;
        ensure!(
            self.enrollment_key(id)? == *key && record.request == request,
            "enrollment changed or expired"
        );
        if let Some(credentials) = record.issued {
            return Ok(credentials);
        }
        let agent_id = record
            .agent_id
            .clone()
            .unwrap_or_else(pier_protocol::new_id);
        let old: Option<AgentRecord> = self.store.get("agents", &agent_id)?;
        ensure!(
            old.is_none_or(|a| a.last_seen.is_none()),
            "agent has already connected; use saved credentials"
        );
        let credentials = Credentials {
            connection_mode: request.connection_mode,
            listen: request.listen,
            agent_id: agent_id.clone(),
            token: pier_protocol::new_token(),
            controller_tcp: if request.connection_mode == ConnectionMode::AgentToController {
                self.agent_endpoint()?
            } else {
                String::new()
            },
        };
        let agent = AgentRecord {
            proxy: record.proxy.clone(),
            connection: record.connection.clone(),
            id: agent_id.clone(),
            name: request.name,
            token_hash: pier_protocol::hash(&credentials.token),
            info: Some(request.info),
            last_seen: None,
            report: AgentReport::default(),
        };
        record.agent_id = Some(agent_id.clone());
        record.issued = Some(credentials.clone());
        self.store
            .put_pair(("enrollments", id, &record), ("agents", &agent_id, &agent))?;
        tracing::info!(%agent_id, "agent enrollment redeemed");
        Ok(credentials)
    }
    pub(crate) fn acknowledge_enrollment(&self, request_id: &str, agent_id: &str) -> Result<()> {
        let _guard = self.mutation_lock.lock().unwrap();
        let mut record: Enrollment = self
            .store
            .get("enrollments", request_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown enrollment"))?;
        ensure!(
            record.agent_id.as_deref() == Some(agent_id),
            "enrollment belongs to another agent"
        );
        record.completed = true;
        record.pairing = None;
        record.issued = None;
        self.store.put("enrollments", request_id, &record)?;
        Ok(())
    }
    pub(crate) fn expire_enrollments(&self) -> Result<()> {
        let _guard = self.mutation_lock.lock().unwrap();
        for mut record in self.store.list::<Enrollment>("enrollments")? {
            if record.expires_at <= pier_protocol::now()
                && (record.pairing.is_some() || record.issued.is_some())
            {
                record.pairing = None;
                record.issued = None;
                self.store
                    .put("enrollments", &record.request.request_id, &record)?;
            }
        }
        Ok(())
    }
    pub(crate) fn passive_enrollments(&self) -> Result<Vec<(String, Route)>> {
        Ok(self
            .store
            .list::<Enrollment>("enrollments")?
            .into_iter()
            .filter(|r| {
                !r.completed
                    && r.expires_at > pier_protocol::now()
                    && r.pairing.is_some()
                    && r.connection.mode == ConnectionMode::ControllerToAgent
            })
            .filter_map(|r| {
                r.connection.endpoint.map(|endpoint| {
                    (
                        r.request.request_id,
                        Route {
                            endpoint,
                            proxy: r.proxy,
                        },
                    )
                })
            })
            .collect())
    }
    pub(crate) fn complete_passive_enrollment(&self, agent: &str) -> Result<()> {
        for record in self.store.list::<Enrollment>("enrollments")? {
            if !record.completed
                && record.agent_id.as_deref() == Some(agent)
                && record.connection.mode == ConnectionMode::ControllerToAgent
            {
                self.acknowledge_enrollment(&record.request.request_id, agent)?;
                self.dialer
                    .cancel(&format!("enroll:{}", record.request.request_id));
            }
        }
        Ok(())
    }
    pub(crate) fn enrollment_status(&self, id: &str) -> Result<Option<Value>> {
        Ok(self.store.get::<Enrollment>("enrollments", id)?.map(|record| {
            let status = if record.completed { "completed" } else if record.expires_at <= pier_protocol::now() { "expired" } else if record.agent_id.is_some() { "issued" } else { "authorized" };
            json!({"id":id,"state":status,"agent_id":record.agent_id,"expires_at":record.expires_at,"proxy_configured":record.proxy.is_some(),"last_error":self.dialer.error(&format!("enroll:{id}"))})
        }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::config;
    fn request() -> InitRequest {
        InitRequest {
            connection_mode: ConnectionMode::default(),
            listen: None,
            request_id: pier_protocol::new_token(),
            name: "server".into(),
            public_url: "https://pier.example.test".into(),
            info: pier_protocol::AgentInfo {
                architecture: pier_pkg::Architecture::Amd64,
                hostname: "server".into(),
                os_release: "fixture".into(),
            },
        }
    }
    #[tokio::test]
    async fn redemption_survives_restart_is_idempotent_and_ack_erases_secrets() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let state = Controller::open(config.clone()).unwrap();
        crate::auth::tests::initialize(&crate::api::router(state.clone())).await;
        let request = request();
        let pairing = state.approve_enrollment(request.clone()).unwrap();
        assert_eq!(
            state.approve_enrollment(request.clone()).unwrap().secret,
            pairing.secret
        );
        let key = state.enrollment_key(&pairing.grant_id).unwrap();
        let credentials = state
            .issue_credentials(&pairing.grant_id, request.clone(), &key)
            .unwrap();
        drop(state);
        let state = Controller::open(config).unwrap();
        let retry = state
            .issue_credentials(&pairing.grant_id, request.clone(), &key)
            .unwrap();
        assert_eq!(credentials.agent_id, retry.agent_id);
        assert_eq!(credentials.token, retry.token);
        let mut changed = request.clone();
        changed.name = "another".into();
        assert!(
            state
                .issue_credentials(&pairing.grant_id, changed, &key)
                .is_err()
        );
        assert!(
            state
                .acknowledge_enrollment(&request.request_id, "another")
                .is_err()
        );
        state
            .acknowledge_enrollment(&request.request_id, &credentials.agent_id)
            .unwrap();
        state
            .acknowledge_enrollment(&request.request_id, &credentials.agent_id)
            .unwrap();
        assert!(state.enrollment_key(&pairing.grant_id).is_err());
        let record: Enrollment = state
            .store
            .get("enrollments", &request.request_id)
            .unwrap()
            .unwrap();
        assert!(record.pairing.is_none() && record.issued.is_none() && record.completed);
        assert_eq!(state.store.list::<AgentRecord>("agents").unwrap().len(), 1);
        let status = state
            .enrollment_status(&request.request_id)
            .unwrap()
            .unwrap()
            .to_string();
        assert!(!status.contains(&credentials.token) && !status.contains(&pairing.secret));
    }
    #[tokio::test]
    async fn pending_proxy_and_issued_identity_survive_restart() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let state = Controller::open(config.clone()).unwrap();
        crate::auth::tests::initialize(&crate::api::router(state.clone())).await;
        let mut request = request();
        request.connection_mode = ConnectionMode::ControllerToAgent;
        request.listen = Some("0.0.0.0:7444".parse().unwrap());
        let proxy = Proxy::try_from("socks5://user:secret@proxy.test:1080".into()).unwrap();
        let pairing = state
            .approve_connection(
                request.clone(),
                Some("agent.test:7444".into()),
                Patch::Set(proxy.clone()),
            )
            .unwrap();
        let key = state.enrollment_key(&pairing.grant_id).unwrap();
        let credentials = state
            .issue_credentials(&pairing.grant_id, request.clone(), &key)
            .unwrap();
        drop(state);
        let state = Controller::open(config).unwrap();
        let targets = state.passive_enrollments().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, request.request_id);
        assert_eq!(targets[0].1.proxy, Some(proxy));
        let retry = state
            .approve_connection(
                request.clone(),
                Some("agent.test:7444".into()),
                Patch::Clear,
            )
            .unwrap();
        assert_eq!(pairing.encode().unwrap(), retry.encode().unwrap());
        let delivered = state
            .issue_credentials(&pairing.grant_id, request, &key)
            .unwrap();
        assert_eq!(delivered.token, credentials.token);
        let agent: AgentRecord = state
            .store
            .get("agents", &credentials.agent_id)
            .unwrap()
            .unwrap();
        assert!(agent.proxy.is_none());
        assert!(state.passive_enrollments().unwrap()[0].1.proxy.is_none());
    }
    #[tokio::test]
    async fn expired_authorization_cannot_issue_credentials() {
        let root = tempfile::tempdir().unwrap();
        let state = Controller::open(config(root.path())).unwrap();
        crate::auth::tests::initialize(&crate::api::router(state.clone())).await;
        let request = request();
        let pairing = state.approve_enrollment(request.clone()).unwrap();
        let key = state.enrollment_key(&pairing.grant_id).unwrap();
        let mut record: Enrollment = state
            .store
            .get("enrollments", &request.request_id)
            .unwrap()
            .unwrap();
        record.expires_at = 0;
        record.pairing.as_mut().unwrap().expires_at = 0;
        state
            .store
            .put("enrollments", &request.request_id, &record)
            .unwrap();
        assert!(
            state
                .issue_credentials(&pairing.grant_id, request.clone(), &key)
                .is_err()
        );
        state.expire_enrollments().unwrap();
        let new = state.approve_enrollment(request).unwrap();
        assert_ne!(new.secret, pairing.secret);
        assert!(
            state
                .store
                .list::<AgentRecord>("agents")
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn authorization_requires_session_and_same_origin_and_is_not_cached() {
        use crate::auth::tests::{initialize, send};
        use axum::{body::to_bytes, http::StatusCode};
        let root = tempfile::tempdir().unwrap();
        let state = Controller::open(config(root.path())).unwrap();
        let router = crate::api::router(state);
        let (cookie, session) = initialize(&router).await;
        for (credential, origin, expected) in [
            (
                "wrong",
                "https://pier.example.test",
                StatusCode::UNAUTHORIZED,
            ),
            (
                cookie.as_str(),
                "https://other.example.test",
                StatusCode::FORBIDDEN,
            ),
            (cookie.as_str(), "https://pier.example.test", StatusCode::OK),
        ] {
            let response = send(
                &router,
                "POST",
                "/v1/enrollments",
                credential,
                session["csrf_token"].as_str().unwrap(),
                origin,
                Some(serde_json::to_value(request()).unwrap()),
            )
            .await;
            assert_eq!(response.status(), expected);
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
        let page = send(&router, "GET", "/agent/init", "", "", "", None).await;
        assert_eq!(page.status(), StatusCode::OK);
        assert!(page.headers().contains_key("content-security-policy"));
        assert!(
            String::from_utf8(to_bytes(page.into_body(), 65536).await.unwrap().to_vec())
                .unwrap()
                .contains("id=\"root\"")
        );
    }
}
