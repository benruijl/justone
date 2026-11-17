use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use justone::game::{Game, ServerMessage};
use justone::room;

const INDEX: &str = include_str!("../public/index.html");
const FAVICON_SVG: &str = include_str!("../public/favicon.svg");
const FAVICON_ICO: &[u8] = include_bytes!("../public/favicon.ico");
const TOUCH_ICON: &[u8] = include_bytes!("../public/apple-touch-icon.png");

type Tx = mpsc::UnboundedSender<Message>;

struct Connection {
    player: Option<usize>,
    tx: Tx,
}

struct Room {
    game: Option<Game>,
    connections: HashMap<u64, Connection>,
    last_active: Instant,
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(0);

fn random_u64() -> u64 {
    // RandomState is seeded from the operating system, which is plenty for word order and ids
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.write_u64(NEXT_CONNECTION.load(Ordering::Relaxed));
    h.finish()
}

fn text(msg: &ServerMessage) -> Message {
    Message::Text(serde_json::to_string(msg).unwrap_or_default().into())
}

fn close(reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code: 1000,
        reason: reason.into(),
    }))
}

#[tokio::main]
pub async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3030);
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());

    let rooms: Rooms = Arc::default();
    tokio::spawn(remove_expired_games(rooms.clone()));

    let file = |content_type: &'static str, body: &'static [u8]| {
        move || async move { ([(header::CONTENT_TYPE, content_type)], body) }
    };
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/chat", get(chat))
        .route("/favicon.svg", get(file("image/svg+xml", FAVICON_SVG.as_bytes())))
        .route("/favicon.ico", get(file("image/x-icon", FAVICON_ICO)))
        .route("/apple-touch-icon.png", get(file("image/png", TOUCH_ICON)))
        .with_state(rooms);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .unwrap_or_else(|e| panic!("Cannot listen on {}:{}: {}", host, port, e));
    println!("Just One is running on http://{}:{}", host, port);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
}

async fn chat(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(rooms): State<Rooms>,
) -> Response {
    let requested = query
        .get("game_id")
        .map(|id| id.trim().to_string())
        .unwrap_or_default();
    let game_id = if requested.is_empty() {
        room::game_id_from_random(random_u64())
    } else {
        requested
    };
    ws.on_upgrade(move |socket| connection(socket, game_id, rooms))
        .into_response()
}

async fn connection(socket: WebSocket, game_id: String, rooms: Rooms) {
    let (mut sink, mut stream) = socket.split();

    // all writes go through a channel, so other connections can send to this one
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let closing = matches!(msg, Message::Close(_));
            if sink.send(msg).await.is_err() || closing {
                break;
            }
        }
    });

    let valid = room::valid_game_id(&game_id);
    let id = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
    if valid {
        rooms
            .lock()
            .unwrap()
            .entry(game_id.clone())
            .or_insert_with(|| Room {
                game: None,
                connections: HashMap::new(),
                last_active: Instant::now(),
            })
            .connections
            .insert(
                id,
                Connection {
                    player: None,
                    tx: tx.clone(),
                },
            );
    } else {
        let _ = tx.send(text(&room::rejected(room::INVALID_GAME_ID)));
    }

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) if t.as_str() == room::PING => {
                let _ = tx.send(Message::Text(room::PONG.into()));
            }
            Message::Text(t) if valid => handle(&rooms, &game_id, id, t.as_str(), &tx),
            Message::Close(_) => break,
            _ => {}
        }
    }

    // the player stays in the game and can reconnect; only forget this connection
    let mut rooms = rooms.lock().unwrap();
    if let Some(r) = rooms.get_mut(&game_id) {
        r.connections.remove(&id);
        if r.game.is_none() && r.connections.is_empty() {
            rooms.remove(&game_id);
        }
    }
}

fn handle(rooms: &Rooms, game_id: &str, id: u64, msg: &str, tx: &Tx) {
    let mut rooms = rooms.lock().unwrap();
    let Some(r) = rooms.get_mut(game_id) else {
        return;
    };
    // a connection that was replaced by a newer one for the same player is no longer listed
    let Some(mut player) = r.connections.get(&id).map(|c| c.player) else {
        return;
    };

    let response = room::handle_message(&mut r.game, game_id, &mut player, msg, random_u64());

    if let Some(c) = r.connections.get_mut(&id) {
        c.player = player;
    }
    for m in &response.reply {
        let _ = tx.send(text(m));
    }
    if let Some(p) = response.replaces_player {
        r.connections.retain(|&other, c| {
            let replaced = other != id && c.player == Some(p);
            if replaced {
                let _ = c.tx.send(close(room::RECONNECTED_ELSEWHERE));
            }
            !replaced
        });
    }
    for (to, m) in &response.to_players {
        let m = text(m);
        for c in r.connections.values() {
            if c.player == Some(*to) {
                let _ = c.tx.send(m.clone());
            }
        }
    }
    if response.changed {
        r.last_active = Instant::now();
    }
}

async fn remove_expired_games(rooms: Rooms) {
    let expiry = Duration::from_millis(room::EXPIRY_MS);
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        rooms.lock().unwrap().retain(|_, r| {
            let expired = r.game.is_some() && r.last_active.elapsed() > expiry;
            if expired {
                for c in r.connections.values() {
                    let _ = c.tx.send(close(room::GAME_EXPIRED));
                }
            }
            !expired
        });
    }
}
