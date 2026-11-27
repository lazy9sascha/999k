use super::*;
use crate::hosted_cloudflare::lifecycle::ConnectionHistory;
use crate::hosted_cloudflare::lifecycle::{ProviderLifecycleOutcome, SavedCloudflareInstallation};

fn saved_resources() -> CloudflareResources {
    CloudflareResources {
        tunnel: ResourceState::Known(ID.to_owned()),
        dns_record: ResourceState::Known(DNS_ID.to_owned()),
        selected_routes: None,
        dot_access_app: None,
    }
}

fn saved(resources: &CloudflareResources) -> SavedCloudflareInstallation<'_> {
    SavedCloudflareInstallation {
        installation_id: installation(),
        account_id: ACCOUNT,
        zone_id: ZONE,
        hostname: HOST,
        resources,
    }
}

fn tunnel_exact(method: &'static str, status: &str) -> Step {
    let mut result = tunnel();
    result["status"] = json!(status);
    Step::result(method, format!("{}/{ID}", tunnels_path()), result)
}

fn dns_exact(method: &'static str) -> Step {
    Step::result(method, format!("{}/{DNS_ID}", dns_path()), dns(&marker()))
}

fn connector_list(method: &'static str, result: serde_json::Value) -> Step {
    Step::result(
        method,
        format!("{}/{ID}/connections", tunnels_path()),
        result,
    )
}

#[tokio::test]
async fn scheduled_reset_revokes_owned_connected_tunnels_and_resumes_after_restart() {
    use crate::enrollment_store::LifecyclePhase;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    let (home, operator) = crate::tests::fixture();
    let first_reset = 1_800_000_000;
    let proof = URL_SAFE_NO_PAD.encode([7; 32]);
    let issue = |at| {
        let id = operator
            .record(|registry| {
                let id = registry
                    .begin(Uuid::new_v4(), &proof, "A1B2C3D4", at - 3)?
                    .id;
                registry.approve_for_test(id, &proof, at - 1)?;
                Ok(id)
            })
            .unwrap();
        operator.persist_resources(id, &saved_resources()).unwrap();
        operator
            .record(|registry| {
                registry.mark_ready(id, at, false)?;
                registry.retain_connected(id, LifecyclePhase::Ready)
            })
            .unwrap();
        id
    };
    let old = issue(first_reset - 30);
    let boundary = issue(first_reset);
    let hostname = operator.hostname(old);
    let replace = |value: &str| {
        value
            .replace(ACCOUNT, &operator.account_id)
            .replace(ZONE, &operator.zone_id)
            .replace(INSTALLATION, &old.to_string())
            .replace(HOST, &hostname)
    };
    let (provider, server) = fixture(
        vec![
            tunnel_exact("GET", "healthy"),
            dns_exact("GET"),
            dns_exact("DELETE"),
            dns_exact("GET").status(503),
            tunnel_exact("GET", "healthy"),
            dns_exact("GET").status(404),
            connector_list("GET", json!([{}])),
            connector_list("DELETE", json!(null)),
            tunnel_exact("DELETE", "down"),
            tunnel_exact("GET", "down").status(404),
        ]
        .into_iter()
        .map(|step| Step {
            path: replace(&step.path),
            result: serde_json::from_str(&replace(&step.result.to_string())).unwrap(),
            ..step
        })
        .collect(),
    )
    .await;
    let settings_path = home.path().join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&crate::read_private(&settings_path).unwrap()).unwrap();
    settings["first_hosted_reset_at"] = json!("2027-01-15T08:00:00Z");
    radio999k::private_file::replace_private(
        &settings_path,
        &serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    radio999k::private_file::replace_private(&home.path().join("cloudflare.key"), API.as_bytes())
        .unwrap();
    let open = || {
        let mut operator = crate::Operator::from_home(home.path()).unwrap();
        operator.cloudflare.api = provider.api.clone();
        operator
    };
    let phase = |operator: &crate::Operator, id| {
        operator
            .record(|registry| registry.lifecycle(id))
            .unwrap()
            .phase
    };
    drop(operator);
    let operator = open();
    operator.cleanup_once(first_reset - 1).await;
    assert_eq!(phase(&operator, old), LifecyclePhase::Ready);
    operator.cleanup_once(first_reset).await;
    assert_eq!(phase(&operator, old), LifecyclePhase::Revoking);
    assert_eq!(phase(&operator, boundary), LifecyclePhase::Ready);

    drop(operator);
    let operator = open();
    operator.cleanup_once(first_reset + 60).await;
    assert_eq!(phase(&operator, old), LifecyclePhase::Revoked);
    operator
        .cleanup_once(first_reset + 5 * 24 * 60 * 60 - 1)
        .await;
    assert_eq!(phase(&operator, boundary), LifecyclePhase::Ready);
    server.await.unwrap();
}

