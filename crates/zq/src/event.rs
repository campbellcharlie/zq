use crossterm::event::{self, Event as CtEvent, KeyEvent};
use tokio::sync::mpsc;

/// Events produced by the crossterm event reader thread.
pub enum Event {
    /// A key was pressed.
    Key(KeyEvent),
    /// Periodic tick for UI refresh (~250ms).
    Tick,
    /// Terminal was resized.
    Resize,
}

/// Reads crossterm events on a dedicated thread and forwards them
/// as [`Event`] values over a tokio channel.
pub struct EventReader {
    rx: mpsc::UnboundedReceiver<Event>,
}

impl EventReader {
    /// Spawn the background reader thread and return the receiver handle.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        // Use a regular OS thread because crossterm's `poll`/`read` are
        // blocking calls and must not run on the tokio runtime.
        std::thread::Builder::new()
            .name("zq-event-reader".into())
            .spawn(move || {
                loop {
                    // Poll with a 250ms timeout to generate periodic Tick events.
                    let has_event = match event::poll(std::time::Duration::from_millis(250)) {
                        Ok(ready) => ready,
                        Err(_) => continue,
                    };

                    if has_event {
                        match event::read() {
                            Ok(CtEvent::Key(key)) => {
                                if tx.send(Event::Key(key)).is_err() {
                                    // Receiver dropped, shut down.
                                    break;
                                }
                            }
                            Ok(CtEvent::Resize(_, _)) => {
                                if tx.send(Event::Resize).is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {
                                // Mouse events, focus events, paste -- ignore.
                            }
                            Err(_) => continue,
                        }
                    } else {
                        // Timeout elapsed -- emit a tick.
                        if tx.send(Event::Tick).is_err() {
                            break;
                        }
                    }
                }
            })
            .expect("failed to spawn event reader thread");

        Self { rx }
    }

    /// Receive the next event. Returns `None` if the sender thread has exited.
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
