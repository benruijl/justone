use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use clap::ArgMatches;
use futures::{FutureExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::time::delay_for;
use warp::ws::{Message, WebSocket};
use warp::Filter;

use crate::justone;

struct User {
    name: String,
    password: String,
    tx: mpsc::UnboundedSender<Result<Message, warp::Error>>,
}

struct Game {
    started: bool, // still accepting new players?
    client_sender: mpsc::UnboundedSender<justone::ClientMessage>,
    users: Vec<User>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    Register {
        game_id: String,
        name: String,
        password: String,
    },
    WordSubmission {
        word: String,
    },
    ManualDuplicateElimination {
        ids: Vec<usize>,
    },
    WordGuess {
        word: String,
    },
    AcceptGuess {
        accept: bool,
    },
    StartGame,
}

struct ServerState {
    games: HashMap<String, Game>,
}

#[derive(Debug, Deserialize)]
struct QueryOptions {
    game_id: Option<String>,
}

pub async fn start<'a>(cli_options: &ArgMatches<'a>) {
    let port: u16 = cli_options
        .value_of("port")
        .unwrap()
        .parse::<u16>()
        .expect("Port must be an integer.");

    let server_state = Arc::new(Mutex::new(ServerState {
        games: HashMap::new(),
    }));

    let server_state_filter = warp::any().map(move || server_state.clone());

    let chat = warp::path("chat")
        .and(warp::ws())
        .and(warp::addr::remote())
        .and(warp::query::<QueryOptions>())
        .and(server_state_filter)
        .map(
            |ws: warp::ws::Ws,
             remote_addr: Option<std::net::SocketAddr>,
             query_options,
             server_state_filter| {
                ws.on_upgrade(move |socket| {
                    new_ws_connection(socket, remote_addr, query_options, server_state_filter)
                })
            },
        );

    let mut index_page = String::new();
    std::io::BufReader::new(std::fs::File::open("./include/index.html").unwrap())
        .read_to_string(&mut index_page)
        .unwrap();

    let index_site = warp::get()
        .and(warp::path::end())
        .map(move || warp::reply::html(index_page.clone()));

    let favicon = warp::path("favicon.ico")
        .and(warp::path::end())
        .and(warp::fs::file("./include/favicon.ico"));

    let routes = favicon.or(index_site).or(chat);

    if cli_options.is_present("tls") {
        let filenames: Vec<&str> = cli_options.values_of("tls").unwrap().collect();
        warp::serve(routes)
            .tls()
            .cert_path(filenames[0])
            .key_path(filenames[1])
            .run(([0, 0, 0, 0], port))
            .await;
    } else {
        warp::serve(routes).run(([0, 0, 0, 0], port)).await;
    }
}

async fn forward_server_message(
    game_id: String,
    mut server_message_rx: mpsc::UnboundedReceiver<justone::ServerMessage>,
    server_state: Arc<Mutex<ServerState>>,
) {
    let mut game_started = false;

    while let Some(req) = server_message_rx.next().await {
        let (i, m) = match &req {
            justone::ServerMessage::UserAdded(i, username) => (
                i,
                json!({
                    "type": "UserAdded",
                    "username": username,
                    "id": i,
                    "game_id": &game_id
                }),
            ),
            justone::ServerMessage::Finished(i, points) => (
                i,
                json!({
                    "type": "Finished",
                    "points": points,
                }),
            ),
            justone::ServerMessage::RoundStart(
                i,
                current_user,
                decision_user,
                score,
                words_left,
            ) => {
                if !game_started {
                    server_state
                        .lock()
                        .await
                        .games
                        .get_mut(&game_id)
                        .unwrap()
                        .started = true;
                    game_started = true;
                }

                (
                    i,
                    json!({
                        "type": "RoundStart",
                        "current_user": current_user,
                        "decision_user": decision_user,
                        "score": score,
                        "words_left": words_left,
                    }),
                )
            }
            justone::ServerMessage::WordSubmitted(i, u) => (
                i,
                json!({
                    "type": "WordSubmitted",
                    "user": u,
                }),
            ),
            justone::ServerMessage::WordSubmissionRequest(i, w) => (
                i,
                json!({
                    "type": "WordSubmissionRequest",
                    "word": w,
                }),
            ),
            justone::ServerMessage::WordGuessRequest(i) => (
                i,
                json!({
                    "type": "WordGuessRequest",
                }),
            ),
            justone::ServerMessage::AcceptGuessRequest(i) => (
                i,
                json!({
                    "type": "AcceptGuessRequest",
                }),
            ),
            justone::ServerMessage::ShowFilteredWords(i, w) => (
                i,
                json!({
                    "type": "ShowFilteredWords",
                    "words": w,
                }),
            ),
            justone::ServerMessage::ShowWords(i, w) => (
                i,
                json!({
                    "type": "ShowWords",
                    "words": w,
                }),
            ),
            justone::ServerMessage::ShowRoundOverview(i, word_to_guess, overview) => (
                i,
                json!({
                    "type": "ShowRoundOverview",
                    "word_to_guess": word_to_guess,
                    "overview": overview,
                }),
            ),
            justone::ServerMessage::ManualDuplicateEliminationRequest(i) => (
                i,
                json!({
                    "type": "ManualDuplicateEliminationRequest",
                }),
            ),
        };

        info!("{}", m.to_string());

        // TODO: find a solution that doesn't require a lock
        match server_state.lock().await.games[&game_id].users.get(*i) {
            Some(u) => {
                u.tx.send(Ok(Message::text(m.to_string())))
                    .unwrap_or_else(|e| info!("Error sending {:?} to client: {}", req, e));
            }
            None => {
                error!("Unknown user id {}", i);
            }
        }
    }

    info!("Server message loop closing: removing game {}", game_id);

    if server_state.lock().await.games.remove(&game_id).is_none() {
        error!("Tried to remove game {} that does not exist", game_id);
    };
}