#[tokio::test]
async fn tracked_dot_access_removal_resumes_and_rejects_shared_apps() {
    let mut shared = dot_access_step("GET");
    shared.result["destinations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"uri":"another.example.test/dot/mcp"}));
    let (provider, server) = fixture(vec![
        tunnel_exact("GET", "healthy").status(404),
        dns_exact("GET").status(404),
        shared,
        tunnel_exact("GET", "healthy").status(404),
        dns_exact("GET").status(404),
        dot_access_step("GET"),
        dot_access_step("DELETE"),
        dot_access_step("GET"),
        tunnel_exact("GET", "healthy").status(404),
        dns_exact("GET").status(404),
        dot_access_step("GET").status(404),
    ])
    .await;
    let resources = CloudflareResources {
        dot_access_app: Some(DOT_ACCESS_ID.to_owned()),
        ..saved_resources()
    };
    assert!(matches!(
        provider.revoke(&saved(&resources)).await,
        Err(ProvisionError::Conflict)
    ));
    assert_eq!(
        provider.revoke(&saved(&resources)).await.unwrap(),
        ProviderLifecycleOutcome::Pending
    );
    assert_eq!(
        provider.revoke(&saved(&resources)).await.unwrap(),
        ProviderLifecycleOutcome::Complete
    );
    server.await.unwrap();
}

#[tokio::test]
async fn exact_history_preserves_disconnected_tunnels_and_unknown_states() {
    let mut conflicting = tunnel_exact("GET", "inactive");
    conflicting.result["conns_active_at"] = json!("2026-11-21T01:02:03Z");
    let (provider, server) = fixture(vec![
        tunnel_exact("GET", "inactive"),
        tunnel_exact("GET", "down"),
        tunnel_exact("GET", "unexpected"),
        conflicting,
        tunnel_exact("GET", "inactive").status(503),
    ])
    .await;
    let resources = saved_resources();
    for expected in [
        ConnectionHistory::NeverConnected,
        ConnectionHistory::PreviouslyConnected,
        ConnectionHistory::Unknown,
        ConnectionHistory::PreviouslyConnected,
    ] {
        let history = provider
            .tunnel_connection_history(&saved(&resources))
            .await
            .unwrap();
        assert!(history == expected);
    }
    assert!(matches!(
        provider.tunnel_connection_history(&saved(&resources)).await,
        Err(ProvisionError::Http(503))
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn revoke_rechecks_dns_and_resumes_after_delete_read_failure() {
    let (provider, server) = fixture(vec![
        tunnel_exact("GET", "healthy"),
        dns_exact("GET"),
        dns_exact("DELETE"),
        dns_exact("GET"),
        tunnel_exact("GET", "healthy"),
        dns_exact("GET").status(404),
        connector_list("GET", json!([{}])),
        connector_list("DELETE", json!(null)),
        tunnel_exact("DELETE", "down"),
        tunnel_exact("GET", "down").status(503),
        tunnel_exact("GET", "down").status(404),
        dns_exact("GET").status(404),
    ])
    .await;
    let resources = saved_resources();
    assert_eq!(
        provider.revoke(&saved(&resources)).await.unwrap(),
        ProviderLifecycleOutcome::Pending
    );
    assert!(matches!(
        provider.revoke(&saved(&resources)).await,
        Err(ProvisionError::Http(503))
    ));
    assert_eq!(
        provider.revoke(&saved(&resources)).await.unwrap(),
        ProviderLifecycleOutcome::Complete
    );
    server.await.unwrap();
}

#[tokio::test]
async fn lifecycle_refuses_to_delete_reassigned_resources() {
    let mut wrong_tunnel = tunnel_exact("GET", "inactive");
    wrong_tunnel.result["name"] = json!("some-other-installation");
    let mut wrong_dns = dns_exact("GET");
    wrong_dns.result["comment"] = json!("some-other-installation");
    let (provider, server) = fixture(vec![wrong_tunnel, wrong_dns]).await;
    let resources = saved_resources();
    assert!(matches!(
        provider.revoke(&saved(&resources)).await,
        Err(ProvisionError::Conflict)
    ));
    assert!(matches!(
        provider.expire_unopened(&saved(&resources)).await,
        Err(ProvisionError::Conflict)
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn expiry_rechecks_history_after_dns_and_never_disconnects() {
    let (provider, server) = fixture(vec![
        tunnel_exact("GET", "inactive"),
        dns_exact("GET"),
        tunnel_exact("GET", "healthy"),
        dns_exact("GET"),
        tunnel_exact("GET", "unknown"),
        dns_exact("GET"),
        tunnel_exact("GET", "inactive").status(503),
        dns_exact("GET"),
        tunnel_exact("GET", "inactive"),
        tunnel_exact("DELETE", "inactive").status(409),
    ])
    .await;
    let resources = saved_resources();
    let registration = saved(&resources);
    assert!(
        provider
            .tunnel_connection_history(&registration)
            .await
            .unwrap()
            == ConnectionHistory::NeverConnected
    );
    assert_eq!(
        provider.expire_unopened(&registration).await.unwrap(),
        ProviderLifecycleOutcome::Retained
    );
    assert!(matches!(
        provider.expire_unopened(&registration).await,
        Err(ProvisionError::Uncertain)
    ));
    assert!(matches!(
        provider.expire_unopened(&registration).await,
        Err(ProvisionError::Http(503))
    ));
    assert!(matches!(
        provider.expire_unopened(&registration).await,
        Err(ProvisionError::Http(409))
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn expiry_resumes_dns_cleanup_after_confirmed_tunnel_deletion() {
    let mut deleted = tunnel_exact("GET", "inactive");
    deleted.result["deleted_at"] = json!("2026-11-21T01:02:03Z");
    let (provider, server) = fixture(vec![
        dns_exact("GET"),
        tunnel_exact("GET", "inactive"),
        tunnel_exact("DELETE", "inactive"),
        deleted,
        dns_exact("GET"),
        dns_exact("DELETE").status(503),
        dns_exact("GET"),
        tunnel_exact("GET", "inactive").status(404),
        dns_exact("GET"),
        dns_exact("DELETE"),
        dns_exact("GET").status(404),
    ])
    .await;
    let resources = saved_resources();
    assert!(matches!(
        provider.expire_unopened(&saved(&resources)).await,
        Err(ProvisionError::Http(503))
    ));
    assert_eq!(
        provider.expire_unopened(&saved(&resources)).await.unwrap(),
        ProviderLifecycleOutcome::Complete
    );
    server.await.unwrap();
}
