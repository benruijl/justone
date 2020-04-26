use rand::seq::SliceRandom;
use rand::thread_rng;
use std::fs::File;
use std::io::prelude::*;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::delay_for;

#[derive(Debug)]
enum State {
    WaitingForPlayers,
    WordSubmission,             // word submission by all users
    ManualDuplicateElimination, // duplicates that will be eliminated by a user
    WordGuess,
    Finished,
}

#[derive(Debug)]
pub enum ServerMessage {
    UserAdded(usize, String),                      // a new player has registered
    RoundStart(usize, usize, usize, usize, usize), // current player, decision player, score, words left
    WordSubmissionRequest(usize, String),
    ManualDuplicateEliminationRequest(usize),
    WordGuessRequest(usize),
    ShowWords(usize, Vec<Option<(String, bool)>>),
    ShowFilteredWords(usize, Vec<Option<String>>),
    ShowRoundOverview(usize, String, Vec<Option<(String, bool)>>),
    AcceptGuessRequest(usize),
    Finished(usize, usize),
    WordSubmitted(usize, usize),
}

#[derive(Debug)]
pub enum ClientMessage {
    NewPlayer(User),
    StartGame,
    ResendGameState(usize),
    WordSubmissionResponse(usize, String),
    ManualDuplicateEliminationResponse(usize, Vec<usize>),
    WordGuessResponse(usize, String),
    AcceptGuessResponse(usize, bool),
}

#[derive(Debug)]
pub struct User {
    pub name: String,
    pub word_guess: Option<String>,
    pub word_stricken: bool,
    pub channel_out: mpsc::UnboundedSender<ServerMessage>, // TODO: replace by id and have the logic have 1 out channel
}

pub struct Logic {
    state: State, // substate
    current_user: usize,
    decision_user: usize,
    update_out_channel: mpsc::UnboundedReceiver<ClientMessage>, // update from user
    word_to_guess: String,
    users: Vec<User>,
    words: Vec<String>,
    score: usize,
    words_left: usize,
}

impl Logic {
    pub async fn new_game_cli(num_users: usize) {
        // create a single listener on stdin to process all users
        let (in_channel_req, out_channel_req) = mpsc::unbounded_channel();
        let (in_channel, out_channel) = mpsc::unbounded_channel();
        tokio::spawn(JustOneCLI::process_input(in_channel));
        tokio::spawn(JustOneCLI::display_request(out_channel_req));

        let mut users = vec![];
        for i in 0..num_users {
            users.push(User {
                name: format!("User{}", i),
                channel_out: in_channel_req.clone(),
                word_stricken: false,
                word_guess: None,
            })
        }

        let words: Vec<String> =
            std::io::BufReader::new(File::open("include/words_en.txt").unwrap())
                .lines()
                .collect::<Result<_, _>>()
                .unwrap();

        // generate random word
        let mut rng = thread_rng();
        let word = words.choose(&mut rng).unwrap();

        let mut l = Logic {
            state: State::WordSubmission,
            users,
            update_out_channel: out_channel,
            current_user: 0,
            decision_user: 1,
            word_to_guess: word.clone(),
            score: 0,
            words_left: 13,
            words,
        };

        l.process_state().await
    }

    pub async fn new_game(update_out_channel: mpsc::UnboundedReceiver<ClientMessage>) {
        let words: Vec<String> =
            std::io::BufReader::new(File::open("include/words_en.txt").unwrap())
                .lines()
                .collect::<Result<_, _>>()
                .unwrap();

        let mut l = Logic {
            state: State::WaitingForPlayers,
            users: vec![],
            update_out_channel,
            current_user: 0,
            decision_user: 1,
            word_to_guess: String::new(),
            score: 0,
            words_left: 13,
            words,
        };

        l.process_state().await
    }

