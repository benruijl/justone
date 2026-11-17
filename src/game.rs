//! The Just One game logic as an event-driven state machine.
//!
//! The game holds no I/O: every call takes an [`Outbox`] that collects the messages to send
//! to each player and an optional timer to schedule. This lets the same code run inside a
//! Cloudflare Durable Object (where state is persisted between events) and in native tests.

use serde::{Deserialize, Serialize};

/// How long the round overview is shown before the next round when the guesser passed.
pub const PASS_DELAY_MS: u64 = 10_000;
/// How long the round overview is shown before the decision player is asked to accept a guess.
pub const ACCEPT_DELAY_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WordList {
    #[serde(rename = "english")]
    English,
    #[serde(rename = "dutch")]
    Dutch,
    #[serde(rename = "hungarian")]
    Hungarian,
    #[serde(rename = "italian")]
    Italian,
    #[serde(rename = "german")]
    German,
    #[serde(rename = "french")]
    French,
}

impl WordList {
    pub fn words(self) -> Vec<&'static str> {
        let text = match self {
            WordList::Dutch => include_str!("../include/words_nl.txt"),
            WordList::English => include_str!("../include/words_en.txt"),
            WordList::French => include_str!("../include/words_fr.txt"),
            WordList::German => include_str!("../include/words_de.txt"),
            WordList::Hungarian => include_str!("../include/words_hu.txt"),
            WordList::Italian => include_str!("../include/words_it.txt"),
        };
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// A message to a client. Serializes to the JSON the web client expects.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// `id` is the id of the receiving player.
    UserAdded {
        username: String,
        id: usize,
        game_id: String,
    },
    RoundStart {
        current_user: usize,
        decision_user: usize,
        score: usize,
        words_left: usize,
    },
    WordSubmissionRequest {
        word: String,
    },
    WordSubmitted {
        user: usize,
    },
    ManualDuplicateEliminationRequest,
    ShowWords {
        words: Vec<Option<(String, bool)>>,
    },
    WordGuessRequest,
    ShowFilteredWords {
        words: Vec<Option<String>>,
    },
    ShowRoundOverview {
        word_to_guess: String,
        overview: Vec<Option<(String, bool)>>,
    },
    AcceptGuessRequest,
    Finished {
        points: usize,
    },
    Reconnecting {
        id: usize,
    },
    Rejected {
        reason: String,
    },
}

#[derive(Debug, Default)]
pub struct Outbox {
    /// Messages per receiving player id.
    pub messages: Vec<(usize, ServerMessage)>,
    /// Call [`Game::timer`] after this many milliseconds.
    pub timer_ms: Option<u64>,
}

impl Outbox {
    fn send(&mut self, to: usize, msg: ServerMessage) {
        self.messages.push((to, msg));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    WaitingForPlayers,
    WordSubmission,
    ManualDuplicateElimination,
    WordGuess,
    /// The round overview is shown. If the guesser made a guess, the decision player is asked
    /// to accept it once the timer fires (`decision_requested` becomes true).
    AcceptGuess {
        decision_requested: bool,
    },
    Finished,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub name: String,
    pub password: String,
    pub word_guess: Option<String>,
    pub word_stricken: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub game_id: String,
    pub state: State,
    pub players: Vec<Player>,
    guess_user: usize,
    decision_user: usize,
    word_list: WordList,
    used_words: Vec<usize>,
    word_to_guess: String,
    score: usize,
    words_left: usize,
    rng: u64,
}

impl Game {
    pub fn new(game_id: String, word_list: WordList, word_count: usize, seed: u64) -> Self {
        Game {
            game_id,
            state: State::WaitingForPlayers,
            players: vec![],
            guess_user: 0,
            decision_user: 1,
            word_list,
            used_words: vec![],
            word_to_guess: String::new(),
            score: 0,
            words_left: word_count.clamp(1, 1000),
            rng: seed | 1,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.state == State::Finished
    }

    fn random(&mut self, n: usize) -> usize {
        // xorshift64
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % n as u64) as usize
    }

