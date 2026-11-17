//! Cloudflare Worker entry point and the per-game Durable Object.
//!
//! The static page is served by Workers static assets (`public/`). The Worker only handles
//! `/chat?game_id=<id>` WebSocket upgrades and routes them to the Durable Object named after
//! the game id. The Durable Object uses the WebSocket hibernation API and persists the game
//! after every event, so it can be evicted from memory between moves.

use std::cell::RefCell;

use serde::{Deserialize, Serialize};
use worker::*;

use crate::game::{Game, ServerMessage};
use crate::room::{self, EXPIRY_MS};

const STORAGE_KEY: &str = "record";

fn now_ms() -> u64 {
    Date::now().as_millis()
}

fn random_u64() -> u64 {
    let hi = (js_sys::Math::random() * 4294967296.0) as u64;
    let lo = (js_sys::Math::random() * 4294967296.0) as u64;
    (hi << 32) | lo
}

fn send(ws: &WebSocket, msg: &ServerMessage) {
    if let Ok(text) = serde_json::to_string(msg) {
        // the socket may already be closed; nothing to do then
        let _ = ws.send_with_str(text);
    }
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
        room::game_id_from_random(random_u64())
    } else {
        requested
    };

    if !room::valid_game_id(&game_id) {
        let pair = WebSocketPair::new()?;
        pair.server.accept()?;
        send(&pair.server, &room::rejected(room::INVALID_GAME_ID));
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
                let next = r.expires_at;
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

    async fn handle_message(&self, ws: &WebSocket, text: &str) -> Result<()> {
        let Some(mut attachment) = ws.deserialize_attachment::<Attachment>()? else {
            return Ok(());
        };

        let mut game = self.load().await?.map(|r| r.game);
        let player_before = attachment.player;
        let response = room::handle_message(
            &mut game,
            &attachment.game_id,
            &mut attachment.player,
            text,
            random_u64(),
        );

        // the attachment decides who gets player messages, so update it first
        if attachment.player != player_before {
            ws.serialize_attachment(&attachment)?;
        }
        for msg in &response.reply {
            send(ws, msg);
        }

        let sockets = self.sockets();
        if let Some(p) = response.replaces_player {
            for (other, a) in &sockets {
                if a.player == Some(p) && a.conn != attachment.conn {
                    let _ = other.close(Some(1000), Some(room::RECONNECTED_ELSEWHERE));
                }
            }
        }
        for (to, msg) in &response.to_players {
            for (other, a) in &sockets {
                if a.player == Some(*to) {
                    send(other, msg);
                }
            }
        }

        if response.changed {
            self.save(game.map(|game| Record {
                game,
                expires_at: now_ms() + EXPIRY_MS,
            }))
            .await?;
        }
        Ok(())
    }
}

impl DurableObject for GameRoom {
    fn new(state: State, _env: Env) -> Self {
        // keepalive pings are answered without waking the object
        if let Ok(pair) = worker_sys::WebSocketRequestResponsePair::new(room::PING, room::PONG) {
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

    /// The only alarm is the expiry of an inactive game.
    async fn alarm(&self) -> Result<Response> {
        let Some(record) = self.load().await? else {
            return Response::ok("");
        };

        if now_ms() >= record.expires_at {
            for (ws, _) in self.sockets() {
                let _ = ws.close(Some(1000), Some(room::GAME_EXPIRED));
            }
            self.save(None).await?;
        } else {
            // activity moved the expiry: reschedule
            self.save(Some(record)).await?;
        }
        Response::ok("")
    }
}
