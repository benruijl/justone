use rand::seq::SliceRandom;
use rand::thread_rng;
use std::fs::File;
use std::io::prelude::*;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::delay_for;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub enum WordList {
    #[serde(rename = "english")]
    English,
    #[serde(rename = "hungarian")]
    Hungarian,
    #[serde(rename = "italian")]
    Italian,
    #[serde(rename = "german")]
    German,
}

#[derive(Debug, PartialEq)]
enum State {
    WaitingForPlayers,
    NextRound,
    WordSubmission,             // word submission by all users
    ManualDuplicateElimination, // duplicates that will be eliminated by a user
    WordGuess,
    AcceptGuess,
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
    NewPlayer(String),
    StartGame,
    ResendGameState(usize),
    WordSubmissionResponse(usize, String),
    ManualDuplicateEliminationResponse(usize, Vec<usize>),
    WordGuessResponse(usize, String),
    AcceptGuessResponse(usize, bool),
    StopGame,
}

#[derive(Debug)]
pub struct Player {
    pub name: String,
    pub word_guess: Option<String>,
    pub word_stricken: bool,
}

pub struct JustOneGame {
    state: State,
    guess_user: usize,
    decision_user: usize,
    client_msg_rx: mpsc::UnboundedReceiver<ClientMessage>,
    pub server_msg_tx: mpsc::UnboundedSender<ServerMessage>,
    word_to_guess: String,
    users: Vec<Player>,
    words: Vec<String>,
    score: usize,
    words_left: usize,
}

impl JustOneGame {
    pub fn read_word_list(word_list: WordList) -> Vec<String> {
        let filename = match word_list {
            WordList::English => "include/words_en.txt",
            WordList::Hungarian => "include/words_hu.txt",
            WordList::Italian => "include/words_it.txt",
            WordList::German => "include/words_de.txt",
        };

        std::io::BufReader::new(File::open(filename).expect("Cannot open word list"))
            .lines()
            .collect::<Result<_, _>>()
            .expect("Cannot read lines from word list")
    }

    pub async fn new_game_cli(num_users: usize, word_list: WordList) {
        // create a single listener on stdin to process all users
        let (in_channel_req, out_channel_req) = mpsc::unbounded_channel();
        let (in_channel, out_channel) = mpsc::unbounded_channel();
        tokio::spawn(JustOneCLI::process_input(in_channel));
        tokio::spawn(JustOneCLI::display_request(out_channel_req));

        let mut users = vec![];
        for i in 0..num_users {
            users.push(Player {
                name: format!("User{}", i),
                word_stricken: false,
                word_guess: None,
            })
        }

        let words = JustOneGame::read_word_list(word_list);

        // generate random word
        let mut rng = thread_rng();
        let word = words.choose(&mut rng).expect("Word list is empty!");

        let mut l = JustOneGame {
            state: State::WordSubmission,
            users,
            client_msg_rx: out_channel,
            server_msg_tx: in_channel_req,
            guess_user: 0,
            decision_user: 1,
            word_to_guess: word.clone(),
            score: 0,
            words_left: 13,
            words,
        };

        l.process_state().await
    }

    pub async fn new_game(
        client_msg_rx: mpsc::UnboundedReceiver<ClientMessage>,
        server_msg_tx: mpsc::UnboundedSender<ServerMessage>,
        word_list: WordList,
        word_count: usize,
    ) {
        let words = JustOneGame::read_word_list(word_list);

        let mut l = JustOneGame {
            state: State::WaitingForPlayers,
            users: vec![],
            client_msg_rx,
            server_msg_tx,
            guess_user: 0,
            decision_user: 1,
            word_to_guess: String::new(),
            score: 0,
            words_left: word_count,
            words,
        };

        l.process_state().await
    }

