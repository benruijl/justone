#[macro_use]
extern crate log;

#[macro_use]
extern crate serde_derive;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use clap::{App, Arg};
use futures::{FutureExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{mpsc, Mutex};
use warp::ws::{Message, WebSocket};
use warp::Filter;

mod logic;

struct User {
    id: usize,
    name: String,
    password: String,
    connected: bool,
}

struct Game {
    id: String,
    started: bool, // still accepting new players?
    client_sender: mpsc::UnboundedSender<logic::ClientMessage>,
    users: Vec<User>,
}

struct ServerState {
    games: HashMap<String, Game>,
}

#[derive(Debug, Deserialize)]
struct QueryOptions {
    game_id: Option<String>,
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let matches = App::new("Just One")
        .version("0.1")
        .author("Ben Ruijl <benruyl@gmail.com>")
        .about("Just One game server")
        .arg(
            Arg::with_name("cli")
                .short("c")
                .long("use_cli")
                .help("Use CLI instead of web server"),
        )
        .get_matches();

    if matches.is_present("cli") {
        logic::Logic::new_game_cli(2).await;
        return;
    }

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
                    new_ws_connection(
                        socket,
                        remote_addr.unwrap(),
                        query_options,
                        server_state_filter,
                    )
                })
            },
        );

    // GET / -> index html
    let index_site = //warp::path::end().map(|| warp::reply::html(contents.clone()));
    warp::get()
    .and(warp::path::end())
    .and(warp::fs::file("./include/index.html"));

    let routes = index_site.or(chat);

    //.tls()
    //.cert_path("examples/tls/cert.pem")
    //.key_path("examples/tls/key.rsa")
    warp::serve(routes).run(([0, 0, 0, 0], 3030)).await;
}

async fn forward_server_message(
    game_id: String,
    mut rreq: mpsc::UnboundedReceiver<logic::ServerMessage>,
    tx: mpsc::UnboundedSender<Result<Message, warp::Error>>,
) {
    while let Some(req) = rreq.next().await {
        let m = match &req {
            logic::ServerMessage::UserAdded(i, username) => json!({
                "type": "UserAdded",
                "username": username,
                "id": i,
                "game_id": &game_id
            }),
            logic::ServerMessage::Finished(_i, points) => json!({
                "type": "Finished",
                "points": points,
            }),
            logic::ServerMessage::RoundStart(
                _i,
                current_user,
                decision_user,
                score,
                words_left,
            ) => json!({
                "type": "RoundStart",
                "current_user": current_user,
                "decision_user": decision_user,
                "score": score,
                "words_left": words_left,
            }),
            logic::ServerMessage::WordSubmitted(_i, u) => json!({
                "type": "WordSubmitted",
                "user": u,
            }),
            logic::ServerMessage::WordSubmissionRequest(_i, w) => json!({
                "type": "WordSubmissionRequest",
                "word": w,
            }),
            logic::ServerMessage::WordGuessRequest(_i) => json!({
                "type": "WordGuessRequest",
            }),
            logic::ServerMessage::AcceptGuessRequest(_i) => json!({
                "type": "AcceptGuessRequest",
            }),
            logic::ServerMessage::ShowFilteredWords(_i, w) => json!({
                "type": "ShowFilteredWords",
                "words": w,
            }),
            logic::ServerMessage::ShowWords(_i, w) => json!({
                "type": "ShowWords",
                "words": w,
            }),
            logic::ServerMessage::ShowRoundOverview(_i, word_to_guess, overview) => json!({
                "type": "ShowRoundOverview",
                "word_to_guess": word_to_guess,
                "overview": overview,
            }),
            logic::ServerMessage::ManualDuplicateEliminationRequest(_i) => json!({
                "type": "ManualDuplicateEliminationRequest",
            }),
        };

        info!("{}", m.to_string());

        tx.send(Ok(Message::text(m.to_string())))
            .unwrap_or_else(|e| info!("Error sending {:?} to client: {}", req, e));
    }

    info!("Server message loop closing!");
    // TODO: clean up the game here?
}