    pub async fn process_state(&mut self) {
        loop {
            match &mut self.state {
                State::Finished => {
                    // TODO: offer new game
                    info!("Game is over!");

                    for (i, u) in self.users.iter().enumerate() {
                        u.channel_out
                            .send(ServerMessage::Finished(i, self.score))
                            .unwrap();
                    }
                    break;
                }
                State::WaitingForPlayers => {
                    while let Some(res) = self.update_out_channel.recv().await {
                        match res {
                            ClientMessage::StartGame if self.users.len() > 1 => {
                                break;
                            }
                            ClientMessage::NewPlayer(u) => {
                                let new_name = u.name.clone();
                                info!("Added user {}", u.name);
                                // inform all other users
                                for (i, u) in self.users.iter().enumerate() {
                                    u.channel_out
                                        .send(ServerMessage::UserAdded(i, new_name.clone()))
                                        .unwrap();
                                }

                                self.users.push(u);

                                // inform the new user of all the users
                                for u_other in &self.users {
                                    self.users
                                        .last()
                                        .unwrap()
                                        .channel_out
                                        .send(ServerMessage::UserAdded(
                                            self.users.len() - 1,
                                            u_other.name.clone(),
                                        ))
                                        .unwrap();
                                }
                            }
                            _ => {
                                error!("Received unexpected message in {:?}: {:?}", self.state, res)
                            }
                        }
                    }

                    info!("Game is starting with {} players!", self.users.len());

                    for (i, u) in self.users.iter().enumerate() {
                        u.channel_out
                            .send(ServerMessage::RoundStart(
                                i,
                                self.current_user,
                                self.decision_user,
                                self.score,
                                self.words_left,
                            ))
                            .unwrap();
                    }

                    // generate random word
                    let mut rng = thread_rng();
                    let word = self.words.choose(&mut rng).unwrap();

                    self.word_to_guess = word.clone();
                    self.state = State::WordSubmission;
                }
                State::WordGuess => {
                    // send a word guess request to the current user
                    self.users[self.current_user]
                        .channel_out
                        .send(ServerMessage::WordGuessRequest(self.current_user))
                        .unwrap();

                    while let Some(res) = self.update_out_channel.recv().await {
                        match res {
                            ClientMessage::WordGuessResponse(i, w) if i == self.current_user => {
                                info!("Received word guess from User{}: {:?}", i, w);

                                let guess = w.to_string();
                                if guess.is_empty() {
                                    self.users[i].word_guess = None;
                                } else {
                                    self.users[i].word_guess = Some(w.to_string());
                                }
                                break;
                            }
                            _ => {
                                error!("Received unexpected message in {:?}: {:?}", self.state, res)
                            }
                        }
                    }

                    // send the final round overview to everyone and wait 5 seconds
                    let overview = self
                        .users
                        .iter()
                        .map(|u| u.word_guess.as_ref().map(|w| (w.clone(), u.word_stricken)))
                        .collect::<Vec<_>>();

                    for (i, u) in self.users.iter().enumerate() {
                        u.channel_out
                            .send(ServerMessage::ShowRoundOverview(
                                i,
                                self.word_to_guess.clone(),
                                overview.clone(),
                            ))
                            .unwrap();
                    }

                    if self.users[self.current_user].word_guess.is_none() {
                        delay_for(Duration::from_secs(10)).await;

                        // the player passed: forward to next round
                        self.words_left -= 1;
                    } else {
                        delay_for(Duration::from_secs(2)).await;

                        self.users[self.decision_user]
                            .channel_out
                            .send(ServerMessage::AcceptGuessRequest(self.decision_user))
                            .unwrap();

                        let mut accept = false;
                        while let Some(res) = self.update_out_channel.recv().await {
                            match res {
                                ClientMessage::AcceptGuessResponse(i, a)
                                    if i == self.decision_user =>
                                {
                                    info!("Received accept response from User{}: {:?}", i, a);
                                    accept = a;
                                    break;
                                }
                                _ => error!(
                                    "Received unexpected message in {:?}: {:?}",
                                    self.state, res
                                ),
                            }
                        }

                        if accept {
                            self.score += 1;
                            self.words_left -= 1;
                        } else {
                            if self.words_left == 1 {
                                self.words_left = 0;
                            } else {
                                self.words_left -= 2;
                            }
                        }
                    }

                    info!("Number of score: {}", self.score);
                    info!("Number of words left: {}", self.words_left);

                    if self.words_left == 0 {
                        // game is done!
                        self.state = State::Finished;
                    } else {
                        info!(
                            "Rotating the players: {} -> {}",
                            self.current_user,
                            (self.current_user + 1) % self.users.len()
                        );
                        self.current_user = (self.current_user + 1) % self.users.len();
                        self.decision_user = (self.current_user + 1) % self.users.len();

                        // TODO: move
                        for (i, u) in self.users.iter().enumerate() {
                            u.channel_out
                                .send(ServerMessage::RoundStart(
                                    i,
                                    self.current_user,
                                    self.decision_user,
                                    self.score,
                                    self.words_left,
                                ))
                                .unwrap();
                        }
                        // generate random word
                        let mut rng = thread_rng();
                        let word = self.words.choose(&mut rng).unwrap();
                        self.word_to_guess = word.clone();
                        self.state = State::WordSubmission;
                    }
                }
                State::WordSubmission => {
                    info!("New word: {}", self.word_to_guess);

                    // ask for words from other users
                    for (i, u) in self.users.iter_mut().enumerate() {
                        u.word_guess = None;
                        u.word_stricken = false;

                        if i != self.current_user {
                            info!("Submitting word request to user {}", i);
                            u.channel_out
                                .send(ServerMessage::WordSubmissionRequest(
                                    i,
                                    self.word_to_guess.to_string(),
                                ))
                                .unwrap();
                        }
                    }

                    while let Some(res) = self.update_out_channel.recv().await {
                        match res {
                            ClientMessage::WordSubmissionResponse(i, w)
                                if i != self.current_user =>
                            {
                                info!("Received word from User{}: {}", i, w);
                                self.users[i].word_guess = Some(w);

                                if self
                                    .users
                                    .iter()
                                    .enumerate()
                                    .all(|(i, u)| i == self.current_user || u.word_guess.is_some())
                                {
                                    break;
                                }

                                // notify all other users of the word submission
                                for (j, u) in self.users.iter().enumerate() {
                                    if i != j {
                                        u.channel_out
                                            .send(ServerMessage::WordSubmitted(j, i))
                                            .unwrap();
                                    }
                                }
                            }
                            _ => {
                                error!("Received unexpected message in {:?}: {:?}", self.state, res)
                            }
                        }
                    }

                    info!("All guesses received, time for manual duplication");
                    self.state = State::ManualDuplicateElimination;
                }
                State::ManualDuplicateElimination => {
                    // automatically filter exact duplicates
                    for i in 0..self.users.len() {
                        for j in 0..i {
                            if self.users[i].word_guess.as_ref().map_or(false, |w1| {
                                self.users[j]
                                    .word_guess
                                    .as_ref()
                                    .map_or(false, |w2| w1.eq_ignore_ascii_case(&w2))
                            }) {
                                self.users[i].word_stricken = true;
                                self.users[j].word_stricken = true;
                                break;
                            }
                        }
                    }

                    // send the words to everyone but the current user
                    let words: Vec<Option<(String, bool)>> = self
                        .users
                        .iter()
                        .map(|u| u.word_guess.as_ref().map(|x| (x.clone(), u.word_stricken)))
                        .collect();

                    for (i, u) in self.users.iter().enumerate() {
                        if i != self.current_user {
                            u.channel_out
                                .send(ServerMessage::ShowWords(i, words.clone()))
                                .unwrap();
                        }
                    }

                    self.users[self.decision_user]
                        .channel_out
                        .send(ServerMessage::ManualDuplicateEliminationRequest(
                            self.decision_user,
                        ))
                        .unwrap();

                    while let Some(res) = self.update_out_channel.recv().await {
                        match res {
                            ClientMessage::ManualDuplicateEliminationResponse(i, remove)
                                if i == self.decision_user =>
                            {
                                info!("Received elimination from User{}: {:?}", i, remove);

                                for uid in remove {
                                    self.users[uid].word_stricken = true;
                                }
                                break;
                            }
                            _ => {
                                error!("Received unexpected message in {:?}: {:?}", self.state, res)
                            }
                        }
                    }

                    // send the filtered words to everyone
                    let words: Vec<Option<String>> = self
                        .users
                        .iter()
                        .map(|u| {
                            if u.word_stricken {
                                &None
                            } else {
                                &u.word_guess
                            }
                        })
                        .cloned()
                        .collect();

                    for (i, u) in self.users.iter().enumerate() {
                        u.channel_out
                            .send(ServerMessage::ShowFilteredWords(i, words.clone()))
                            .unwrap();
                    }

                    self.state = State::WordGuess;
                }
            }
        }
    }
}