async fn new_ws_connection(
    ws: WebSocket,
    remote_addr: Option<std::net::SocketAddr>,
    _query_options: QueryOptions,
    server_state: Arc<Mutex<ServerState>>,
) {
    info!("New user connected to websocket: {:?}", remote_addr);

    let (user_ws_tx, mut user_ws_rx) = ws.split();

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn(rx.forward(user_ws_tx).map(|result| {
        if let Err(e) = result {
            eprintln!("websocket send error: {}", e);
        }
    }));

    let mut client_sender = None;
    let mut client_id = None;

    while let Some(result) = user_ws_rx.next().await {
        match result {
            Ok(msg) => {
                if !msg.is_text() {
                    info!("Received non-text message {:?}", msg);
                    continue;
                }

                let msg = msg.to_str().expect("Message type should be text");
                info!("Received {:?}", msg);

                let client_message: ClientMessage = match serde_json::from_str(msg) {
                    Ok(v) => v,
                    Err(e) => {
                        error!("Client sent unknown message: {}; Error: {}", msg, e);
                        continue;
                    }
                };

                match client_message {
                    ClientMessage::Register {
                        mut game_id,
                        name,
                        password,
                    } => {
                        let games = &mut server_state.lock().await.games;

                        if game_id.is_empty() || !games.contains_key(&game_id) {
                            // create a new game
                            let (c_tx, c_rx) = mpsc::unbounded_channel();
                            let (s_tx, s_rx) = mpsc::unbounded_channel();
                            tokio::spawn(justone::JustOneGame::new_game(c_rx, s_tx));

                            client_sender = Some(c_tx.clone());
                            client_id = Some(0); // using the index as the id is not very flexible

                            let user = User {
                                name: name.clone(),
                                password,
                                tx: tx.clone(),
                            };

                            if game_id.is_empty() {
                                game_id = thread_rng().sample_iter(&Alphanumeric).take(5).collect()
                            }

                            loop {
                                if !games.contains_key(&game_id) {
                                    info!("Created new game with id {}", game_id);
                                    tokio::spawn(forward_server_message(
                                        game_id.clone(),
                                        s_rx,
                                        server_state.clone(),
                                    ));
                                    games.insert(
                                        game_id.clone(),
                                        Game {
                                            client_sender: c_tx.clone(),
                                            users: vec![user],
                                            started: false,
                                        },
                                    );

                                    // remove abandoned games after 90 minutes
                                    let game_id = game_id.clone();
                                    let c_tx_abandon = c_tx.clone();
                                    tokio::spawn(async move {
                                        delay_for(Duration::from_secs(90 * 60)).await;

                                        info!("Closing game {} due to timeout", game_id);
                                        c_tx_abandon
                                            .send(justone::ClientMessage::StopGame)
                                            .unwrap_or_else(|e| {
                                                debug!("Game {} already closed: {}", game_id, e)
                                            });
                                    });
                                    break;
                                }

                                // generate a random id
                                game_id = thread_rng().sample_iter(&Alphanumeric).take(5).collect();
                            }

                            c_tx.send(justone::ClientMessage::NewPlayer(name.clone()))
                                .unwrap_or_else(|e| {
                                    info!("Error sending NewPlayer to server: {}", e)
                                });
                        } else if let Some(game) = games.get_mut(&game_id) {
                            client_sender = Some(game.client_sender.clone());

                            // attempt to join an existing game
                            // check if this user is already in the game
                            let mut new_player = true;
                            for (u_i, u) in game.users.iter_mut().enumerate() {
                                if u.name == name {
                                    new_player = false;
                                    if u.password == password {
                                        warn!(
                                            "User {} reconnecting: partially implemented",
                                            u.name
                                        );
                                        u.tx = tx.clone();
                                        client_id = Some(u_i);

                                        // send the reconnecting message to the client here, since the logic
                                        // may be in a state where it takes a while before the reconnecting occurs
                                        let m = json!({
                                            "type": "Reconnecting",
                                            "id": u_i,
                                        });
                                        u.tx.send(Ok(Message::text(m.to_string()))).unwrap_or_else(
                                            |e| info!("Error sending {:?} to client: {}", m, e),
                                        );

                                        game.client_sender
                                            .send(justone::ClientMessage::ResendGameState(u_i))
                                            .unwrap_or_else(|e| {
                                                info!("Error sending NewPlayer to server: {}", e)
                                            });
                                        break;
                                    } else {
                                        // reject the player
                                        let m = json!({
                                            "type": "Rejected",
                                            "reason": "the password does not match the existing player's",
                                        });
                                        tx.send(Ok(Message::text(m.to_string()))).unwrap_or_else(
                                            |e| info!("Error sending {:?} to client: {}", m, e),
                                        );
                                    }
                                    break;
                                }
                            }

                            if new_player {
                                // do not allow a new player on a game that has started
                                if game.started {
                                    let m = json!({
                                        "type": "Rejected",
                                        "reason": "the game has already started",
                                    });
                                    tx.send(Ok(Message::text(m.to_string())))
                                        .unwrap_or_else(|e| {
                                            info!("Error sending {:?} to client: {}", m, e)
                                        });
                                } else {
                                    client_id = Some(game.users.len());

                                    let user = User {
                                        name: name.clone(),
                                        password,
                                        tx: tx.clone(),
                                    };

                                    game.users.push(user);

                                    game.client_sender
                                        .send(justone::ClientMessage::NewPlayer(name.clone()))
                                        .unwrap_or_else(|e| {
                                            info!("Error sending NewPlayer to server: {}", e)
                                        });
                                }
                            }
                        } else {
                            unreachable!()
                        }
                    }
                    ClientMessage::WordSubmission { word } if client_id.is_some() => {
                        let m = justone::ClientMessage::WordSubmissionResponse(
                            client_id.unwrap(),
                            word,
                        );

                        client_sender
                            .as_ref()
                            .unwrap()
                            .send(m)
                            .unwrap_or_else(|e| info!("Error sending {:?} to server: {}", msg, e));
                    }
                    ClientMessage::ManualDuplicateElimination { ids } if client_id.is_some() => {
                        let m = justone::ClientMessage::ManualDuplicateEliminationResponse(
                            client_id.unwrap(),
                            ids,
                        );
                        client_sender
                            .as_ref()
                            .unwrap()
                            .send(m)
                            .unwrap_or_else(|e| info!("Error sending {:?} to server: {}", msg, e));
                    }
                    ClientMessage::WordGuess { word } if client_id.is_some() => {
                        let m = justone::ClientMessage::WordGuessResponse(client_id.unwrap(), word);
                        client_sender
                            .as_ref()
                            .unwrap()
                            .send(m)
                            .unwrap_or_else(|e| info!("Error sending {:?} to server: {}", msg, e));
                    }
                    ClientMessage::AcceptGuess { accept } if client_id.is_some() => {
                        let m =
                            justone::ClientMessage::AcceptGuessResponse(client_id.unwrap(), accept);
                        client_sender
                            .as_ref()
                            .unwrap()
                            .send(m)
                            .unwrap_or_else(|e| info!("Error sending {:?} to server: {}", msg, e));
                    }
                    ClientMessage::StartGame if client_id == Some(0) => {
                        let m = justone::ClientMessage::StartGame;
                        client_sender
                            .as_ref()
                            .unwrap()
                            .send(m)
                            .unwrap_or_else(|e| info!("Error sending {:?} to server: {}", msg, e));
                    }
                    _ => error!(
                        "Message received but there is no client id: {:?}",
                        client_message
                    ),
                }
            }
            Err(e) => {
                error!("websocket error(uid={:?}): {}", client_id, e);
                break;
            }
        };
    }

    info!("User disconnected: {:?}", remote_addr);
}
