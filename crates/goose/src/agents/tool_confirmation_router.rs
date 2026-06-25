use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, Mutex};
use tracing::warn;

use crate::permission::PermissionConfirmation;

const DELEGATED_CONFIRMATION_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
pub struct ToolConfirmationRouter {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<PermissionConfirmation>>>>,
}

#[derive(Clone)]
struct DelegatedConfirmationRoute {
    parent_session_id: String,
    subagent_id: String,
    child_request_id: String,
    child_router: ToolConfirmationRouter,
    created_at: Instant,
}

impl ToolConfirmationRouter {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn register(&self, request_id: String) -> oneshot::Receiver<PermissionConfirmation> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().await;
        pending.retain(|_, sender| !sender.is_closed());
        pending.insert(request_id, tx);
        rx
    }

    pub async fn deliver(&self, request_id: String, confirmation: PermissionConfirmation) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(&request_id) {
            if tx.send(confirmation).is_err() {
                warn!(
                    request_id = %request_id,
                    "Confirmation receiver was dropped (task cancelled)"
                );
                false
            } else {
                true
            }
        } else {
            warn!(
                request_id = %request_id,
                "No task waiting for confirmation"
            );
            false
        }
    }
}

static DELEGATED_TOOL_CONFIRMATIONS: OnceLock<Mutex<HashMap<String, DelegatedConfirmationRoute>>> =
    OnceLock::new();

fn delegated_routes() -> &'static Mutex<HashMap<String, DelegatedConfirmationRoute>> {
    DELEGATED_TOOL_CONFIRMATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prune_expired_delegated_routes(routes: &mut HashMap<String, DelegatedConfirmationRoute>) {
    let now = Instant::now();
    routes.retain(|_, route| now.duration_since(route.created_at) < DELEGATED_CONFIRMATION_TTL);
}

pub fn delegated_tool_confirmation_id(subagent_id: &str, child_request_id: &str) -> String {
    format!("subagent:{subagent_id}:{child_request_id}")
}

pub async fn register_delegated_tool_confirmation(
    parent_session_id: String,
    subagent_id: String,
    delegated_request_id: String,
    child_request_id: String,
    child_router: ToolConfirmationRouter,
) {
    let mut routes = delegated_routes().lock().await;
    prune_expired_delegated_routes(&mut routes);
    routes.insert(
        delegated_request_id,
        DelegatedConfirmationRoute {
            parent_session_id,
            subagent_id,
            child_request_id,
            child_router,
            created_at: Instant::now(),
        },
    );
}

pub async fn deliver_delegated_tool_confirmation(
    parent_session_id: &str,
    delegated_request_id: &str,
    confirmation: PermissionConfirmation,
) -> bool {
    let route = {
        let mut routes = delegated_routes().lock().await;
        prune_expired_delegated_routes(&mut routes);
        match routes.get(delegated_request_id) {
            Some(route) if route.parent_session_id != parent_session_id => {
                warn!(
                    request_id = %delegated_request_id,
                    session_id = %parent_session_id,
                    "Delegated tool confirmation was not scoped to this parent session"
                );
                return false;
            }
            Some(_) => routes.remove(delegated_request_id),
            None => None,
        }
    };
    if let Some(route) = route {
        route
            .child_router
            .deliver(route.child_request_id, confirmation)
            .await
    } else {
        false
    }
}

#[cfg(test)]
pub(crate) async fn unregister_delegated_tool_confirmation(delegated_request_id: &str) {
    let mut routes = delegated_routes().lock().await;
    prune_expired_delegated_routes(&mut routes);
    routes.remove(delegated_request_id);
}

pub fn schedule_unregister_delegated_tool_confirmations_for_subagent(subagent_id: String) {
    tokio::spawn(async move {
        unregister_delegated_tool_confirmations_for_subagent(&subagent_id).await;
    });
}