    /// Send the complete game state to a user (who has rejoined).
    pub fn send_full_game_state(&self, user: usize) {
        // send the list of users
        for u in &self.users {
            if let Err(e) = self
                .server_msg_tx
                .send(ServerMessage::UserAdded(user, u.name.clone()))
            {
                error!("Could not send server message to middleware: {}", e);
            }
        }

        if self.state != State::WaitingForPlayers && self.state != State::Finished {
            if let Err(e) = self.server_msg_tx.send(ServerMessage::RoundStart(
                user,
                self.guess_user,
                self.decision_user,
                self.score,
                self.words_left,
            )) {
                error!("Could not send server message to middleware: {}", e);
            }
        }

        match &self.state {
            State::WaitingForPlayers => {}
            State::NextRound => {}
            State::WordSubmission => {
                if user != self.guess_user {
                    info!("Submitting word request to user {}", user);
                    if let Err(e) = self
                        .server_msg_tx
                        .send(ServerMessage::WordSubmissionRequest(
                            user,
                            self.word_to_guess.to_string(),
                        ))
                    {
                        error!("Could not send server message to middleware: {}", e);
                    }
                }

                // now replay all events of people who have submitted already
                for (j, u) in self.users.iter().enumerate() {
                    if u.word_guess.is_some() {
                        if let Err(e) = self
                            .server_msg_tx
                            .send(ServerMessage::WordSubmitted(user, j))
                        {
                            error!("Could not send server message to middleware: {}", e);
                        }
                    }
                }
            }
            State::ManualDuplicateElimination => {
                // this state is reached when the player has not submitted the duplication response yet

                // send the words to everyone but the guess user
                let words: Vec<Option<(String, bool)>> = self
                    .users
                    .iter()
                    .map(|u| u.word_guess.as_ref().map(|x| (x.clone(), u.word_stricken)))
                    .collect();

                if user != self.guess_user {
                    if let Err(e) = self
                        .server_msg_tx
                        .send(ServerMessage::ShowWords(user, words.clone()))
                    {
                        error!("Could not send server message to middleware: {}", e);
                    }
                }

                if user == self.decision_user {
                    if let Err(e) =
                        self.server_msg_tx
                            .send(ServerMessage::ManualDuplicateEliminationRequest(
                                self.decision_user,
                            ))
                    {
                        error!("Could not send server message to middleware: {}", e);
                    }
                }
            }
            State::WordGuess => {
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
                if let Err(e) = self
                    .server_msg_tx
                    .send(ServerMessage::ShowFilteredWords(user, words.clone()))
                {
                    error!("Could not send server message to middleware: {}", e);
                }

                if user == self.guess_user {
                    if let Err(e) = self
                        .server_msg_tx
                        .send(ServerMessage::WordGuessRequest(self.guess_user))
                    {
                        error!("Could not send server message to middleware: {}", e);
                    }
                }
            }
            State::AcceptGuess => {
                // send the filtered words to everyone
                let overview = self
                    .users
                    .iter()
                    .map(|u| u.word_guess.as_ref().map(|w| (w.clone(), u.word_stricken)))
                    .collect::<Vec<_>>();

                if let Err(e) = self.server_msg_tx.send(ServerMessage::ShowRoundOverview(
                    user,
                    self.word_to_guess.clone(),
                    overview.clone(),
                )) {
                    error!("Could not send server message to middleware: {}", e);
                }

                if self.users[self.guess_user].word_guess.is_some() && user == self.decision_user {
                    if let Err(e) = self
                        .server_msg_tx
                        .send(ServerMessage::AcceptGuessRequest(self.decision_user))
                    {
                        error!("Could not send server message to middleware: {}", e);
                    }
                }
            }
            State::Finished => {
                // the game will be shut down imminently and this state will not be triggered
            }
        }
    }

