use crate::{
    api_manager::models::{EventInfo, EventType, WebsocketEvents},
    bridge::BridgeState,
};
use api_manager::ApiManager;

use bridge::Bridge;
use crossbeam_channel::unbounded;
use sqlx::{Connection, Executor, SqliteConnection};
use tokio::{
    fs::OpenOptions,
    spawn,
    task::{spawn_blocking, JoinHandle},
};
mod api_manager;
mod bridge;
mod client_update_check;
mod parser;

#[tokio::main]
async fn main() {
    let _guard = sentry::init((
        "https://2a3db3e9cab34ab2996414dd5bf6e169@o229745.ingest.sentry.io/5843753",
        sentry::ClientOptions {
            release: sentry::release_name!(),
            ..Default::default()
        },
    ));
    setup_db().await;

    client_update_check::check_updates().await;

    let mut manager = Manager::new();
    manager.start().await;
}

struct Manager {
    bridge_thread: Option<JoinHandle<()>>,
    api_thread: Option<JoinHandle<()>>,
}

impl Manager {
    fn new() -> Self {
        Self {
            bridge_thread: None,
            api_thread: None,
        }
    }

    async fn start<'a>(&'a mut self) {
        let (dist_sender, dist_receiver) = unbounded();

        let dist_sender_clone = dist_sender.clone();
        let (ws_sender, ws_receiver) = unbounded();
        let (bridge_sender, bridge_receiver) = unbounded();
        self.api_thread = Some(spawn_blocking(move || {
            let _ = spawn(ApiManager::start(dist_sender_clone, ws_receiver));
        }));

        for event in dist_receiver.iter() {
            match event.event_type {
                EventType::Bridge(api_manager::models::BridgeEvents::ConnectionCreate {
                    address,
                    port,
                }) => {
                    println!(
                        "[MAIN] Creating new bridge instance: {}:{}",
                        &address, &port
                    );
                    if self.bridge_thread.is_some() {
                        panic!("Created connection before old connection was terminated");
                        // continue;
                    }

                    let dist_sender_clone = dist_sender.clone();
                    let bridge_receiver_clone = bridge_receiver.clone();
                    self.bridge_thread = Some(spawn_blocking(move || {
                        let mut bridge =
                            Bridge::new(dist_sender_clone, bridge_receiver_clone, address, port);
                        bridge.start();
                    }));
                }
                EventType::Bridge(api_manager::models::BridgeEvents::ConnectionCreateError {
                    error,
                }) => {
                    eprintln!("[BRIDGE] Creating connection caused an error: {} ", error);

                    dist_sender
                        .send(EventInfo {
                            event_type: EventType::Websocket(WebsocketEvents::StateUpdate {
                                state: bridge::BridgeState::ERRORED,
                                description: api_manager::models::StateDescription::Error {
                                    message: error,
                                },
                            }),
                        })
                        .expect("Cannot send message");

                    if let Some(handle) = &self.bridge_thread {
                        handle.abort();
                        self.bridge_thread = None;
                    } else {
                        panic!("Connection error when thread was already closed.");
                        // continue
                    }
                }
                EventType::Bridge(api_manager::models::BridgeEvents::TerminalRead { message }) => {
                    println!("[Bridge] Received message: {}", message);
                    // todo: Group messages in "chunks", to make interface updates better to handle.

                    dist_sender
                        .send(EventInfo {
                            event_type: EventType::Websocket(WebsocketEvents::TerminalRead {
                                message,
                            }),
                        })
                        .expect("Cannot send message");
                }
                EventType::Bridge(api_manager::models::BridgeEvents::TerminalSend { message }) => {
                    if self.bridge_thread.is_none() {
                        continue;
                    }
                    bridge_sender
                        .send(EventInfo {
                            event_type: EventType::Bridge(
                                api_manager::models::BridgeEvents::TerminalSend {
                                    message: message.clone(),
                                },
                            ),
                        })
                        .expect("Cannot send message");
                    dist_sender
                        .send(EventInfo {
                            event_type: EventType::Websocket(WebsocketEvents::TerminalSend {
                                message,
                            }),
                        })
                        .expect("Cannot send message");
                }
                EventType::Bridge(api_manager::models::BridgeEvents::PrintEnd) => {
                    bridge_sender
                        .send(EventInfo {
                            event_type: EventType::Bridge(
                                api_manager::models::BridgeEvents::PrintEnd,
                            ),
                        })
                        .expect("Cannot send message");
                }
                EventType::Bridge(api_manager::models::BridgeEvents::PrintStart { info }) => {
                    if self.bridge_thread.is_none() {
                        continue;
                    }
                    bridge_sender
                        .send(EventInfo {
                            event_type: EventType::Bridge(
                                api_manager::models::BridgeEvents::PrintStart { info },
                            ),
                        })
                        .expect("Cannot send message");
                }
                EventType::Bridge(api_manager::models::BridgeEvents::StateUpdate {
                    state,
                    description,
                }) => {
                    if self.bridge_thread.is_none() {
                        continue;
                    }

                    if state == BridgeState::DISCONNECTED || state == BridgeState::ERRORED {
                        let _ = bridge_sender.send(EventInfo {
                            event_type: EventType::KILL,
                        });
                        self.bridge_thread.take();
                    } else {
                        bridge_sender
                            .send(EventInfo {
                                event_type: EventType::Bridge(
                                    api_manager::models::BridgeEvents::StateUpdate {
                                        state: state.clone(),
                                        description: description.clone(),
                                    },
                                ),
                            })
                            .expect("Cannot send message");
                    }

                    dist_sender
                        .send(EventInfo {
                            event_type: EventType::Websocket(
                                api_manager::models::WebsocketEvents::StateUpdate {
                                    state,
                                    description,
                                },
                            ),
                        })
                        .expect("Cannot send message");
                }
                EventType::Websocket(ws_event) => {
                    ws_sender
                        .send(EventInfo {
                            event_type: EventType::Websocket(ws_event),
                        })
                        .expect("Failed to send message to websocket");
                }
                _ => (),
            }
        }
    }
}

async fn setup_db() {
    let _ = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open("storage.db")
        .await;

    let mut connection = (SqliteConnection::connect("storage.db")).await.unwrap();
    connection
        .execute(
            "
        CREATE TABLE IF NOT EXISTS users (
            username VARCHAR(255) NOT NULL primary key,
            password VARCHAR(255) NOT NULL,
            permissions INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS tokens (
            username VARCHAR(255) NOT NULL,
            token VARCHAR(255) NOT NULL primary key,
            expire DATETIME,
            FOREIGN KEY(username) REFERENCES users(username) on update cascade on delete cascade
        );
        
        CREATE TABLE IF NOT EXISTS settings (
            id varchar(255) primary key,
            value TEXT,
            type integer(3) not null
        );

        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('S_devicePath', 0, null);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('N_deviceBaud', 2, null);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('B_startOnBoot', 1, false);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('F_adjustCorrectionF', 3, null);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('B_savePrinterNotifications', 1, true);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('B_savePrinterNotifications', 1, true);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('N_deviceWidth', 2, null);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('N_deviceHeight', 2, null);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('N_deviceDepth', 2, null);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('B_deviceHB', 1, false);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('B_deviceHC', 1, false);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('N_clientTerminalAmount', 2, 500);
        INSERT OR IGNORE INTO SETTINGS (id, type, value) VALUES ('S_sentryDsn', 0, 'https://cd35379ff0fc45daa30a67bfe9aa8b36@0229745.ingest.sentry.io/5778789');

        DELETE FROM tokens where expire < DATE('now');
    ",
        )
        .await
        .expect("Error while creating tables.");
}
