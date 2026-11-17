//! Cloudflare Worker entry point and the per-game Durable Object.
//!
//! The static page is served by Workers static assets (`public/`). The Worker only handles
//! `/chat?game_id=<id>` WebSocket upgrades and routes them to the Durable Object named after
//! the game id. The Durable Object uses the WebSocket hibernation API and persists the game
//! after every event, so it can be evicted from memory between moves.

use std::cell::RefCell;

use serde::{Deserialize, Serialize};
use worker::*;

use crate::game::{Game, Outbox, ServerMessage, WordList};

/// Games are removed after this much inactivity.
const EXPIRY_MS: u64 = 90 * 60 * 1000;
const STORAGE_KEY: &str = "record";

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    NewGame {
        name: String,
        password: String,
        word_list: WordList,
        word_count: usize,
    },
    JoinGame {
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

fn now_ms() -> u64 {
    Date::now().as_millis()
}

fn random_u64() -> u64 {
    let hi = (js_sys::Math::random() * 4294967296.0) as u64;
    let lo = (js_sys::Math::random() * 4294967296.0) as u64;
    (hi << 32) | lo
}

fn random_game_id() -> String {
    const CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..5)
        .map(|_| CHARS[(js_sys::Math::random() * CHARS.len() as f64) as usize] as char)
        .collect()
}

fn valid_game_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn send(ws: &WebSocket, msg: &ServerMessage) {
    if let Ok(text) = serde_json::to_string(msg) {
        // the socket may already be closed; nothing to do then
        let _ = ws.send_with_str(text);
    }
}

fn reject(ws: &WebSocket, reason: &str) {
    send(
        ws,
        &ServerMessage::Rejected {
            reason: reason.to_string(),
        },
    );
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    if url.path() != "/chat" {
        return Response::error("Not found", 404);
    }
    if req.headers().get("Upgrade")?.as_deref() != Some("websocket") {
        return Response::error("Expected a WebSocket upgrade", 426);
    }

    let requested = url
        .query_pairs()
        .find(|(k, _)| k == "game_id")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    let game_id = if requested.is_empty() {
        random_game_id()
    } else {
        requested
    };

    if !valid_game_id(&game_id) {
        let pair = WebSocketPair::new()?;
        pair.server.accept()?;
        reject(
            &pair.server,
            "the game id may only contain letters, digits, - and _ (at most 32 characters)",
        );
        return Response::from_websocket(pair.client);
    }

    let stub = env.durable_object("GAMES")?.get_by_name(&game_id)?;
    let headers = Headers::new();
    headers.set("Upgrade", "websocket")?;
    let mut init = RequestInit::new();
    init.with_headers(headers);
    let forwarded =
        Request::new_with_init(&format!("https://game/chat?game_id={}", game_id), &init)?;
    stub.fetch_with_request(forwarded).await
}

/// Per-socket data that survives hibernation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attachment {
    /// Unique per connection, to tell sockets apart.
    conn: u32,
    game_id: String,
    player: Option<usize>,
}

/// What is persisted for a game.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    game: Game,
    timer_at: Option<u64>,
    expires_at: u64,
}

#[durable_object]
pub struct GameRoom {
    state: State,
    /// `None` = not loaded from storage yet; `Some(None)` = no game.
    cache: RefCell<Option<Option<Record>>>,
}

impl GameRoom {
    async fn load(&self) -> Result<Option<Record>> {
        let cached = self.cache.borrow().clone();
        match cached {
            Some(r) => Ok(r),
            None => {
                // stored as JSON: the game contains u64s that JavaScript numbers cannot hold
                let json: Option<String> = self.state.storage().get(STORAGE_KEY).await?;
                let r: Option<Record> = json
                    .map(|j| serde_json::from_str(&j))
                    .transpose()
                    .map_err(|e| Error::RustError(e.to_string()))?;
                *self.cache.borrow_mut() = Some(r.clone());
                Ok(r)
            }
        }
    }

    /// Persist the record (or remove the game when `None`) and schedule the next alarm.
    async fn save(&self, record: Option<Record>) -> Result<()> {
        *self.cache.borrow_mut() = Some(record.clone());
        let storage = self.state.storage();
        match record {
            Some(r) => {
                let next = r.timer_at.map_or(r.expires_at, |t| t.min(r.expires_at));
                let json =
                    serde_json::to_string(&r).map_err(|e| Error::RustError(e.to_string()))?;
                storage.put(STORAGE_KEY, json).await?;
                storage
                    .set_alarm(ScheduledTime::from(next.saturating_sub(now_ms()) as i64))
                    .await?;
            }
            None => {
                storage.delete_alarm().await?;
                storage.delete_all().await?;
            }
        }
        Ok(())
    }

    fn sockets(&self) -> Vec<(WebSocket, Attachment)> {
        self.state
            .get_websockets()
            .into_iter()
            .filter_map(|ws| {
                let a = ws.deserialize_attachment::<Attachment>().ok().flatten()?;
                Some((ws, a))
            })
            .collect()
    }