/// Command line interface version for testing
pub struct JustOneCLI {}

impl JustOneCLI {
    pub async fn process_input(
        out_channel_req: mpsc::UnboundedSender<ClientMessage>,
    ) -> Result<(), std::io::Error> {
        let mut process_input = BufReader::new(tokio::io::stdin()).lines();

        'read: while let Some(line) = process_input.next_line().await? {
            // get the user id and then parse the message
            info!("{} submitted", line);
            let cmd_split: Vec<&str> = line.split(" ").collect();

            if cmd_split.len() < 2 {
                error!("Wrong submission format: {}", line);
                continue;
            }

            let id = match cmd_split[0].parse::<usize>() {
                Ok(uid) => uid,
                Err(_) => {
                    error!("Wrong submission format: {}", line);
                    continue;
                }
            };

            match cmd_split[1] {
                "s" if cmd_split.len() == 3 => {
                    out_channel_req
                        .send(ClientMessage::WordSubmissionResponse(
                            id,
                            cmd_split[2].to_string(),
                        ))
                        .unwrap();
                }
                "a" if cmd_split.len() == 3 => match cmd_split[2].parse::<bool>() {
                    Ok(b) => {
                        out_channel_req
                            .send(ClientMessage::AcceptGuessResponse(id, b))
                            .unwrap();
                    }
                    Err(_) => {
                        error!("Wrong submission format: {}", line);
                        continue 'read;
                    }
                },
                "g" if cmd_split.len() == 3 => {
                    out_channel_req
                        .send(ClientMessage::WordGuessResponse(
                            id,
                            cmd_split[2].to_string(),
                        ))
                        .unwrap();
                }
                "r" if cmd_split.len() == 2 => {
                    out_channel_req
                        .send(ClientMessage::ResendGameState(id))
                        .unwrap();
                }
                "f" => {
                    let mut ids = vec![];
                    for x in &cmd_split[2..] {
                        let id = match x.parse::<usize>() {
                            Ok(uid) => uid,
                            Err(_) => {
                                error!("Wrong submission format: {}", line);
                                continue 'read;
                            }
                        };
                        ids.push(id);
                    }
                    out_channel_req
                        .send(ClientMessage::ManualDuplicateEliminationResponse(id, ids))
                        .unwrap();
                }
                _ => {
                    error!("Wrong submission format: {}", line);
                    continue 'read;
                }
            }
        }

