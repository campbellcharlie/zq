use crate::client::DaemonClient;
use crate::event::{Event, EventReader};
use crate::ui;
use anyhow::Result;
use crossterm::event::KeyCode;
use tracing::{error, info, warn};
use zq_proto::config::Config;
use zq_proto::*;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Which panel has keyboard focus.
pub enum Focus {
    Apps,
    Flows,
}

/// Root application state.
pub struct App {
    pub focus: Focus,
    pub apps: Vec<AppInfo>,
    pub flows: Vec<FlowInfo>,
    pub proxy_status: ProxyStatus,
    pub selected_app: usize,
    pub selected_flow: usize,
    pub should_quit: bool,
    pub connected: bool,
    pub status_message: String,
}

impl App {
    pub fn new() -> Self {
        Self {
            focus: Focus::Apps,
            apps: Vec::new(),
            flows: Vec::new(),
            proxy_status: ProxyStatus::default(),
            selected_app: 0,
            selected_flow: 0,
            should_quit: false,
            connected: false,
            status_message: String::new(),
        }
    }

    /// Handle a keyboard event and optionally send commands to the daemon.
    pub async fn on_key(&mut self, key: KeyCode, client: &mut DaemonClient) {
        match key {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }

            // Switch focus between panels
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Apps => Focus::Flows,
                    Focus::Flows => Focus::Apps,
                };
            }

            // Navigation
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),

            // Toggle routing for selected app
            KeyCode::Char('r') => {
                if matches!(self.focus, Focus::Apps) {
                    self.toggle_app_routing(client).await;
                }
            }

            // Toggle global routing
            KeyCode::Char('g') => {
                self.toggle_global_routing(client).await;
            }

            _ => {}
        }
    }

    /// Apply a message received from the daemon to the local state.
    pub fn apply_daemon_message(&mut self, msg: DaemonToTuiMessage) {
        match msg {
            DaemonToTuiMessage::FullState {
                apps,
                flows,
                proxy_status,
            } => {
                self.apps = apps;
                self.flows = flows;
                self.proxy_status = proxy_status;
                self.clamp_selections();
                self.status_message = "Connected".into();
                info!(
                    "full state: {} apps, {} flows",
                    self.apps.len(),
                    self.flows.len()
                );
            }
            DaemonToTuiMessage::FlowUpdate { flow } => {
                if let Some(existing) = self.flows.iter_mut().find(|f| f.flow_id == flow.flow_id) {
                    *existing = flow;
                } else {
                    self.flows.push(flow);
                }
            }
            DaemonToTuiMessage::FlowRemoved { flow_id } => {
                self.flows.retain(|f| f.flow_id != flow_id);
                self.clamp_selections();
            }
            DaemonToTuiMessage::AppUpdate { app } => {
                if let Some(existing) = self
                    .apps
                    .iter_mut()
                    .find(|a| a.bundle_id == app.bundle_id)
                {
                    *existing = app;
                } else {
                    self.apps.push(app);
                }
            }
            DaemonToTuiMessage::ProxyStatusUpdate { status } => {
                self.proxy_status = status;
            }
        }
    }

    // -- Private helpers ------------------------------------------------------

    fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Apps => {
                if self.apps.is_empty() {
                    return;
                }
                let len = self.apps.len() as i32;
                let new = (self.selected_app as i32 + delta).rem_euclid(len);
                self.selected_app = new as usize;
            }
            Focus::Flows => {
                if self.flows.is_empty() {
                    return;
                }
                let len = self.flows.len() as i32;
                let new = (self.selected_flow as i32 + delta).rem_euclid(len);
                self.selected_flow = new as usize;
            }
        }
    }

    fn clamp_selections(&mut self) {
        if !self.apps.is_empty() {
            self.selected_app = self.selected_app.min(self.apps.len() - 1);
        } else {
            self.selected_app = 0;
        }
        if !self.flows.is_empty() {
            self.selected_flow = self.selected_flow.min(self.flows.len() - 1);
        } else {
            self.selected_flow = 0;
        }
    }

    async fn toggle_app_routing(&mut self, client: &mut DaemonClient) {
        if let Some(app_info) = self.apps.get(self.selected_app) {
            let new_action = match app_info.routing {
                RouteAction::Passthrough => RouteAction::RouteToProxy,
                RouteAction::RouteToProxy => RouteAction::Passthrough,
            };

            let cmd = TuiCommand::SetAppRouting {
                bundle_id: app_info.bundle_id.clone(),
                action: new_action,
            };

            if let Err(e) = client.send(cmd).await {
                error!("failed to send routing command: {e}");
                self.status_message = format!("Send error: {e}");
            } else {
                self.status_message = format!(
                    "Routing for {} -> {:?}",
                    app_info.name, new_action
                );
            }
        }
    }

    async fn toggle_global_routing(&mut self, client: &mut DaemonClient) {
        let any_routed = self
            .apps
            .iter()
            .any(|a| a.routing == RouteAction::RouteToProxy);

        let new_action = if any_routed {
            RouteAction::Passthrough
        } else {
            RouteAction::RouteToProxy
        };

        let cmd = TuiCommand::SetGlobalRouting {
            action: new_action,
        };

        if let Err(e) = client.send(cmd).await {
            error!("failed to send global routing command: {e}");
            self.status_message = format!("Send error: {e}");
        } else {
            self.status_message = format!("Global routing -> {:?}", new_action);
        }
    }
}

// ---------------------------------------------------------------------------
// Main run loop
// ---------------------------------------------------------------------------

pub async fn run(config: Config) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut events = EventReader::new();
    let mut client = DaemonClient::new(config.socket_path.clone());
    let mut app = App::new();

    match client.connect().await {
        Ok(()) => {
            app.connected = true;
            app.status_message = "Connected".into();
        }
        Err(e) => {
            warn!("initial connection failed: {e}");
            app.status_message = format!("Daemon not available: {e}");
        }
    }

    let mut reconnect_interval = tokio::time::interval(std::time::Duration::from_secs(3));
    reconnect_interval.tick().await;

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        if app.should_quit {
            if client.is_connected() {
                if let Err(e) = client.send(TuiCommand::Shutdown).await {
                    warn!("failed to send shutdown command: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            break;
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Event::Key(key_event)) => {
                        if key_event.kind == crossterm::event::KeyEventKind::Press {
                            app.on_key(key_event.code, &mut client).await;
                        }
                    }
                    Some(Event::Tick) | Some(Event::Resize) => {}
                    None => break,
                }
            }

            maybe_msg = client.recv(), if client.is_connected() => {
                match maybe_msg {
                    Some(msg) => {
                        app.apply_daemon_message(msg);
                    }
                    None => {
                        app.connected = false;
                        app.status_message = "Daemon disconnected".into();
                    }
                }
            }

            _ = reconnect_interval.tick(), if !client.is_connected() => {
                match client.connect().await {
                    Ok(()) => {
                        app.connected = true;
                        app.status_message = "Reconnected".into();
                        info!("reconnected to daemon");
                    }
                    Err(e) => {
                        app.status_message = format!("Reconnect failed: {e}");
                    }
                }
            }
        }
    }

    ratatui::restore();
    Ok(())
}