    /// Add a player, or reconnect a player with the same name and password.
    /// Returns the player id or the reason for rejection.
    pub fn join(
        &mut self,
        name: String,
        password: String,
        out: &mut Outbox,
    ) -> Result<usize, String> {
        if let Some(i) = self.players.iter().position(|p| p.name == name) {
            if self.players[i].password != password {
                return Err("the password does not match the existing player's".into());
            }
            out.send(i, ServerMessage::Reconnecting { id: i });
            self.send_full_game_state(i, out);
            return Ok(i);
        }

        if self.state != State::WaitingForPlayers {
            return Err("the game has already started".into());
        }

        // inform all other players
        for i in 0..self.players.len() {
            out.send(i, self.user_added(i, &name));
        }

        self.players.push(Player {
            name,
            password,
            word_guess: None,
            word_stricken: false,
        });

        // inform the new player of all the players
        let id = self.players.len() - 1;
        for p in &self.players {
            out.send(id, self.user_added(id, &p.name));
        }

        Ok(id)
    }

    fn user_added(&self, to: usize, name: &str) -> ServerMessage {
        ServerMessage::UserAdded {
            username: name.to_string(),
            id: to,
            game_id: self.game_id.clone(),
        }
    }

    /// The host (player 0) starts the game.
    pub fn start_game(&mut self, player: usize, out: &mut Outbox) {
        if player == 0 && self.state == State::WaitingForPlayers && self.players.len() > 1 {
            self.next_round(out);
        }
    }

    pub fn word_submission(&mut self, player: usize, word: String, out: &mut Outbox) {
        if self.state != State::WordSubmission
            || player == self.guess_user
            || player >= self.players.len()
        {
            return;
        }

        self.players[player].word_guess = Some(word);

        if self
            .players
            .iter()
            .enumerate()
            .all(|(i, p)| i == self.guess_user || p.word_guess.is_some())
        {
            self.start_duplicate_elimination(out);
            return;
        }

        // notify all other players of the word submission
        for j in 0..self.players.len() {
            if j != player {
                out.send(j, ServerMessage::WordSubmitted { user: player });
            }
        }
    }

    pub fn manual_duplicate_elimination(
        &mut self,
        player: usize,
        ids: Vec<usize>,
        out: &mut Outbox,
    ) {
        if self.state != State::ManualDuplicateElimination || player != self.decision_user {
            return;
        }

        for id in ids {
            if let Some(p) = self.players.get_mut(id) {
                p.word_stricken = true;
            }
        }

        self.start_word_guess(out);
    }

    /// An empty word means the guesser passes.
    pub fn word_guess(&mut self, player: usize, word: String, out: &mut Outbox) {
        if self.state != State::WordGuess || player != self.guess_user {
            return;
        }

        self.players[player].word_guess = if word.is_empty() { None } else { Some(word) };

        self.state = State::AcceptGuess {
            decision_requested: false,
        };

        let overview = self.overview();
        for i in 0..self.players.len() {
            out.send(
                i,
                ServerMessage::ShowRoundOverview {
                    word_to_guess: self.word_to_guess.clone(),
                    overview: overview.clone(),
                },
            );
        }

        out.timer_ms = Some(if self.players[self.guess_user].word_guess.is_none() {
            PASS_DELAY_MS
        } else {
            ACCEPT_DELAY_MS
        });
    }

    /// Called when the timer requested through [`Outbox::timer_ms`] fires.
    pub fn timer(&mut self, out: &mut Outbox) {
        if self.state
            != (State::AcceptGuess {
                decision_requested: false,
            })
        {
            return;
        }

        let guess = self.players[self.guess_user].word_guess.as_deref();
        if guess.is_none() {
            // the player passed
            self.words_left -= 1;
            self.end_round(out);
        } else if guess
            .is_some_and(|g| g.trim().to_lowercase() == self.word_to_guess.trim().to_lowercase())
        {
            // an exact match needs no decision
            self.score += 1;
            self.words_left -= 1;
            self.end_round(out);
        } else {
            self.state = State::AcceptGuess {
                decision_requested: true,
            };
            out.send(self.decision_user, ServerMessage::AcceptGuessRequest);
        }
    }

    pub fn accept_guess(&mut self, player: usize, accept: bool, out: &mut Outbox) {
        if self.state
            != (State::AcceptGuess {
                decision_requested: true,
            })
            || player != self.decision_user
        {
            return;
        }

        if accept {
            self.score += 1;
            self.words_left -= 1;
        } else {
            // a wrong guess costs the current and the next card
            self.words_left = self.words_left.saturating_sub(2);
        }

        self.end_round(out);
    }