    fn dispatch(&self, out: Outbox) {
        let sockets = self.sockets();
        for (to, msg) in &out.messages {
            for (ws, a) in &sockets {
                if a.player == Some(*to) {
                    send(ws, msg);
                }
            }
        }
    }

    /// Apply the outbox to the record: send messages, schedule timers and remove finished games.
    async fn commit(&self, mut record: Record, out: Outbox) -> Result<()> {
        if let Some(ms) = out.timer_ms {
            record.timer_at = Some(now_ms() + ms);
        }
        record.expires_at = now_ms() + EXPIRY_MS;
        let finished = record.game.is_finished();
        self.dispatch(out);
        self.save(if finished { None } else { Some(record) }).await
    }

    async fn handle_message(&self, ws: &WebSocket, text: &str) -> Result<()> {
        let Some(mut attachment) = ws.deserialize_attachment::<Attachment>()? else {
            return Ok(());
        };
        let msg: ClientMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                console_warn!("Client sent unknown message: {}; Error: {}", text, e);
                return Ok(());
            }
        };

        let record = self.load().await?;
        let mut out = Outbox::default();

        match msg {
            ClientMessage::NewGame {
                name,
                password,
                word_list,
                word_count,
            } => {
                if record.is_some() {
                    reject(ws, "the game id already exists");
                    return Ok(());
                }
                let mut game = Game::new(
                    attachment.game_id.clone(),
                    word_list,
                    word_count,
                    random_u64(),
                );
                let id = game
                    .join(name, password, &mut out)
                    .map_err(Error::RustError)?;
                attachment.player = Some(id);
                ws.serialize_attachment(&attachment)?;
                let record = Record {
                    game,
                    timer_at: None,
                    expires_at: 0,
                };
                self.commit(record, out).await
            }
            ClientMessage::JoinGame { name, password } => {
                let Some(mut record) = record else {
                    reject(ws, "the game id does not exist");
                    return Ok(());
                };
                match record.game.join(name, password, &mut out) {
                    Ok(id) => {
                        // a reconnecting player replaces their old connection
                        for (other, a) in self.sockets() {
                            if a.player == Some(id) && a.conn != attachment.conn {
                                let _ = other.close(Some(1000), Some("reconnected elsewhere"));
                            }
                        }
                        attachment.player = Some(id);
                        ws.serialize_attachment(&attachment)?;
                        self.commit(record, out).await
                    }
                    Err(reason) => {
                        reject(ws, &reason);
                        Ok(())
                    }
                }
            }
            msg => {
                let (Some(player), Some(mut record)) = (attachment.player, record) else {
                    return Ok(());
                };
                let game = &mut record.game;
                match msg {
                    ClientMessage::WordSubmission { word } => {
                        game.word_submission(player, word, &mut out)
                    }
                    ClientMessage::ManualDuplicateElimination { ids } => {
                        game.manual_duplicate_elimination(player, ids, &mut out)
                    }
                    ClientMessage::WordGuess { word } => game.word_guess(player, word, &mut out),
                    ClientMessage::AcceptGuess { accept } => {
                        game.accept_guess(player, accept, &mut out)
                    }
                    ClientMessage::StartGame => game.start_game(player, &mut out),
                    ClientMessage::NewGame { .. } | ClientMessage::JoinGame { .. } => {
                        unreachable!()
                    }
                }
                self.commit(record, out).await
            }
        }
    }
}

impl DurableObject for GameRoom {
    fn new(state: State, _env: Env) -> Self {
        // keepalive pings are answered without waking the object
        if let Ok(pair) = worker_sys::WebSocketRequestResponsePair::new("ping", "pong") {
            state.set_websocket_auto_response(&pair);
        }
        Self {
            state,
            cache: RefCell::new(None),
        }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let game_id = req
            .url()?
            .query_pairs()
            .find(|(k, _)| k == "game_id")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();

        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        pair.server.serialize_attachment(&Attachment {
            conn: (js_sys::Math::random() * 4294967295.0) as u32,
            game_id,
            player: None,
        })?;
        Response::from_websocket(pair.client)
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        match message {
            WebSocketIncomingMessage::String(text) => self.handle_message(&ws, &text).await,
            WebSocketIncomingMessage::Binary(_) => Ok(()),
        }
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        let _ = ws.close(Some(1000), Some("closing"));
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: Error) -> Result<()> {
        Ok(())
    }

    async fn alarm(&self) -> Result<Response> {
        let Some(mut record) = self.load().await? else {
            return Response::ok("");
        };
        let now = now_ms();

        if now >= record.expires_at {
            for (ws, _) in self.sockets() {
                let _ = ws.close(Some(1000), Some("game expired"));
            }
            self.save(None).await?;
            return Response::ok("");
        }

        if record.timer_at.is_some_and(|t| now >= t) {
            record.timer_at = None;
            let mut out = Outbox::default();
            record.game.timer(&mut out);
            self.commit(record, out).await?;
        } else {
            // spurious alarm: reschedule
            self.save(Some(record)).await?;
        }
        Response::ok("")
    }
}
