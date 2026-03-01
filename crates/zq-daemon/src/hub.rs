//! Central state manager for the zq daemon.
//!
//! `Hub` is `Clone` and thread-safe -- all mutable state lives behind
//! `Arc<Mutex<...>>`. It is the single source of truth for flows, apps,
//! routing rules, and proxy connectivity status.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, Notify};
use tracing::{debug, info};

use zq_proto::{
    AppInfo, DaemonToTuiMessage, FlowInfo, Proto, ProxyStatus, RouteAction, TuiCommand,
};

// ---------------------------------------------------------------------------
// Inner (guarded by Mutex)
// ---------------------------------------------------------------------------

struct HubInner {
    flows: HashMap<u64, FlowInfo>,
    apps: HashMap<String, AppInfo>,
    routing_rules: HashMap<String, RouteAction>,
    global_routing: RouteAction,
    proxy_status: ProxyStatus,
    tui_subscribers: Vec<mpsc::UnboundedSender<DaemonToTuiMessage>>,
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
    shutdown: Arc<Notify>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HubInner {
                flows: HashMap::new(),
                apps: HashMap::new(),
                routing_rules: HashMap::new(),
                global_routing: RouteAction::default(),
                proxy_status: ProxyStatus::default(),
                tui_subscribers: Vec::new(),
            })),
            shutdown: Arc::new(Notify::new()),
        }
    }

    // ---------------------------------------------------------------------
    // Flow lifecycle (called from proxy.rs)
    // ---------------------------------------------------------------------

    /// Register a new flow and determine its routing action.
    ///
    /// PID resolution is done by the caller (proxy) before calling this,
    /// so no blocking work happens under the mutex.
    pub fn handle_flow_start(
        &self,
        flow_id: u64,
        pid: u32,
        process_name: &str,
        bundle_id: &str,
        local_addr: &str,
        remote_addr: &str,
        proto: Proto,
    ) -> RouteAction {
        let mut inner = self.inner.lock().unwrap();

        let routing = inner.get_route_action_inner(bundle_id);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let flow = FlowInfo {
            flow_id,
            pid,
            process_name: process_name.to_string(),
            bundle_id: bundle_id.to_string(),
            local_addr: local_addr.to_string(),
            remote_addr: remote_addr.to_string(),
            proto,
            bytes_in: 0,
            bytes_out: 0,
            routing,
            started_at: now,
        };

        inner.flows.insert(flow_id, flow.clone());

        // Update (or create) the app entry.
        let app = inner
            .apps
            .entry(bundle_id.to_string())
            .or_insert_with(|| AppInfo {
                bundle_id: bundle_id.to_string(),
                name: process_name.to_string(),
                pids: Vec::new(),
                flow_count: 0,
                bytes_in: 0,
                bytes_out: 0,
                routing,
            });
        app.flow_count += 1;
        if !app.pids.contains(&pid) {
            app.pids.push(pid);
        }
        let app_snapshot = app.clone();

        // Notify TUI.
        inner.notify_tui_inner(DaemonToTuiMessage::FlowUpdate {
            flow: flow.clone(),
        });
        inner.notify_tui_inner(DaemonToTuiMessage::AppUpdate {
            app: app_snapshot,
        });

        debug!(flow_id, %bundle_id, ?routing, "flow started");

        routing
    }

    /// Update byte counters for a live flow and broadcast to TUI if changed.
    pub fn update_flow_bytes(&self, flow_id: u64, bytes_in: u64, bytes_out: u64) {
        let mut inner = self.inner.lock().unwrap();

        if let Some(flow) = inner.flows.get_mut(&flow_id) {
            if flow.bytes_in != bytes_in || flow.bytes_out != bytes_out {
                flow.bytes_in = bytes_in;
                flow.bytes_out = bytes_out;
                let snapshot = flow.clone();
                inner.notify_tui_inner(DaemonToTuiMessage::FlowUpdate {
                    flow: snapshot,
                });
            }
        }
    }

    /// Record completed byte counts and remove a flow.
    pub fn handle_flow_end(&self, flow_id: u64, bytes_in: u64, bytes_out: u64) {
        let mut inner = self.inner.lock().unwrap();

        if let Some(mut flow) = inner.flows.remove(&flow_id) {
            flow.bytes_in = bytes_in;
            flow.bytes_out = bytes_out;

            // Update app-level counters and decrement flow count.
            if let Some(app) = inner.apps.get_mut(&flow.bundle_id) {
                app.bytes_in += bytes_in;
                app.bytes_out += bytes_out;
                app.flow_count = app.flow_count.saturating_sub(1);
                let app_snapshot = app.clone();
                inner.notify_tui_inner(DaemonToTuiMessage::AppUpdate {
                    app: app_snapshot,
                });
            }
            inner.notify_tui_inner(DaemonToTuiMessage::FlowRemoved { flow_id });
            debug!(flow_id, bytes_in, bytes_out, "flow ended");
        } else {
            debug!(flow_id, "FlowEnded for unknown flow, ignoring");
        }
    }

    // ---------------------------------------------------------------------
    // TUI command handling
    // ---------------------------------------------------------------------

    /// Process a command from a TUI client. Returns a response message
    /// if the command warrants one (e.g., GetState).
    pub fn handle_tui_command(&self, cmd: TuiCommand) -> Option<DaemonToTuiMessage> {
        match cmd {
            TuiCommand::Subscribe => {
                // Subscription is handled separately via subscribe_tui().
                // Return a full state snapshot so the client is immediately
                // up to date.
                Some(self.get_full_state())
            }

            TuiCommand::GetState => Some(self.get_full_state()),

            TuiCommand::SetAppRouting { bundle_id, action } => {
                let mut inner = self.inner.lock().unwrap();
                inner.routing_rules.insert(bundle_id.clone(), action);
                info!(%bundle_id, ?action, "app routing rule updated");

                // Update the cached routing on the app entry, if it exists.
                if let Some(app) = inner.apps.get_mut(&bundle_id) {
                    app.routing = action;
                    let app_snapshot = app.clone();
                    inner.notify_tui_inner(DaemonToTuiMessage::AppUpdate {
                        app: app_snapshot,
                    });
                }

                // Update routing on all active flows for this app.
                for flow in inner.flows.values_mut() {
                    if flow.bundle_id == bundle_id {
                        flow.routing = action;
                    }
                }

                None
            }

            TuiCommand::SetGlobalRouting { action } => {
                let mut inner = self.inner.lock().unwrap();
                inner.global_routing = action;
                info!(?action, "global routing updated");

                let overridden: std::collections::HashSet<String> =
                    inner.routing_rules.keys().cloned().collect();

                for app in inner.apps.values_mut() {
                    if !overridden.contains(&app.bundle_id) {
                        app.routing = action;
                    }
                }
                for flow in inner.flows.values_mut() {
                    if !overridden.contains(&flow.bundle_id) {
                        flow.routing = action;
                    }
                }

                None
            }

            TuiCommand::Shutdown => {
                info!("shutdown command received from TUI");
                self.signal_shutdown();
                None
            }
        }
    }

    // ---------------------------------------------------------------------
    // Routing
    // ---------------------------------------------------------------------

    /// Determine the routing action for a given bundle ID.
    /// Checks per-app rules first, then falls back to the global default.
    #[cfg(test)]
    pub fn get_route_action(&self, bundle_id: &str) -> RouteAction {
        let inner = self.inner.lock().unwrap();
        inner.get_route_action_inner(bundle_id)
    }

    // ---------------------------------------------------------------------
    // Proxy status
    // ---------------------------------------------------------------------

    /// Update the proxy reachability status and notify TUI clients
    /// if it changed.
    pub fn set_proxy_status(&self, status: ProxyStatus) {
        let mut inner = self.inner.lock().unwrap();
        if inner.proxy_status != status {
            inner.proxy_status = status;
            info!(?status, "proxy status changed");
            inner.notify_tui_inner(DaemonToTuiMessage::ProxyStatusUpdate { status });
        }
    }

    // ---------------------------------------------------------------------
    // Shutdown signaling
    // ---------------------------------------------------------------------

    /// Signal all waiters that a shutdown has been requested.
    pub fn signal_shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Wait until a shutdown is signaled.
    pub async fn wait_shutdown(&self) {
        self.shutdown.notified().await;
    }

    // ---------------------------------------------------------------------
    // TUI subscription
    // ---------------------------------------------------------------------

    /// Register a new TUI subscriber and return its receiving end.
    pub fn subscribe_tui(&self) -> mpsc::UnboundedReceiver<DaemonToTuiMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().unwrap();
        inner.tui_subscribers.push(tx);
        rx
    }

    /// Build a full state snapshot for a newly connected TUI client.
    pub fn get_full_state(&self) -> DaemonToTuiMessage {
        let inner = self.inner.lock().unwrap();
        DaemonToTuiMessage::FullState {
            apps: inner.apps.values().cloned().collect(),
            flows: inner.flows.values().cloned().collect(),
            proxy_status: inner.proxy_status,
        }
    }
}