async fn new_ws_connection(
    ws: WebSocket,
    remote_addr: std::net::SocketAddr,
    _query_options: QueryOptions,
    server_state: Arc<Mutex<ServerState>>,
) {
    // Use a counter to assign a new unique ID for this user.
    info!("New user connected to websocket: {:?}", remote_addr);

    // Split the socket into a sender and receive of messages.
    let (user_ws_tx, mut user_ws_rx) = ws.split();

    // Use an unbounded channel to handle buffering and flushing of messages
    // to the websocket...
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

                info!("{:?}", msg);
                info!("Received {}", msg.to_str().unwrap());
                // TODO: guard
                let value: serde_json::Value = serde_json::from_str(msg.to_str().unwrap()).unwrap();
                match value["type"].as_str().unwrap() {
                    "Register" => {
                        let mut game_id = value["game_id"].as_str().unwrap().to_string();
                        let username = value["name"].as_str().unwrap().to_string();
                        let password = value["password"].as_str().unwrap().to_string();

                        let games = &mut server_state.lock().await.games;

                        if game_id.is_empty() || !games.contains_key(&game_id) {
                            // create a new game
                            let (s, r) = mpsc::unbounded_channel();
                            tokio::spawn(logic::Logic::new_game(r));

                            client_sender = Some(s.clone());
                            client_id = Some(0); // using the index as the id is not very flexible

                            // create a new user registered to the logic
                            let (sreq, rreq) = mpsc::unbounded_channel();

                            let logic_user = logic::User {
                                name: username.clone(),
                                word_guess: None,
                                word_stricken: false,
                                channel_out: sreq.clone(),
                            };

                            let user = User {
                                id: 0,
                                name: username,
                                password,
                                connected: true,
                            };

                            if game_id.is_empty() {
                                game_id = thread_rng().sample_iter(&Alphanumeric).take(5).collect()
                            }

                            loop {
                                if !games.contains_key(&game_id) {
                                    info!("Created new game with id {}", game_id);
                                    tokio::spawn(forward_server_message(
                                        game_id.clone(),
                                        rreq,
                                        tx.clone(),
                                    ));
                                    games.insert(
                                        game_id.clone(),
                                        Game {
                                            id: game_id.clone(),
                                            client_sender: s.clone(),
                                            users: vec![user],
                                            started: false,
                                        },
                                    );
                                    break;
                                }

                                // generate a random id
                                game_id = thread_rng().sample_iter(&Alphanumeric).take(5).collect();
                            }

                            s.send(logic::ClientMessage::NewPlayer(logic_user))
                                .unwrap_or_else(|e| {
                                    info!("Error sending NewPlayer to server: {}", e)
                                });
                        } else if let Some(game) = games.get_mut(&game_id) {
                            // attempt to join an existing game
                            // check if this user is already in the game
                            let mut new_player = true;
                            for (u_i, u) in game.users.iter().enumerate() {
                                if u.name == username && u.password == password {
                                    // already exists
                                    error!("User {} reconnecting: not implemented", u.name);
                                    new_player = false;
                                    client_id = Some(u_i);
                                    break;
                                }
                            }

                            client_sender = Some(game.client_sender.clone());

                            // TODO: do not allow a new player on a game that has started
                            if new_player {
                                client_id = Some(game.users.len());

                                let (sreq, rreq) = mpsc::unbounded_channel();
                                tokio::spawn(forward_server_message(
                                    game_id.clone(),
                                    rreq,
                                    tx.clone(),
                                ));
                                let logic_user = logic::User {
                                    name: username.clone(),
                                    word_guess: None,
                                    word_stricken: false,
                                    channel_out: sreq.clone(),
                                };

                                let user = User {
                                    id: client_id.unwrap(),
                                    name: username,
                                    password,
                                    connected: true,
                                };

                                game.users.push(user);

                                client_sender
                                    .as_ref()
                                    .unwrap()
                                    .send(logic::ClientMessage::NewPlayer(logic_user))
                                    .unwrap_or_else(|e| {
                                        info!("Error sending NewPlayer to server: {}", e)
                                    });
                            }
                        } else {
                            unreachable!()
                        }
                    }
                    "WordSubmission" if client_id.is_some() => {
                        let m = logic::ClientMessage::WordSubmissionResponse(
                            client_id.unwrap(),
                            value["word"].as_str().unwrap().to_string(),
                        );

                        client_sender.as_ref().unwrap().send(m).unwrap_or_else(|e| {
                            info!("Error sending {:?} to server: {}", value, e)
                        });
                    }
                    "ManualDuplicateElimination" if client_id.is_some() => {
                        let m = logic::ClientMessage::ManualDuplicateEliminationResponse(
                            client_id.unwrap(),
                            value["ids"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|x| x.as_u64().unwrap() as usize)
                                .collect(),
                        );
                        client_sender.as_ref().unwrap().send(m).unwrap_or_else(|e| {
                            info!("Error sending {:?} to server: {}", value, e)
                        });
                    }
                    "WordGuess" if client_id.is_some() => {
                        let m = logic::ClientMessage::WordGuessResponse(
                            client_id.unwrap(),
                            value["word"].as_str().unwrap().to_string(),
                        );
                        client_sender.as_ref().unwrap().send(m).unwrap_or_else(|e| {
                            info!("Error sending {:?} to server: {}", value, e)
                        });
                    }
                    "AcceptGuess" if client_id.is_some() => {
                        let m = logic::ClientMessage::AcceptGuessResponse(
                            client_id.unwrap(),
                            value["accept"].as_bool().unwrap(),
                        );
                        client_sender.as_ref().unwrap().send(m).unwrap_or_else(|e| {
                            info!("Error sending {:?} to server: {}", value, e)
                        });
                    }
                    "StartGame" if client_id == Some(0) => {
                        let m = logic::ClientMessage::StartGame;
                        client_sender.as_ref().unwrap().send(m).unwrap_or_else(|e| {
                            info!("Error sending {:?} to server: {}", value, e)
                        });
                    }
                    _ => {
                        error!("Unknown client message from {:?}: {}", remote_addr, value);
                    }
                }
            }
            Err(e) => {
                error!("websocket error(uid={:?}): {}", client_id, e);
                break;
            }
        };
    }

    // TODO: set the disconnected state
    info!("User disconnected: {:?}", remote_addr);
}
