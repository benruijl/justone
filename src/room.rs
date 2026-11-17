//! The WebSocket protocol of a game room, shared by the Cloudflare Worker and the native server.
//!
//! A room is one game id. Each WebSocket connection belongs to one room and becomes a player
//! after a successful `NewGame` or `JoinGame`. The servers only move messages around: they call
//! [`handle_message`] for every text message and carry out the returned [`Response`].

use serde::Deserialize;

use crate::game::{Game, Outbox, ServerMessage, WordList};

/// Games are removed after this much inactivity.
pub const EXPIRY_MS: u64 = 90 * 60 * 1000;

/// Keepalive message from the client, answered with [`PONG`].
pub const PING: &str = "ping";
pub const PONG: &str = "pong";

/// Close reason for a connection whose player joined again elsewhere. The client does not
/// reconnect automatically when it sees this.
pub const RECONNECTED_ELSEWHERE: &str = "reconnected elsewhere";
/// Close reason for the connections of an expired game.
pub const GAME_EXPIRED: &str = "game expired";

pub const INVALID_GAME_ID: &str =
    "the game id may only contain letters, digits, - and _ (at most 32 characters)";

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
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

pub fn valid_game_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A short, readable game id from random bits (no easily confused characters).
pub fn game_id_from_random(mut bits: u64) -> String {
    const CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..5)
        .map(|_| {
            let c = CHARS[(bits % CHARS.len() as u64) as usize] as char;
            bits /= CHARS.len() as u64;
            c
        })
        .collect()
}

pub fn rejected(reason: &str) -> ServerMessage {
    ServerMessage::Rejected {
        reason: reason.to_string(),
    }
}

/// What the server has to do after a client message.
#[derive(Debug, Default)]
pub struct Response {
    /// Messages for the connection that sent the message.
    pub reply: Vec<ServerMessage>,
    /// Messages per player id, for every connection of that player.
    pub to_players: Vec<(usize, ServerMessage)>,
    /// Close the *other* connections of this player with [`RECONNECTED_ELSEWHERE`].
    pub replaces_player: Option<usize>,
    /// The game changed (or was removed): persist it and reset the expiry.
    pub changed: bool,
}

/// Handle a text message from a connection in room `game_id`.
///
/// `game` is the room's game (`None` when there is none) and `player` the player of the
/// connection, which is set when it joins. A finished game is removed (`game` becomes `None`).
/// `seed` is used for the word order of a new game.
pub fn handle_message(
    game: &mut Option<Game>,
    game_id: &str,
    player: &mut Option<usize>,
    text: &str,
    seed: u64,
) -> Response {
    let mut response = Response::default();
    let Ok(msg) = serde_json::from_str::<ClientMessage>(text) else {
        return response;
    };
    let mut out = Outbox::default();

    match msg {
        ClientMessage::NewGame {
            name,
            password,
            word_list,
            word_count,
        } => {
            if game.is_some() {
                response.reply.push(rejected("the game id already exists"));
                return response;
            }
            let mut new_game = Game::new(game_id.to_string(), word_list, word_count, seed);
            match new_game.join(name, password, &mut out) {
                Ok(id) => {
                    *player = Some(id);
                    *game = Some(new_game);
                }
                Err(reason) => {
                    response.reply.push(rejected(&reason));
                    return response;
                }
            }
        }
        ClientMessage::JoinGame { name, password } => {
            let Some(g) = game.as_mut() else {
                response.reply.push(rejected("the game id does not exist"));
                return response;
            };
            match g.join(name, password, &mut out) {
                Ok(id) => {
                    // a reconnecting player replaces their old connection
                    response.replaces_player = Some(id);
                    *player = Some(id);
                }
                Err(reason) => {
                    response.reply.push(rejected(&reason));
                    return response;
                }
            }
        }
        msg => {
            let (Some(p), Some(g)) = (*player, game.as_mut()) else {
                return response;
            };
            match msg {
                ClientMessage::WordSubmission { word } => g.word_submission(p, word, &mut out),
                ClientMessage::ManualDuplicateElimination { ids } => {
                    g.manual_duplicate_elimination(p, ids, &mut out)
                }
                ClientMessage::WordGuess { word } => g.word_guess(p, word, &mut out),
                ClientMessage::AcceptGuess { accept } => g.accept_guess(p, accept, &mut out),
                ClientMessage::StartGame => g.start_game(p, &mut out),
                ClientMessage::NewGame { .. } | ClientMessage::JoinGame { .. } => unreachable!(),
            }
        }
    }

    if game.as_ref().is_some_and(Game::is_finished) {
        *game = None;
    }
    response.to_players = out.messages;
    response.changed = true;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(game: &mut Option<Game>, player: &mut Option<usize>, msg: &str) -> Response {
        handle_message(game, "abc", player, msg, 7)
    }

    #[test]
    fn game_ids() {
        assert!(valid_game_id("my-game_1"));
        assert!(!valid_game_id(""));
        assert!(!valid_game_id("no spaces"));
        assert!(!valid_game_id(&"x".repeat(33)));
        let id = game_id_from_random(123456789);
        assert_eq!(id.len(), 5);
        assert!(valid_game_id(&id));
    }

    #[test]
    fn create_join_reconnect_and_finish() {
        let mut game = None;

        // joining a game that does not exist
        let mut p = None;
        let r = send(&mut game, &mut p, r#"{"type":"JoinGame","name":"a","password":"x"}"#);
        assert_eq!(r.reply, vec![rejected("the game id does not exist")]);
        assert!(!r.changed);

        let mut host = None;
        let r = send(
            &mut game,
            &mut host,
            r#"{"type":"NewGame","game_id":"","name":"Host","password":"pw","word_list":"english","word_count":1}"#,
        );
        assert!(r.changed && game.is_some());
        assert_eq!(host, Some(0));
        assert!(matches!(&r.to_players[0], (0, ServerMessage::UserAdded { game_id, .. }) if game_id == "abc"));

        let mut dup = None;
        let r = send(
            &mut game,
            &mut dup,
            r#"{"type":"NewGame","game_id":"abc","name":"X","password":"pw","word_list":"english","word_count":1}"#,
        );
        assert_eq!(r.reply, vec![rejected("the game id already exists")]);

        let mut anna = None;
        send(&mut game, &mut anna, r#"{"type":"JoinGame","name":"Anna","password":"a"}"#);
        assert_eq!(anna, Some(1));

        // reconnecting replaces the old connection
        let mut anna2 = None;
        let r = send(&mut game, &mut anna2, r#"{"type":"JoinGame","name":"Anna","password":"a"}"#);
        assert_eq!((anna2, r.replaces_player), (Some(1), Some(1)));

        // messages from connections that are not a player are ignored
        let mut nobody = None;
        assert!(!send(&mut game, &mut nobody, r#"{"type":"StartGame"}"#).changed);
        assert!(!send(&mut game, &mut host, "not json").changed);

        // play the single word: Anna guesses, the host gives a hint and judges
        send(&mut game, &mut host, r#"{"type":"StartGame"}"#);
        send(&mut game, &mut host, r#"{"type":"WordSubmission","word":"hint"}"#);
        send(&mut game, &mut host, r#"{"type":"ManualDuplicateElimination","ids":[]}"#);
        send(&mut game, &mut anna2, r#"{"type":"WordGuess","word":""}"#);
        assert!(game.is_none(), "a finished game is removed");
    }
}