// ---------------------------------------------------------------------------
// Private helpers on the inner state
// ---------------------------------------------------------------------------

impl HubInner {
    /// Routing lookup without re-locking.
    fn get_route_action_inner(&self, bundle_id: &str) -> RouteAction {
        self.routing_rules
            .get(bundle_id)
            .copied()
            .unwrap_or(self.global_routing)
    }

    /// Broadcast a message to all live TUI subscribers, pruning any whose
    /// channel has been closed.
    fn notify_tui_inner(&mut self, msg: DaemonToTuiMessage) {
        self.tui_subscribers.retain(|tx| tx.send(msg.clone()).is_ok());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hub_new_defaults() {
        let hub = Hub::new();
        assert_eq!(hub.get_route_action("com.example.app"), RouteAction::Passthrough);
    }

    #[test]
    fn test_set_proxy_status() {
        let hub = Hub::new();
        hub.set_proxy_status(ProxyStatus::Reachable);
        let state = hub.get_full_state();
        match state {
            DaemonToTuiMessage::FullState { proxy_status, .. } => {
                assert_eq!(proxy_status, ProxyStatus::Reachable);
            }
            _ => panic!("expected FullState"),
        }
    }

    #[test]
    fn test_routing_rules() {
        let hub = Hub::new();
        hub.handle_tui_command(TuiCommand::SetAppRouting {
            bundle_id: "com.example.app".to_string(),
            action: RouteAction::RouteToProxy,
        });
        assert_eq!(
            hub.get_route_action("com.example.app"),
            RouteAction::RouteToProxy
        );
        // Unset app still gets global default.
        assert_eq!(
            hub.get_route_action("com.other.app"),
            RouteAction::Passthrough
        );
    }

    #[test]
    fn test_global_routing() {
        let hub = Hub::new();
        hub.handle_tui_command(TuiCommand::SetGlobalRouting {
            action: RouteAction::RouteToProxy,
        });
        assert_eq!(
            hub.get_route_action("com.anything.app"),
            RouteAction::RouteToProxy
        );
    }

    #[test]
    fn test_subscribe_receives_updates() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_tui();

        hub.set_proxy_status(ProxyStatus::Reachable);

        let msg = rx.try_recv().expect("should have received a message");
        match msg {
            DaemonToTuiMessage::ProxyStatusUpdate { status } => {
                assert_eq!(status, ProxyStatus::Reachable);
            }
            _ => panic!("expected ProxyStatusUpdate"),
        }
    }

