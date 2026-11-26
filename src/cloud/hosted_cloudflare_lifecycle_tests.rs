use super::*;
use crate::hosted_cloudflare::lifecycle::ConnectionHistory;
use crate::hosted_cloudflare::lifecycle::{ProviderLifecycleOutcome, SavedCloudflareInstallation};

fn saved_resources() -> CloudflareResources {
    CloudflareResources {
        tunnel: ResourceState::Known(ID.to_owned()),
        dns_record: ResourceState::Known(DNS_ID.to_owned()),
        selected_routes: None,
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