    pub async fn process_state(&mut self) {
        'main_loop: loop {
            match &mut self.state {
                State::WaitingForPlayers => {
                    while let Some(res) = self.client_msg_rx.recv().await {
                        match res {
                            ClientMessage::ResendGameState(i) => {
                                self.send_full_game_state(i);
                            }
                            ClientMessage::StopGame => {
                                break 'main_loop;
                            }
                            ClientMessage::StartGame if self.users.len() > 1 => {
                                break;
                            }
                            ClientMessage::NewPlayer(name) => {
                                info!("Added user {}", name);
                                // inform all other users
                                for i in 0..self.users.len() {
                                    if let Err(e) = self
                                        .server_msg_tx
                                        .send(ServerMessage::UserAdded(i, name.clone()))
                                    {
                                        error!(
                                            "Could not send server message to middleware: {}",
                                            e
                                        );
                                        break 'main_loop;
                                    }
                                }

                                self.users.push(Player {
                                    name,
                                    word_guess: None,
                                    word_stricken: false,
                                });

                                // inform the new user of all the users
                                for u_other in &self.users {
                                    if let Err(e) =
                                        self.server_msg_tx.send(ServerMessage::UserAdded(
                                            self.users.len() - 1,
                                            u_other.name.clone(),
                                        ))
                                    {
                                        error!(
                                            "Could not send server message to middleware: {}",
                                            e
                                        );
                                        break 'main_loop;
                                    }
                                }
                            }
                            _ => {
                                error!("Received unexpected message in {:?}: {:?}", self.state, res)
                            }
                        }
                    }

                    info!("Game is starting with {} players!", self.users.len());
                    self.state = State::NextRound;
                }
                State::NextRound => {
                    info!(
                        "Rotating the players: {} -> {}",
                        self.guess_user,
                        (self.guess_user + 1) % self.users.len()
                    );
                    self.guess_user = (self.guess_user + 1) % self.users.len();
                    self.decision_user = (self.guess_user + 1) % self.users.len();

                    for i in 0..self.users.len() {
                        if let Err(e) = self.server_msg_tx.send(ServerMessage::RoundStart(
                            i,
                            self.guess_user,
                            self.decision_user,
                            self.score,
                            self.words_left,
                        )) {
                            error!("Could not send server message to middleware: {}", e);
                            break 'main_loop;
                        }
                    }

                    // generate random word
                    // TODO: filter words selected before
                    let mut rng = thread_rng();
                    let word = self.words.choose(&mut rng).expect("Word list is empty!");

                    self.word_to_guess = word.clone();
                    self.state = State::WordSubmission;
                }
                State::WordSubmission => {
                    info!("New word: {}", self.word_to_guess);

                    // ask for words from other users
                    for (i, u) in self.users.iter_mut().enumerate() {
                        u.word_guess = None;
                        u.word_stricken = false;

                        if i != self.guess_user {
                            info!("Submitting word request to user {}", i);
                            if let Err(e) =
                                self.server_msg_tx
                                    .send(ServerMessage::WordSubmissionRequest(
                                        i,
                                        self.word_to_guess.to_string(),
                                    ))
                            {
                                error!("Could not send server message to middleware: {}", e);
                                break 'main_loop;
                            }
                        }
                    }

                    while let Some(res) = self.client_msg_rx.recv().await {
                        match res {
                            ClientMessage::ResendGameState(i) => {
                                self.send_full_game_state(i);
                            }
                            ClientMessage::StopGame => {
                                break 'main_loop;
                            }
                            ClientMessage::WordSubmissionResponse(i, w) if i != self.guess_user => {
                                info!("Received word from User{}: {}", i, w);
                                self.users[i].word_guess = Some(w);

                                if self
                                    .users
                                    .iter()
                                    .enumerate()
                                    .all(|(i, u)| i == self.guess_user || u.word_guess.is_some())
                                {
                                    break;
                                }

                                // notify all other users of the word submission
                                for j in 0..self.users.len() {
                                    if i != j {
                                        if let Err(e) = self
                                            .server_msg_tx
                                            .send(ServerMessage::WordSubmitted(j, i))
                                        {
                                            error!(
                                                "Could not send server message to middleware: {}",
                                                e
                                            );
                                            break 'main_loop;
                                        }
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

                    for i in 0..self.users.len() {
                        if i != self.guess_user {
                            if let Err(e) = self
                                .server_msg_tx
                                .send(ServerMessage::ShowWords(i, words.clone()))
                            {
                                error!("Could not send server message to middleware: {}", e);
                                break 'main_loop;
                            }
                        }
                    }

                    if let Err(e) =
                        self.server_msg_tx
                            .send(ServerMessage::ManualDuplicateEliminationRequest(
                                self.decision_user,
                            ))
                    {
                        error!("Could not send server message to middleware: {}", e);
                        break 'main_loop;
                    }

                    while let Some(res) = self.client_msg_rx.recv().await {
                        match res {
                            ClientMessage::ResendGameState(i) => {
                                self.send_full_game_state(i);
                            }
                            ClientMessage::StopGame => {
                                break 'main_loop;
                            }
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

                    self.state = State::WordGuess;
                }
                State::WordGuess => {
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

                    for i in 0..self.users.len() {
                        if let Err(e) = self
                            .server_msg_tx
                            .send(ServerMessage::ShowFilteredWords(i, words.clone()))
                        {
                            error!("Could not send server message to middleware: {}", e);
                            break 'main_loop;
                        }
                    }

                    // send a word guess request to the current user
                    if let Err(e) = self
                        .server_msg_tx
                        .send(ServerMessage::WordGuessRequest(self.guess_user))
                    {
                        error!("Could not send server message to middleware: {}", e);
                        break 'main_loop;
                    }

                    while let Some(res) = self.client_msg_rx.recv().await {
                        match res {
                            ClientMessage::ResendGameState(i) => {
                                self.send_full_game_state(i);
                            }
                            ClientMessage::StopGame => {
                                break 'main_loop;
                            }
                            ClientMessage::WordGuessResponse(i, w) if i == self.guess_user => {
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

                    self.state = State::AcceptGuess;
                }
                State::AcceptGuess => {
                    // send the final round overview to everyone and wait 5 seconds
                    let overview = self
                        .users
                        .iter()
                        .map(|u| u.word_guess.as_ref().map(|w| (w.clone(), u.word_stricken)))
                        .collect::<Vec<_>>();

                    for i in 0..self.users.len() {
                        if let Err(e) = self.server_msg_tx.send(ServerMessage::ShowRoundOverview(
                            i,
                            self.word_to_guess.clone(),
                            overview.clone(),
                        )) {
                            error!("Could not send server message to middleware: {}", e);
                            break 'main_loop;
                        }
                    }

                    if self.users[self.guess_user].word_guess.is_none() {
                        delay_for(Duration::from_secs(10)).await;

                        // the player passed: forward to next round
                        self.words_left -= 1;
                    } else {
                        delay_for(Duration::from_secs(2)).await;

                        if let Err(e) = self
                            .server_msg_tx
                            .send(ServerMessage::AcceptGuessRequest(self.decision_user))
                        {
                            error!("Could not send server message to middleware: {}", e);
                            break 'main_loop;
                        }

                        let mut accept = false;
                        while let Some(res) = self.client_msg_rx.recv().await {
                            match res {
                                ClientMessage::ResendGameState(i) => {
                                    self.send_full_game_state(i);
                                }
                                ClientMessage::StopGame => {
                                    break 'main_loop;
                                }
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
                        self.state = State::NextRound;
                    }
                }
                State::Finished => {
                    // TODO: offer new game
                    info!("Game is over!");

                    for i in 0..self.users.len() {
                        if let Err(e) = self
                            .server_msg_tx
                            .send(ServerMessage::Finished(i, self.score))
                        {
                            error!("Could not send server message to middleware: {}", e);
                            break 'main_loop;
                        }
                    }
                    break;
                }
            }
        }

        info!("Main game loop has finished");
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
                        .expect("Could not send to client");
                }
                "a" if cmd_split.len() == 3 => match cmd_split[2].parse::<bool>() {
                    Ok(b) => {
                        out_channel_req
                            .send(ClientMessage::AcceptGuessResponse(id, b))
                            .expect("Could not send to client");
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
                        .expect("Could not send to client");
                }
                "r" if cmd_split.len() == 2 => {
                    out_channel_req
                        .send(ClientMessage::ResendGameState(id))
                        .expect("Could not send to client");
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
                        .expect("Could not send to client");
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