    #[test]
    fn test_flow_lifecycle() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_tui();

        // Start a flow.
        let action = hub.handle_flow_start(
            1,
            std::process::id(),
            "test_process",
            "com.test.app",
            "127.0.0.1:50000",
            "10.0.0.1:443",
            Proto::Tcp,
        );
        assert_eq!(action, RouteAction::Passthrough);

        // Drain the FlowUpdate and AppUpdate messages.
        let mut saw_flow_update = false;
        while let Ok(msg) = rx.try_recv() {
            if matches!(msg, DaemonToTuiMessage::FlowUpdate { .. }) {
                saw_flow_update = true;
            }
        }
        assert!(saw_flow_update);

        // End the flow.
        hub.handle_flow_end(1, 1024, 512);

        let mut saw_flow_removed = false;
        while let Ok(msg) = rx.try_recv() {
            if matches!(msg, DaemonToTuiMessage::FlowRemoved { flow_id: 1 }) {
                saw_flow_removed = true;
            }
        }
        assert!(saw_flow_removed);
    }

    #[test]
    fn test_update_flow_bytes() {
        let hub = Hub::new();
        let mut rx = hub.subscribe_tui();

        // Start a flow.
        hub.handle_flow_start(
            42,
            std::process::id(),
            "test",
            "com.test.app",
            "127.0.0.1:50000",
            "10.0.0.1:443",
            Proto::Tcp,
        );

        // Drain initial messages.
        while rx.try_recv().is_ok() {}

        // Update bytes.
        hub.update_flow_bytes(42, 100, 200);

        let msg = rx.try_recv().expect("should have received FlowUpdate");
        match msg {
            DaemonToTuiMessage::FlowUpdate { flow } => {
                assert_eq!(flow.flow_id, 42);
                assert_eq!(flow.bytes_in, 100);
                assert_eq!(flow.bytes_out, 200);
            }
            _ => panic!("expected FlowUpdate"),
        }

        // Same values should not re-broadcast.
        hub.update_flow_bytes(42, 100, 200);
        assert!(rx.try_recv().is_err(), "should not broadcast unchanged bytes");
    }

    #[tokio::test]
    async fn test_shutdown_signal() {
        let hub = Hub::new();
        let hub2 = hub.clone();

        let handle = tokio::spawn(async move {
            hub2.wait_shutdown().await;
            true
        });

        // Brief delay to ensure the waiter is registered.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        hub.signal_shutdown();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("timeout")
            .expect("join error");

        assert!(result);
    }
}