    fn end_round(&mut self, out: &mut Outbox) {
        if self.words_left == 0 {
            self.state = State::Finished;
            for i in 0..self.players.len() {
                out.send(i, ServerMessage::Finished { points: self.score });
            }
        } else {
            self.next_round(out);
        }
    }

    fn next_round(&mut self, out: &mut Outbox) {
        let n = self.players.len();
        self.guess_user = (self.guess_user + 1) % n;
        self.decision_user = (self.guess_user + 1) % n;

        for i in 0..n {
            out.send(i, self.round_start());
        }

        // pick a random word that has not been used in this game yet
        let words = self.word_list.words();
        if self.used_words.len() >= words.len() {
            self.used_words.clear();
        }
        let mut index = self.random(words.len() - self.used_words.len());
        let mut used = self.used_words.clone();
        used.sort_unstable();
        for u in used {
            if u <= index {
                index += 1;
            }
        }
        self.used_words.push(index);
        self.word_to_guess = words[index].to_string();

        self.state = State::WordSubmission;
        for (i, p) in self.players.iter_mut().enumerate() {
            p.word_guess = None;
            p.word_stricken = false;
            if i != self.guess_user {
                out.send(
                    i,
                    ServerMessage::WordSubmissionRequest {
                        word: self.word_to_guess.clone(),
                    },
                );
            }
        }
    }

    fn start_duplicate_elimination(&mut self, out: &mut Outbox) {
        self.state = State::ManualDuplicateElimination;

        // automatically strike exact duplicates
        for i in 0..self.players.len() {
            for j in 0..i {
                let duplicate = match (&self.players[i].word_guess, &self.players[j].word_guess) {
                    (Some(w1), Some(w2)) => w1.eq_ignore_ascii_case(w2),
                    _ => false,
                };
                if duplicate {
                    self.players[i].word_stricken = true;
                    self.players[j].word_stricken = true;
                    break;
                }
            }
        }

        let words = self.overview();
        for i in 0..self.players.len() {
            if i != self.guess_user {
                out.send(
                    i,
                    ServerMessage::ShowWords {
                        words: words.clone(),
                    },
                );
            }
        }
        out.send(
            self.decision_user,
            ServerMessage::ManualDuplicateEliminationRequest,
        );
    }

    fn start_word_guess(&mut self, out: &mut Outbox) {
        self.state = State::WordGuess;

        let words = self.filtered_words();
        for i in 0..self.players.len() {
            out.send(
                i,
                ServerMessage::ShowFilteredWords {
                    words: words.clone(),
                },
            );
        }
        out.send(self.guess_user, ServerMessage::WordGuessRequest);
    }

    fn round_start(&self) -> ServerMessage {
        ServerMessage::RoundStart {
            current_user: self.guess_user,
            decision_user: self.decision_user,
            score: self.score,
            words_left: self.words_left,
        }
    }

    fn overview(&self) -> Vec<Option<(String, bool)>> {
        self.players
            .iter()
            .map(|p| p.word_guess.clone().map(|w| (w, p.word_stricken)))
            .collect()
    }

    fn filtered_words(&self) -> Vec<Option<String>> {
        self.players
            .iter()
            .map(|p| {
                if p.word_stricken {
                    None
                } else {
                    p.word_guess.clone()
                }
            })
            .collect()
    }