        Ok(())
    }

    pub async fn display_request(
        mut out_channel_req: mpsc::UnboundedReceiver<ServerMessage>,
    ) -> Result<(), std::io::Error> {
        while let Some(req) = out_channel_req.recv().await {
            match req {
                ServerMessage::WordSubmitted(_i, _u) => {
                    // don't display it
                }
                ServerMessage::Finished(i, score) => {
                    println!("User{}: Game finished! Scored {} score", i, score)
                }
                ServerMessage::RoundStart(
                    i,
                    current_user,
                    decision_user,
                    score,
                    words_left,
                ) => println!(
                    "User{}: Game is starting, player {} is guessing and {} is deciding. score: {}, words to guess: {}",
                    i, current_user, decision_user, score, words_left
                ),
                ServerMessage::UserAdded(i, u) => println!("User{}: User added \"{}\"", i, u),
                ServerMessage::WordSubmissionRequest(i, w) => {
                    println!("User{}: Give a hint for the word \"{}\"", i, w)
                }
                ServerMessage::ManualDuplicateEliminationRequest(i) => {
                    println!("User{}: select which words to remove", i)
                }
                ServerMessage::ShowWords(i, w) => {
                    println!("User{}: received words", i);
                    for (ii, ww) in w.iter().enumerate() {
                        println!("\t User{} -> {:?}", ii, ww);
                    }
                }
                ServerMessage::ShowFilteredWords(i, w) => {
                    println!("User{}: received filter words:", i);
                    for (ii, ww) in w.iter().enumerate() {
                        println!("\t User{} -> {:?}", ii, ww);
                    }
                }
                ServerMessage::WordGuessRequest(i) => {
                    println!("User{}: guess the word:", i);
                }
                ServerMessage::ShowRoundOverview(i, word_to_guess, overview) => {
                    println!("User{}: word to guess: {}", i, word_to_guess);
                    for (ii, ww) in overview.iter().enumerate() {
                        println!("\t User{} -> {:?}", ii, ww);
                    }
                }
                ServerMessage::AcceptGuessRequest(i) => {
                    println!("User{}: accept guess?", i);
                }
            }
        }

        Ok(())
    }
}