pub async fn unregister_delegated_tool_confirmations_for_subagent(subagent_id: &str) {
    let mut routes = delegated_routes().lock().await;
    prune_expired_delegated_routes(&mut routes);
    routes.retain(|_, route| route.subagent_id != subagent_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::permission_confirmation::PrincipalType;
    use crate::permission::Permission;

    fn test_confirmation() -> PermissionConfirmation {
        PermissionConfirmation {
            principal_type: PrincipalType::Tool,
            permission: Permission::AllowOnce,
        }
    }

    #[tokio::test]
    async fn test_register_then_deliver() {
        let router = ToolConfirmationRouter::new();
        let rx = router.register("req_1".to_string()).await;
        assert!(
            router
                .deliver("req_1".to_string(), test_confirmation())
                .await
        );
        let confirmation = rx.await.unwrap();
        assert_eq!(confirmation.permission, Permission::AllowOnce);
    }

    #[tokio::test]
    async fn test_deliver_unknown_request() {
        let router = ToolConfirmationRouter::new();
        assert!(
            !router
                .deliver("unknown".to_string(), test_confirmation())
                .await
        );
    }

    #[tokio::test]
    async fn test_cancelled_receiver() {
        let router = ToolConfirmationRouter::new();
        let rx = router.register("req_1".to_string()).await;
        drop(rx); // simulate task cancellation
        assert!(
            !router
                .deliver("req_1".to_string(), test_confirmation())
                .await
        );
    }

    #[tokio::test]
    async fn test_stale_entries_pruned_on_register() {
        let router = ToolConfirmationRouter::new();
        let rx = router.register("req_1".to_string()).await;
        drop(rx); // simulate task cancellation — entry is now stale

        assert_eq!(router.pending.lock().await.len(), 1);

        let _rx2 = router.register("req_2".to_string()).await;
        assert_eq!(router.pending.lock().await.len(), 1); // only req_2 remains
        assert!(router.pending.lock().await.contains_key("req_2"));
    }

    #[tokio::test]
    async fn test_concurrent_requests_out_of_order() {
        use std::sync::Arc;

        let router = Arc::new(ToolConfirmationRouter::new());

        // Register two requests
        let rx1 = router.register("req_1".to_string()).await;
        let rx2 = router.register("req_2".to_string()).await;

        // Deliver in reverse order
        assert!(
            router
                .deliver(
                    "req_2".to_string(),
                    PermissionConfirmation {
                        principal_type: PrincipalType::Tool,
                        permission: Permission::DenyOnce,
                    }
                )
                .await
        );
        assert_eq!(router.pending.lock().await.len(), 1);
        assert!(
            router
                .deliver("req_1".to_string(), test_confirmation())
                .await
        );
        assert_eq!(router.pending.lock().await.len(), 0);

        let c1 = rx1.await.unwrap();
        assert_eq!(c1.permission, Permission::AllowOnce);
        let c2 = rx2.await.unwrap();
        assert_eq!(c2.permission, Permission::DenyOnce);
    }

    #[tokio::test]
    async fn test_delegated_route_delivers_to_child_router() {
        let child_router = ToolConfirmationRouter::new();
        let child_rx = child_router.register("child_req".to_string()).await;
        let delegated_id = delegated_tool_confirmation_id("sub_1", "child_req");

        register_delegated_tool_confirmation(
            "parent_session".to_string(),
            "sub_1".to_string(),
            delegated_id.clone(),
            "child_req".to_string(),
            child_router,
        )
        .await;

        assert!(
            deliver_delegated_tool_confirmation(
                "parent_session",
                &delegated_id,
                test_confirmation()
            )
            .await
        );
        let confirmation = child_rx.await.unwrap();
        assert_eq!(confirmation.permission, Permission::AllowOnce);
        assert!(
            !deliver_delegated_tool_confirmation(
                "parent_session",
                &delegated_id,
                test_confirmation()
            )
            .await
        );
    }

    #[tokio::test]
    async fn test_delegated_route_rejects_wrong_parent_session() {
        let child_router = ToolConfirmationRouter::new();
        let child_rx = child_router.register("child_req_scoped".to_string()).await;
        let delegated_id = delegated_tool_confirmation_id("sub_2", "child_req_scoped");

        register_delegated_tool_confirmation(
            "parent_a".to_string(),
            "sub_2".to_string(),
            delegated_id.clone(),
            "child_req_scoped".to_string(),
            child_router,
        )
        .await;

        assert!(
            !deliver_delegated_tool_confirmation("parent_b", &delegated_id, test_confirmation())
                .await
        );
        assert!(
            deliver_delegated_tool_confirmation("parent_a", &delegated_id, test_confirmation())
                .await
        );
        let confirmation = child_rx.await.unwrap();
        assert_eq!(confirmation.permission, Permission::AllowOnce);
    }

    #[tokio::test]
    async fn test_delegated_route_unregister_removes_pending_route() {
        let child_router = ToolConfirmationRouter::new();
        let _child_rx = child_router.register("child_req_cleanup".to_string()).await;
        let delegated_id = delegated_tool_confirmation_id("sub_cleanup", "child_req_cleanup");

        register_delegated_tool_confirmation(
            "parent_cleanup".to_string(),
            "sub_cleanup".to_string(),
            delegated_id.clone(),
            "child_req_cleanup".to_string(),
            child_router,
        )
        .await;

        unregister_delegated_tool_confirmation(&delegated_id).await;
        assert!(
            !deliver_delegated_tool_confirmation(
                "parent_cleanup",
                &delegated_id,
                test_confirmation()
            )
            .await
        );
    }

    #[tokio::test]
    async fn test_expired_delegated_route_is_pruned_on_delivery() {
        let child_router = ToolConfirmationRouter::new();
        let mut child_rx = child_router.register("child_req_expired".to_string()).await;
        let delegated_id = delegated_tool_confirmation_id("sub_expired", "child_req_expired");

        {
            let mut routes = delegated_routes().lock().await;
            routes.insert(
                delegated_id.clone(),
                DelegatedConfirmationRoute {
                    parent_session_id: "parent_expired".to_string(),
                    subagent_id: "sub_expired".to_string(),
                    child_request_id: "child_req_expired".to_string(),
                    child_router,
                    created_at: Instant::now()
                        - DELEGATED_CONFIRMATION_TTL
                        - Duration::from_secs(1),
                },
            );
        }

        assert!(
            !deliver_delegated_tool_confirmation(
                "parent_expired",
                &delegated_id,
                test_confirmation()
            )
            .await
        );
        assert!(!delegated_routes().lock().await.contains_key(&delegated_id));
        assert!(child_rx.try_recv().is_err());
    }
}