    /// Send the complete game state to a player who has rejoined.
    pub fn send_full_game_state(&self, user: usize, out: &mut Outbox) {
        for p in &self.players {
            out.send(user, self.user_added(user, &p.name));
        }

        if self.state != State::WaitingForPlayers && self.state != State::Finished {
            out.send(user, self.round_start());
        }

        match self.state {
            State::WaitingForPlayers | State::Finished => {}
            State::WordSubmission => {
                if user != self.guess_user {
                    out.send(
                        user,
                        ServerMessage::WordSubmissionRequest {
                            word: self.word_to_guess.clone(),
                        },
                    );
                }

                // replay the submissions so far
                for (j, p) in self.players.iter().enumerate() {
                    if p.word_guess.is_some() {
                        out.send(user, ServerMessage::WordSubmitted { user: j });
                    }
                }
            }
            State::ManualDuplicateElimination => {
                if user != self.guess_user {
                    out.send(
                        user,
                        ServerMessage::ShowWords {
                            words: self.overview(),
                        },
                    );
                }
                if user == self.decision_user {
                    out.send(user, ServerMessage::ManualDuplicateEliminationRequest);
                }
            }
            State::WordGuess => {
                out.send(
                    user,
                    ServerMessage::ShowFilteredWords {
                        words: self.filtered_words(),
                    },
                );
                if user == self.guess_user {
                    out.send(user, ServerMessage::WordGuessRequest);
                }
            }
            State::AcceptGuess { decision_requested } => {
                out.send(
                    user,
                    ServerMessage::ShowRoundOverview {
                        word_to_guess: self.word_to_guess.clone(),
                        overview: self.overview(),
                    },
                );
                if decision_requested && user == self.decision_user {
                    out.send(user, ServerMessage::AcceptGuessRequest);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game_with(n: usize, words: usize) -> Game {
        let mut g = Game::new("abc".into(), WordList::English, words, 42);
        for i in 0..n {
            g.join(format!("p{}", i), "pw".into(), &mut Outbox::default())
                .unwrap();
        }
        g
    }

    fn to(out: &Outbox, player: usize) -> Vec<&ServerMessage> {
        out.messages
            .iter()
            .filter(|(p, _)| *p == player)
            .map(|(_, m)| m)
            .collect()
    }

    #[test]
    fn json_matches_client_protocol() {
        let m = ServerMessage::ShowWords {
            words: vec![Some(("a".into(), true)), None],
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"type":"ShowWords","words":[["a",true],null]}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMessage::WordGuessRequest).unwrap(),
            r#"{"type":"WordGuessRequest"}"#
        );
    }

    #[test]
    fn word_lists_load() {
        for l in [
            WordList::English,
            WordList::Dutch,
            WordList::French,
            WordList::German,
            WordList::Hungarian,
            WordList::Italian,
        ] {
            assert!(l.words().len() > 50, "{:?}", l);
        }
    }

    #[test]
    fn join_and_reconnect() {
        let mut g = game_with(2, 13);
        let mut out = Outbox::default();
        assert_eq!(g.join("p3".into(), "x".into(), &mut out), Ok(2));
        // existing players are told, the new one gets the full list
        assert_eq!(to(&out, 0).len(), 1);
        assert_eq!(to(&out, 2).len(), 3);

        assert!(g
            .join("p0".into(), "wrong".into(), &mut Outbox::default())
            .is_err());

        g.start_game(0, &mut Outbox::default());
        assert!(g
            .join("new".into(), "pw".into(), &mut Outbox::default())
            .is_err());

        let mut out = Outbox::default();
        assert_eq!(g.join("p0".into(), "pw".into(), &mut out), Ok(0));
        assert_eq!(out.messages[0].1, ServerMessage::Reconnecting { id: 0 });
        assert!(
            to(&out, 0).contains(&&ServerMessage::WordSubmissionRequest {
                word: g.word_to_guess.clone()
            })
        );
    }

    #[test]
    fn only_host_starts_with_two_players() {
        let mut g = game_with(1, 13);
        g.start_game(0, &mut Outbox::default());
        assert_eq!(g.state, State::WaitingForPlayers);
        g.join("p1".into(), "pw".into(), &mut Outbox::default())
            .unwrap();
        g.start_game(1, &mut Outbox::default());
        assert_eq!(g.state, State::WaitingForPlayers);
        g.start_game(0, &mut Outbox::default());
        assert_eq!(g.state, State::WordSubmission);
        assert_eq!((g.guess_user, g.decision_user), (1, 0));
    }

    #[test]
    fn full_round() {
        let mut g = game_with(4, 3);
        g.start_game(0, &mut Outbox::default());
        let guesser = g.guess_user;
        let decider = g.decision_user;
        let others: Vec<usize> = (0..4).filter(|&i| i != guesser).collect();

        // the guesser cannot submit a hint
        g.word_submission(guesser, "x".into(), &mut Outbox::default());
        assert!(g.players[guesser].word_guess.is_none());

        g.word_submission(others[0], "Same".into(), &mut Outbox::default());
        g.word_submission(others[1], "same".into(), &mut Outbox::default());
        let mut out = Outbox::default();
        g.word_submission(others[2], "unique".into(), &mut out);
        assert_eq!(g.state, State::ManualDuplicateElimination);
        assert!(g.players[others[0]].word_stricken && g.players[others[1]].word_stricken);
        assert!(!g.players[others[2]].word_stricken);
        assert!(to(&out, guesser).is_empty());
        assert!(to(&out, decider).contains(&&ServerMessage::ManualDuplicateEliminationRequest));

        // out-of-range ids are ignored
        g.manual_duplicate_elimination(decider, vec![99], &mut Outbox::default());
        assert_eq!(g.state, State::WordGuess);
        assert_eq!(g.filtered_words().iter().filter(|w| w.is_some()).count(), 1);

        let mut out = Outbox::default();
        g.word_guess(guesser, "guess".into(), &mut out);
        assert_eq!(out.timer_ms, Some(ACCEPT_DELAY_MS));
        // accepting before the request is ignored
        g.accept_guess(decider, true, &mut Outbox::default());
        assert_eq!(g.score, 0);

        let mut out = Outbox::default();
        g.timer(&mut out);
        assert_eq!(
            out.messages,
            vec![(decider, ServerMessage::AcceptGuessRequest)]
        );
        g.accept_guess(decider, true, &mut Outbox::default());
        assert_eq!((g.score, g.words_left), (1, 2));
        assert_eq!(g.state, State::WordSubmission);
        assert_eq!(g.guess_user, (guesser + 1) % 4);
    }

    #[test]
    fn pass_and_reject_until_finished() {
        let mut g = game_with(2, 3);
        g.start_game(0, &mut Outbox::default());

        let play = |g: &mut Game, guess: &str| {
            let hinter = 1 - g.guess_user;
            g.word_submission(hinter, "hint".into(), &mut Outbox::default());
            g.manual_duplicate_elimination(g.decision_user, vec![], &mut Outbox::default());
            let mut out = Outbox::default();
            g.word_guess(g.guess_user, guess.into(), &mut out);
            out.timer_ms
        };

        // pass: costs one word after the long delay
        assert_eq!(play(&mut g, ""), Some(PASS_DELAY_MS));
        g.timer(&mut Outbox::default());
        assert_eq!(g.words_left, 2);

        // wrong guess costs two words and ends the game
        play(&mut g, "nope");
        g.timer(&mut Outbox::default());
        let mut out = Outbox::default();
        g.accept_guess(g.decision_user, false, &mut out);
        assert!(g.is_finished());
        assert_eq!(to(&out, 0), vec![&ServerMessage::Finished { points: 0 }]);

        // a stale timer does nothing
        g.timer(&mut Outbox::default());
        assert!(g.is_finished());
    }

    #[test]
    fn exact_guess_is_accepted_automatically() {
        let mut g = game_with(2, 3);
        g.start_game(0, &mut Outbox::default());
        g.word_submission(1 - g.guess_user, "hint".into(), &mut Outbox::default());
        g.manual_duplicate_elimination(g.decision_user, vec![], &mut Outbox::default());
        let guess = format!(" {} ", g.word_to_guess.to_uppercase());
        g.word_guess(g.guess_user, guess, &mut Outbox::default());

        let mut out = Outbox::default();
        g.timer(&mut out);
        assert!(!out
            .messages
            .iter()
            .any(|(_, m)| *m == ServerMessage::AcceptGuessRequest));
        assert_eq!((g.score, g.words_left), (1, 2));
        assert_eq!(g.state, State::WordSubmission);
    }

    #[test]
    fn words_do_not_repeat() {
        let mut g = game_with(2, 1000);
        let total = WordList::English.words().len();
        g.start_game(0, &mut Outbox::default());
        for _ in 1..total {
            g.next_round(&mut Outbox::default());
        }
        let mut used = g.used_words.clone();
        used.sort_unstable();
        assert_eq!(used, (0..total).collect::<Vec<_>>());

        // once exhausted, the list starts over
        g.next_round(&mut Outbox::default());
        assert_eq!(g.used_words.len(), 1);
    }
}
