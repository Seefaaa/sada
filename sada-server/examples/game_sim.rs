//! A fake SS13 round driving the control socket, with a terminal UI.
//!
//! It stands in for the BYOND server entirely: a crew standing on a map, a 200 ms game tick, delta state pushes,
//! push-to-talk, radio channels and the auth-code handshake. The whole control protocol can therefore be exercised
//! without BYOND, and the effect of every game-side action is visible immediately.
//!
//! ```sh
//! cargo run -p sada-server                              # in one terminal
//! cargo run -p sada-server --example game_sim           # in another
//! ```
//!
//! Then open the web client, press `e` here to mint a code for the selected crew member, and type that code into the
//! browser. The player's row picks up a session id and push-to-talk starts having an audible effect.
//!
//! The socket path defaults to `/tmp/sada.sock` and can be overridden with `SADA_CONTROL_SOCKET`.
//!
//! # What it models
//!
//! Everything the game owns and the server does not: positions, earshot, hearing and speaking impairments, radio
//! equipment, and who is holding their talk key. Each tick the simulation diffs its crew against what the server was
//! last told and sends only the difference.

use std::{
    collections::VecDeque,
    env,
    io,
    os::unix::net::UnixStream,
    time::{Duration, Instant, SystemTime},
};

use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use sada_common::{
    AuthCode,
    Ckey,
    ControlEvent,
    ControlFrameBuffer,
    ControlRequest,
    ControlResponse,
    Freq,
    PlayerPatch,
    Position,
    SessionId,
    Transmit,
};

/// Socket path used when `SADA_CONTROL_SOCKET` is unset.
const DEFAULT_SOCKET: &str = "/tmp/sada.sock";

/// How often the simulated game recomputes and pushes state.
const TICK: Duration = Duration::from_millis(200);

/// How long to wait before deciding the server is not answering.
///
/// The UI is single threaded, so a hung server would otherwise freeze the whole interface rather than just the
/// connection.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Width of the simulated map, in tiles.
const MAP_WIDTH: i32 = 56;

/// Height of the simulated map, in tiles.
const MAP_HEIGHT: i32 = 28;

/// Distance at which one crew member can still hear another.
///
/// Chebyshev, matching `ProximityRouter`'s default, so the hearer lists this example sends agree with what the server
/// would compute from positions alone.
const HEARING_RADIUS: i32 = 7;

/// Largest number of events collected in one poll.
const EVENTS_PER_POLL: u16 = 64;

/// Number of log lines kept.
const LOG_CAPACITY: usize = 500;

/// Characters an auth code is drawn from.
///
/// No vowels, so a generated code cannot read as a word, and no `0`/`O` or
/// `1`/`I`, which players misread when copying a code off a screen.
const CODE_ALPHABET: &[u8] = b"23456789BCDFGHJKLMNPQRSTVWXYZ";

/// Length of a generated auth code.
const CODE_LENGTH: usize = 6;

/// A radio channel the crew can talk on.
struct Channel {
    /// Name shown in the interface.
    name: &'static str,
    /// Frequency sent to the server.
    freq: Freq,
}

/// Radio channels this round has, in the order their number keys select them.
const CHANNELS: [Channel; 4] = [
    Channel {
        name: "common",
        freq: Freq(1459),
    },
    Channel {
        name: "security",
        freq: Freq(1359),
    },
    Channel {
        name: "engineering",
        freq: Freq(1357),
    },
    Channel {
        name: "medical",
        freq: Freq(1355),
    },
];

/// The crew the round starts with, as name, map glyph, position and department.
const CREW: [(&str, char, i32, i32, usize); 6] = [
    ("sefa", 'a', 6, 5, 1),
    ("bishop", 'b', 10, 6, 1),
    ("cutler", 'c', 14, 8, 2),
    ("dorn", 'd', 20, 4, 3),
    ("ekko", 'e', 23, 11, 2),
    ("fern", 'f', 4, 11, 0),
];

fn main() -> io::Result<()> {
    let path = env::var("SADA_CONTROL_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_owned());
    let mut app = App::new(path);

    ratatui::run(|terminal| {
        while app.running {
            terminal.draw(|frame| app.render(frame))?;
            app.wait_for_input()?;
            app.advance();
        }
        Ok(())
    })
}

/// What a crew member is currently transmitting on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Talking {
    /// Speaking out loud, heard by whoever is in earshot.
    Local,
    /// Speaking into a headset on one frequency.
    Radio(Freq),
}

/// The state of one crew member, as the game sees them.
struct Player {
    /// Player key, the identity the server knows them by.
    ckey: Ckey,
    /// Character drawn on the map.
    glyph: char,
    /// Where they are standing.
    position: Position,
    /// Whether the game considers them unable to speak.
    mute: bool,
    /// Whether the game considers them unable to hear.
    deaf: bool,
    /// Whether they hold admin rights.
    admin: bool,
    /// Whether they are a ghost with omnipresent hearing.
    ghost_ears: bool,
    /// Frequencies their headset can transmit on.
    hot: Vec<Freq>,
    /// Frequencies their headset receives.
    hear: Vec<Freq>,
    /// Language they are speaking, as a BYOND type path.
    language: String,
    /// What they are transmitting on right now, if anything.
    talking: Option<Talking>,
    /// Session bound to them once a browser redeemed their code.
    session: Option<SessionId>,
    /// Code minted for them and not yet redeemed.
    code: Option<AuthCode>,
    /// Direction autopilot is currently walking them in.
    heading: (i32, i32),
    /// State the server has already been told about, for diffing.
    sent: Option<Snapshot>,
    /// Transmit intent the server has already been told about.
    sent_talking: Option<Talking>,
}

impl Player {
    /// Create a crew member standing at `x`, `y` with `department`'s radio.
    fn new(ckey: &str, glyph: char, x: i32, y: i32, department: usize) -> Self {
        let mut freqs = vec![CHANNELS[0].freq];

        if let Some(channel) = CHANNELS.get(department).filter(|_| department != 0) {
            freqs.push(channel.freq);
        }

        Self {
            ckey: Ckey::from(ckey),
            glyph,
            position: Position { x, y, z: 2 },
            mute: false,
            deaf: false,
            admin: false,
            ghost_ears: false,
            hot: freqs.clone(),
            hear: freqs,
            language: "galactic-common".to_owned(),
            talking: None,
            session: None,
            code: None,
            heading: (1, 0),
            sent: None,
            sent_talking: None,
        }
    }

    /// Whether this crew member can use `freq`.
    fn carries(&self, freq: Freq) -> bool { self.hot.contains(&freq) }

    /// Build the state to push, given the earshot list the game computed.
    fn snapshot(&self, local_with: Vec<Ckey>) -> Snapshot {
        Snapshot {
            mute: self.mute,
            deaf: self.deaf,
            admin: self.admin,
            ghost_ears: self.ghost_ears,
            position: self.position,
            local_with,
            hot: self.hot.clone(),
            hear: self.hear.clone(),
            language: self.language.clone(),
        }
    }
}

/// Everything about a player that lives on the server.
///
/// Kept alongside the player so a tick can send the difference rather than the whole state. The real bridge has the
/// same problem and the same answer: the game knows what changed, the wire should only carry that.
#[derive(Clone, PartialEq)]
struct Snapshot {
    /// Whether the game considers them unable to speak.
    mute: bool,
    /// Whether the game considers them unable to hear.
    deaf: bool,
    /// Whether they hold admin rights.
    admin: bool,
    /// Whether they are a ghost with omnipresent hearing.
    ghost_ears: bool,
    /// Where they are standing.
    position: Position,
    /// Who is in earshot of them.
    local_with: Vec<Ckey>,
    /// Frequencies their headset can transmit on.
    hot: Vec<Freq>,
    /// Frequencies their headset receives.
    hear: Vec<Freq>,
    /// Language they are speaking.
    language: String,
}

impl Snapshot {
    /// Build the patch that takes the server from `previous` to `self`.
    fn diff(&self, previous: Option<&Self>) -> PlayerPatch {
        // A player the server has never heard of needs every field, which is
        // also what a reconnect needs after the snapshots are cleared.
        let Some(old) = previous else {
            return self.full();
        };

        let language = old.language != self.language;

        PlayerPatch {
            mute: (old.mute != self.mute).then_some(self.mute),
            deaf: (old.deaf != self.deaf).then_some(self.deaf),
            is_admin: (old.admin != self.admin).then_some(self.admin),
            ghost_ears: (old.ghost_ears != self.ghost_ears).then_some(self.ghost_ears),
            position: (old.position != self.position).then_some(self.position),
            local_with: (old.local_with != self.local_with).then(|| self.local_with.clone()),
            hot_freqs: (old.hot != self.hot).then(|| self.hot.clone()),
            hear_freqs: (old.hear != self.hear).then(|| self.hear.clone()),
            known_languages: language.then(|| vec![self.language.clone()]),
            current_language: language.then(|| self.language.clone()),
        }
    }

    /// Build a patch carrying every field.
    fn full(&self) -> PlayerPatch {
        PlayerPatch {
            mute: Some(self.mute),
            deaf: Some(self.deaf),
            is_admin: Some(self.admin),
            ghost_ears: Some(self.ghost_ears),
            position: Some(self.position),
            local_with: Some(self.local_with.clone()),
            hot_freqs: Some(self.hot.clone()),
            hear_freqs: Some(self.hear.clone()),
            known_languages: Some(vec![self.language.clone()]),
            current_language: Some(self.language.clone()),
        }
    }
}

/// What to remember once the server acknowledges a batched request.
///
/// Committing only on acknowledgement is what keeps the diff honest: a request the server rejected must not be treated
/// as state it knows about, or the field would never be sent again.
enum Commit {
    /// A player's state was accepted.
    Patch {
        /// Index of the player in the roster.
        player: usize,
        /// State the server now holds for them.
        snapshot: Snapshot,
    },
    /// A player's transmit intent was accepted.
    Talking {
        /// Index of the player in the roster.
        player: usize,
        /// Intent the server now holds for them.
        talking: Option<Talking>,
    },
    /// The response carries queued events.
    Events,
}

/// A blocking client for the control socket.
///
/// Reconnects lazily, because the simulation is expected to outlive server restarts, that is half of what makes it
/// useful to leave running.
struct Control {
    /// Path of the socket to connect to.
    path: String,
    /// Current connection, absent while disconnected.
    stream: Option<UnixStream>,
    /// Reusable framing buffer.
    buffer: ControlFrameBuffer,
    /// Why the last attempt failed, shown in the status bar.
    error: Option<String>,
    /// Number of requests sent since startup.
    requests: u64,
}

impl Control {
    /// Create a client that will connect to `path` on first use.
    fn new(path: String) -> Self {
        Self {
            path,
            stream: None,
            buffer: ControlFrameBuffer::new(),
            error: None,
            requests: 0,
        }
    }

    /// Whether a connection is currently established.
    fn is_connected(&self) -> bool { self.stream.is_some() }

    /// Send one request and read its response, connecting if needed.
    ///
    /// Any failure drops the connection so the next call reconnects, and is reported through [`Control::error`] rather
    /// than by returning a value the caller would have to thread through the UI.
    fn call(&mut self, request: &ControlRequest) -> Option<ControlResponse> {
        if self.stream.is_none() {
            self.connect()?;
        }

        let stream = self.stream.as_mut()?;

        self.requests += 1;

        let outcome = self
            .buffer
            .write(stream, request)
            .and_then(|()| self.buffer.read(stream));

        match outcome {
            Ok(Some(response)) => {
                self.error = None;
                Some(response)
            },
            Ok(None) => {
                self.drop_connection("the server closed the connection".to_owned());
                None
            },
            Err(err) => {
                self.drop_connection(err.to_string());
                None
            },
        }
    }

    /// Open the socket and apply the call timeouts.
    fn connect(&mut self) -> Option<()> {
        match UnixStream::connect(&self.path) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(CALL_TIMEOUT));
                let _ = stream.set_write_timeout(Some(CALL_TIMEOUT));
                self.stream = Some(stream);
                self.error = None;
                Some(())
            },
            Err(err) => {
                self.error = Some(err.to_string());
                None
            },
        }
    }

    /// Forget the connection and record why.
    fn drop_connection(&mut self, reason: String) {
        self.stream = None;
        self.error = Some(reason);
    }
}

/// How a log line should be coloured.
#[derive(Clone, Copy)]
enum LogKind {
    /// Something the simulation did.
    Action,
    /// An event the server reported.
    Event,
    /// Something went wrong.
    Error,
}

/// One line in the log pane.
struct LogLine {
    /// Tick the line was recorded on.
    tick: u64,
    /// Text to show.
    text: String,
    /// How to colour it.
    kind: LogKind,
}

/// The simulation and its interface.
struct App {
    /// Connection to the voice server.
    control: Control,
    /// Everyone in the round.
    players: Vec<Player>,
    /// Index of the crew member the keyboard controls.
    selected: usize,
    /// Scroll state of the crew list.
    crew_state: ListState,
    /// Recent activity, newest last.
    log: VecDeque<LogLine>,
    /// Whether unselected crew wander on their own.
    autopilot: bool,
    /// Number of game ticks elapsed.
    tick: u64,
    /// Version string reported by the server, once known.
    server_version: Option<String>,
    /// Connection error already written to the log.
    ///
    /// The tick runs five times a second, so without this a server that is
    /// simply not running would bury every other line in the log.
    reported_error: Option<String>,
    /// When the next tick is due.
    next_tick: Instant,
    /// Whether the interface should keep running.
    running: bool,
    /// State of the code generator.
    seed: u64,
    /// Number of crew added since startup, used to name them.
    spawned: usize,
}

impl App {
    /// Set up a round with the starting crew.
    fn new(path: String) -> Self {
        let players = CREW
            .iter()
            .map(|&(ckey, glyph, x, y, department)| Player::new(ckey, glyph, x, y, department))
            .collect();

        Self {
            control: Control::new(path),
            players,
            selected: 0,
            crew_state: ListState::default().with_selected(Some(0)),
            log: VecDeque::new(),
            autopilot: true,
            tick: 0,
            server_version: None,
            reported_error: None,
            next_tick: Instant::now(),
            running: true,
            seed: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0x9E37_79B9_7F4A_7C15, |since| since.as_nanos() as u64)
                | 1,
            spawned: 0,
        }
    }

    /// Block until the next tick is due or a key is pressed.
    fn wait_for_input(&mut self) -> io::Result<()> {
        let timeout = self.next_tick.saturating_duration_since(Instant::now());

        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            self.on_key(key.code, key.modifiers);
        }

        Ok(())
    }

    /// Run a game tick if one is due.
    fn advance(&mut self) {
        let now = Instant::now();

        if now < self.next_tick {
            return;
        }

        self.next_tick = now + TICK;
        self.run_tick();
    }

    /// Advance the world by one tick and push the difference to the server.
    fn run_tick(&mut self) {
        self.tick += 1;

        if self.autopilot {
            self.wander();
        }

        if !self.control.is_connected() {
            // A fresh connection knows nothing about us, so everything has to be
            // sent again. Clearing the snapshots is what makes reconnecting work.
            self.forget_server_state();
        }

        let (requests, commits) = self.build_requests();

        let Some(response) = self.control.call(&ControlRequest::Batch(requests)) else {
            self.report_connection_error();
            return;
        };
        self.reported_error = None;

        if self.server_version.is_none() {
            self.ask_version();
        }

        match response {
            ControlResponse::Batch(responses) => self.apply_batch(responses, commits),
            other => self.log(LogKind::Error, format!("unexpected response to a batch: {other:?}")),
        }
    }

    /// Walk every crew member the player is not controlling.
    fn wander(&mut self) {
        for index in 0..self.players.len() {
            if index == self.selected {
                continue;
            }

            // 1 in 8 chance of turning, otherwise keep walking straight.
            let turn = self.next_random().is_multiple_of(8);
            let player = &mut self.players[index];

            if turn {
                let (dx, dy) = match self.seed % 4 {
                    0 => (1, 0),
                    1 => (-1, 0),
                    2 => (0, 1),
                    _ => (0, -1),
                };
                player.heading = (dx, dy);
            }

            let x = player.position.x + player.heading.0;
            let y = player.position.y + player.heading.1;

            if (0..MAP_WIDTH).contains(&x) && (0..MAP_HEIGHT).contains(&y) {
                player.position.x = x;
                player.position.y = y;
            } else {
                // bounce off the wall instead of grinding along it.
                player.heading = (-player.heading.0, -player.heading.1);
            }
        }
    }

    /// Turn the current world state into the requests one tick should send.
    fn build_requests(&self) -> (Vec<ControlRequest>, Vec<Commit>) {
        let mut requests = Vec::new();
        let mut commits = Vec::new();

        for (index, player) in self.players.iter().enumerate() {
            let snapshot = player.snapshot(self.earshot(index));
            let patch = snapshot.diff(player.sent.as_ref());

            if !patch.is_empty() {
                requests.push(ControlRequest::PatchPlayer {
                    ckey: player.ckey.clone(),
                    patch,
                });
                commits.push(Commit::Patch {
                    player: index,
                    snapshot,
                });
            }
        }

        for (index, player) in self.players.iter().enumerate() {
            // Push-to-talk addresses a session, so it only means something once
            // a browser has redeemed this player's code.
            let (Some(session), true) = (player.session, player.talking != player.sent_talking) else {
                continue;
            };

            requests.push(match player.talking {
                Some(Talking::Local) => ControlRequest::SetTransmit {
                    session,
                    transmit: Some(Transmit::Local),
                },
                Some(Talking::Radio(freq)) => ControlRequest::SetTransmit {
                    session,
                    transmit: Some(Transmit::Radio(freq)),
                },
                None => ControlRequest::SetTransmit {
                    session,
                    transmit: None,
                },
            });
            commits.push(Commit::Talking {
                player: index,
                talking: player.talking,
            });
        }

        requests.push(ControlRequest::PollEvents { max: EVENTS_PER_POLL });
        commits.push(Commit::Events);

        (requests, commits)
    }

    /// Match each response to what it acknowledges.
    fn apply_batch(&mut self, responses: Vec<ControlResponse>, commits: Vec<Commit>) {
        if responses.len() != commits.len() {
            self.log(
                LogKind::Error,
                format!("batch answered {} of {} requests", responses.len(), commits.len()),
            );
        }

        for (response, commit) in responses.into_iter().zip(commits) {
            match (response, commit) {
                (ControlResponse::Ok, Commit::Patch { player, snapshot }) => {
                    self.players[player].sent = Some(snapshot);
                },
                (ControlResponse::Ok, Commit::Talking { player, talking }) => {
                    self.players[player].sent_talking = talking;
                    let who = &self.players[player].ckey;
                    self.log(LogKind::Action, format!("{who} {}", describe_talking(talking)));
                },
                (ControlResponse::Events(events), Commit::Events) => {
                    for event in events {
                        self.on_event(event);
                    }
                },
                (ControlResponse::Error { message }, _) => {
                    self.log(LogKind::Error, message);
                },
                (other, _) => {
                    self.log(LogKind::Error, format!("unexpected response: {other:?}"));
                },
            }
        }
    }

    /// React to one event the server queued for the game.
    fn on_event(&mut self, event: ControlEvent) {
        match event {
            ControlEvent::Authenticated { ckey, session } => {
                self.log(LogKind::Event, format!("{ckey} authenticated as session {session}"));

                if let Some(player) = self.player_mut(&ckey) {
                    player.session = Some(session);
                    player.code = None;
                    player.sent_talking = None;
                }
            },
            ControlEvent::Disconnected { ckey, session } => {
                self.log(LogKind::Event, format!("{ckey} lost session {session}"));

                if let Some(player) = self.player_mut(&ckey)
                    && player.session == Some(session)
                {
                    player.session = None;
                    player.sent_talking = None;
                }
            },
            ControlEvent::Speaking { speaker, listeners } => {
                let heard = listeners.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                self.log(LogKind::Event, format!("{speaker} heard by [{heard}]"));
            },
            ControlEvent::Heard {
                speaker,
                listener,
                channel,
                language,
            } => {
                let where_ = channel.map_or_else(|| "locally".to_owned(), |freq| format!("on {}", freq.0));
                let language = language.unwrap_or_else(|| "?".to_owned());
                self.log(
                    LogKind::Event,
                    format!("{listener} heard {speaker} {where_} in {language}"),
                );
            },
        }
    }

    /// Ask the server what it is, once per connection.
    fn ask_version(&mut self) {
        match self.control.call(&ControlRequest::Version) {
            Some(ControlResponse::Version { protocol, version }) => {
                if protocol != sada_common::PROTOCOL_VERSION {
                    self.log(
                        LogKind::Error,
                        format!(
                            "server speaks control protocol {protocol}, this simulation speaks {}",
                            sada_common::PROTOCOL_VERSION
                        ),
                    );
                }
                self.log(LogKind::Action, format!("connected to sada {version}"));
                self.server_version = Some(version);
            },
            Some(other) => self.log(LogKind::Error, format!("unexpected version response: {other:?}")),
            None => {},
        }
    }

    /// Write the current connection error to the log, but only once.
    fn report_connection_error(&mut self) {
        if self.reported_error == self.control.error {
            return;
        }

        self.reported_error.clone_from(&self.control.error);

        if let Some(error) = &self.reported_error {
            self.log(LogKind::Error, format!("control socket: {error}"));
        }
    }

    /// Drop every record of what the server knows.
    fn forget_server_state(&mut self) {
        self.server_version = None;
        for player in &mut self.players {
            player.sent = None;
            player.sent_talking = None;
        }
    }

    /// Who can hear the player at `index` speak out loud.
    ///
    /// This is the game's job in the real system too: only it knows about walls, doors and holopads. The simulation
    /// approximates that with plain distance on the same z-level.
    fn earshot(&self, index: usize) -> Vec<Ckey> {
        let origin = self.players[index].position;

        self.players
            .iter()
            .enumerate()
            .filter(|&(other, player)| {
                other != index
                    && player.position.z == origin.z
                    && (player.position.x - origin.x).abs() <= HEARING_RADIUS
                    && (player.position.y - origin.y).abs() <= HEARING_RADIUS
            })
            .map(|(_, player)| player.ckey.clone())
            .collect()
    }

    /// Find a crew member by key.
    fn player_mut(&mut self, ckey: &Ckey) -> Option<&mut Player> {
        self.players.iter_mut().find(|player| &player.ckey == ckey)
    }

    /// Apply one key press.
    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.running = false,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => self.running = false,

            KeyCode::Tab => self.select(1),
            KeyCode::BackTab => self.select(-1),

            KeyCode::Left => self.step(-1, 0),
            KeyCode::Right => self.step(1, 0),
            KeyCode::Up => self.step(0, 1),
            KeyCode::Down => self.step(0, -1),

            KeyCode::Char(' ') => self.toggle_talking(Talking::Local),
            KeyCode::Char(digit @ '1'..='4') => self.toggle_radio(digit),

            KeyCode::Char('m') => self.toggle_flag(Flag::Mute),
            KeyCode::Char('d') => self.toggle_flag(Flag::Deaf),
            KeyCode::Char('g') => self.toggle_flag(Flag::GhostEars),
            KeyCode::Char('A') => self.toggle_flag(Flag::Admin),

            KeyCode::Char('e') => self.mint_code(),
            KeyCode::Char('k') => self.check_auth(),
            KeyCode::Char('z') => self.change_level(),

            KeyCode::Char('n') => self.add_player(),
            KeyCode::Char('x') => self.remove_player(),
            KeyCode::Char('p') => {
                self.autopilot = !self.autopilot;
                let state = if self.autopilot { "on" } else { "off" };
                self.log(LogKind::Action, format!("autopilot {state}"));
            },

            _ => {},
        }
    }

    /// Move the selection by `delta` rows, wrapping.
    fn select(&mut self, delta: isize) {
        if self.players.is_empty() {
            return;
        }

        let count = self.players.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(count)) as usize;
        self.crew_state.select(Some(self.selected));
    }

    /// Walk the selected crew member one tile.
    fn step(&mut self, dx: i32, dy: i32) {
        let Some(player) = self.players.get_mut(self.selected) else {
            return;
        };

        player.position.x = (player.position.x + dx).clamp(0, MAP_WIDTH - 1);
        player.position.y = (player.position.y + dy).clamp(0, MAP_HEIGHT - 1);
    }

    /// Move the selected crew member to the next z-level, cycling through three.
    fn change_level(&mut self) {
        if let Some(player) = self.players.get_mut(self.selected) {
            player.position.z = player.position.z % 3 + 1;
        }
    }

    /// Start or stop transmitting on `channel`.
    ///
    /// Not every terminal reports key releases, so this toggles rather than acting
    /// as a held key. The server sees exactly the same requests either way.
    fn toggle_talking(&mut self, channel: Talking) {
        if let Some(player) = self.players.get_mut(self.selected) {
            player.talking = if player.talking == Some(channel) {
                None
            } else {
                Some(channel)
            };
        }
    }

    /// Start or stop transmitting on the channel a number key selects.
    fn toggle_radio(&mut self, digit: char) {
        let Some(index) = digit.to_digit(10).map(|digit| digit as usize - 1) else {
            return;
        };
        let Some(channel) = CHANNELS.get(index) else {
            return;
        };
        let Some(player) = self.players.get(self.selected) else {
            return;
        };

        if !player.carries(channel.freq) {
            self.log(
                LogKind::Error,
                format!(
                    "{} has no headset for {} ({})",
                    player.ckey, channel.name, channel.freq.0
                ),
            );
            return;
        }

        self.toggle_talking(Talking::Radio(channel.freq));
    }

    /// Flip one of the boolean states the game owns.
    fn toggle_flag(&mut self, flag: Flag) {
        let Some(player) = self.players.get_mut(self.selected) else {
            return;
        };

        let field = match flag {
            Flag::Mute => &mut player.mute,
            Flag::Deaf => &mut player.deaf,
            Flag::Admin => &mut player.admin,
            Flag::GhostEars => &mut player.ghost_ears,
        };
        *field = !*field;

        let state = if *field { "set" } else { "cleared" };
        let who = player.ckey.clone();
        self.log(LogKind::Action, format!("{who}: {} {state}", flag.name()));
    }

    /// Mint a code for the selected crew member and register it.
    fn mint_code(&mut self) {
        let Some(ckey) = self.players.get(self.selected).map(|player| player.ckey.clone()) else {
            return;
        };
        let code = self.generate_code();

        let request = ControlRequest::RegisterCode {
            code: code.clone(),
            ckey: ckey.clone(),
        };

        match self.control.call(&request) {
            Some(ControlResponse::Ok) => {
                self.players[self.selected].code = Some(code.clone());
                self.log(
                    LogKind::Action,
                    format!("{ckey}: code {} - type it into the web client", code.as_str()),
                );
            },
            Some(other) => self.log(LogKind::Error, format!("register-code refused: {other:?}")),
            None => self.log(LogKind::Error, "register-code failed: not connected".to_owned()),
        }
    }

    /// Ask the server which session the selected crew member holds.
    fn check_auth(&mut self) {
        let Some(ckey) = self.players.get(self.selected).map(|player| player.ckey.clone()) else {
            return;
        };

        match self.control.call(&ControlRequest::CheckAuth { ckey: ckey.clone() }) {
            Some(ControlResponse::Session { session }) => {
                let answer = session.map_or_else(|| "not authenticated".to_owned(), |id| format!("session {id}"));
                self.log(LogKind::Action, format!("{ckey}: {answer}"));
                if let Some(player) = self.players.get_mut(self.selected) {
                    player.session = session;
                }
            },
            Some(other) => self.log(LogKind::Error, format!("check-auth refused: {other:?}")),
            None => self.log(LogKind::Error, "check-auth failed: not connected".to_owned()),
        }
    }

    /// Add one more crew member, as a late joiner would.
    fn add_player(&mut self) {
        self.spawned += 1;

        let name = format!("latejoin{}", self.spawned);
        let glyph = char::from(b'g' + ((self.spawned - 1) % 20) as u8);
        let x = (self.next_random() % MAP_WIDTH as u64) as i32;
        let y = (self.next_random() % MAP_HEIGHT as u64) as i32;
        let department = (self.next_random() % CHANNELS.len() as u64) as usize;

        self.players.push(Player::new(&name, glyph, x, y, department));

        self.log(LogKind::Action, format!("{name} joined at ({x}, {y})"));
    }

    /// Drop the selected crew member, as a disconnect would.
    fn remove_player(&mut self) {
        if self.players.len() <= 1 {
            return;
        }

        let player = self.players.remove(self.selected);

        self.selected = self.selected.min(self.players.len() - 1);
        self.crew_state.select(Some(self.selected));

        match self.control.call(&ControlRequest::RemovePlayer {
            ckey: player.ckey.clone(),
        }) {
            Some(ControlResponse::Ok) => self.log(LogKind::Action, format!("{} left the round", player.ckey)),
            Some(other) => self.log(LogKind::Error, format!("remove-player refused: {other:?}")),
            None => self.log(LogKind::Error, "remove-player failed: not connected".to_owned()),
        }

        // Everyone's earshot list just changed, so make the next tick resend it.
        for player in &mut self.players {
            player.sent = None;
        }
    }

    /// Record a line in the log pane.
    fn log(&mut self, kind: LogKind, text: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(LogLine {
            tick: self.tick,
            text,
            kind,
        });
    }

    /// Draw the next value from the xorshift generator.
    ///
    /// Hand-rolled so the example stays free of dependencies the server does not
    /// already have; nothing here needs randomness worth defending.
    fn next_random(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    /// Mint a code in the shape the game would show a player.
    fn generate_code(&mut self) -> AuthCode {
        let code = (0..CODE_LENGTH)
            .map(|_| char::from(CODE_ALPHABET[self.next_random() as usize % CODE_ALPHABET.len()]))
            .collect::<String>();

        AuthCode::new(code)
    }

    /// Draw the whole interface.
    fn render(&mut self, frame: &mut Frame<'_>) {
        let [status, main, log, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(9),
            Constraint::Length(2),
        ])
        .areas(frame.area());

        let [crew, map, detail] =
            Layout::horizontal([Constraint::Length(48), Constraint::Min(20), Constraint::Length(34)]).areas(main);

        self.render_status(frame, status);
        self.render_crew(frame, crew);
        self.render_map(frame, map);
        self.render_detail(frame, detail);
        self.render_log(frame, log);
        render_help(frame, help);
    }

    /// Draw the one-line status bar.
    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let (state, style) = if self.control.is_connected() {
            ("connected", Style::new().fg(Color::Black).bg(Color::Green))
        } else {
            ("offline", Style::new().fg(Color::White).bg(Color::Red))
        };

        let trailer = match &self.control.error {
            Some(error) => format!("  ({error})"),
            None => String::new(),
        };

        let version = self.server_version.as_deref().unwrap_or("?");
        let bound = self.players.iter().filter(|player| player.session.is_some()).count();
        let talking = self.players.iter().filter(|player| player.talking.is_some()).count();

        let line = Line::from(vec![
            Span::styled(format!(" {state} "), style),
            Span::raw(format!(
                " sada {version}  tick {}  requests {}  crew {} ({bound} bound, {talking} talking){trailer}  {}",
                self.tick,
                self.control.requests,
                self.players.len(),
                self.control.path,
            )),
        ]);

        frame.render_widget(Paragraph::new(line), area);
    }

    /// Draw the crew roster.
    fn render_crew(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let items = self
            .players
            .iter()
            .map(|player| {
                let session = player
                    .session
                    .map_or_else(|| format!("{:^5}", "---"), |id| format!("{id:^5}"));

                let mut flags = String::new();
                flags.push(if player.mute { 'M' } else { '-' });
                flags.push(if player.deaf { 'D' } else { '-' });
                flags.push(if player.admin { 'A' } else { '-' });
                flags.push(if player.ghost_ears { 'G' } else { '-' });

                let mut spans = vec![
                    Span::styled(format!("{} ", player.glyph), Style::new().fg(Color::Cyan)),
                    Span::raw(format!("{:<12}", player.ckey)),
                    Span::styled(
                        format!(
                            "{:>3},{:>3},{:>3} ",
                            player.position.x, player.position.y, player.position.z
                        ),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        session,
                        Style::new().fg(if player.session.is_some() {
                            Color::Green
                        } else {
                            Color::DarkGray
                        }),
                    ),
                    Span::styled(format!(" {flags} "), Style::new().fg(Color::Yellow)),
                ];

                spans.push(match player.talking {
                    Some(Talking::Local) => {
                        Span::styled("local", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
                    },
                    Some(Talking::Radio(freq)) => Span::styled(
                        format!("{}", freq.0),
                        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                    ),
                    None => Span::raw(""),
                });

                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();

        let list = List::new(items)
            .block(Block::bordered().title(" crew "))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.crew_state);
    }

    /// Draw the map of the selected crew member's z-level.
    fn render_map(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(here) = self.players.get(self.selected) else {
            return;
        };
        let level = here.position.z;

        let mut lines = Vec::with_capacity(MAP_HEIGHT as usize);

        // Rows run top to bottom on screen but y grows northward in the game.
        for y in (0..MAP_HEIGHT).rev() {
            let mut spans = Vec::with_capacity(MAP_WIDTH as usize);

            for x in 0..MAP_WIDTH {
                let occupant =
                    self.players.iter().enumerate().find(|(_, player)| {
                        player.position.z == level && player.position.x == x && player.position.y == y
                    });

                spans.push(match occupant {
                    Some((index, player)) => {
                        let style = match player.talking {
                            Some(Talking::Local) => Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                            Some(Talking::Radio(_)) => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                            None if index == self.selected => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                            None => Style::new().fg(Color::White),
                        };
                        Span::styled(format!("{} ", player.glyph), style)
                    },
                    None => {
                        let in_earshot = (x - here.position.x).abs() <= HEARING_RADIUS
                            && (y - here.position.y).abs() <= HEARING_RADIUS;
                        let glyph = if in_earshot { "· " } else { "  " };
                        Span::styled(glyph, Style::new().fg(Color::DarkGray))
                    },
                });
            }

            lines.push(Line::from(spans));
        }

        let title = format!(" z={level} · earshot {HEARING_RADIUS} ");
        frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), area);
    }

    /// Draw everything known about the selected crew member.
    fn render_detail(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(player) = self.players.get(self.selected) else {
            return;
        };

        let freqs = |list: &[Freq]| {
            list.iter()
                .map(|freq| {
                    CHANNELS
                        .iter()
                        .find(|channel| channel.freq == *freq)
                        .map_or_else(|| freq.0.to_string(), |channel| channel.name.to_owned())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        let earshot = self.earshot(self.selected);
        let heard_by = if earshot.is_empty() {
            "nobody".to_owned()
        } else {
            earshot.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
        };

        let mut lines = vec![
            field("ckey", player.ckey.to_string()),
            field(
                "at",
                format!("({}, {}) z={}", player.position.x, player.position.y, player.position.z),
            ),
            field(
                "session",
                player
                    .session
                    .map_or_else(|| "not authenticated".to_owned(), |id| id.to_string()),
            ),
        ];

        if let Some(code) = &player.code {
            lines.push(Line::from(vec![
                Span::styled("code     ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    code.as_str().to_owned(),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        lines.extend([
            field("talking", describe_talking(player.talking)),
            field("mute", player.mute.to_string()),
            field("deaf", player.deaf.to_string()),
            field("admin", player.admin.to_string()),
            field("ghost", player.ghost_ears.to_string()),
            field("can send", freqs(&player.hot)),
            field("can hear", freqs(&player.hear)),
            field("language", player.language.clone()),
            field("earshot", heard_by),
        ]);

        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title(" selected "))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    /// Draw the activity log.
    fn render_log(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = area.height.saturating_sub(2) as usize;

        let lines = self
            .log
            .iter()
            .rev()
            .take(rows)
            .rev()
            .map(|entry| {
                let colour = match entry.kind {
                    LogKind::Action => Color::White,
                    LogKind::Event => Color::Cyan,
                    LogKind::Error => Color::Red,
                };

                Line::from(vec![
                    Span::styled(format!("{:>6} ", entry.tick), Style::new().fg(Color::DarkGray)),
                    Span::styled(entry.text.clone(), Style::new().fg(colour)),
                ])
            })
            .collect::<Vec<_>>();

        frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(" log ")), area);
    }
}

/// Which boolean state a key press flips.
#[derive(Clone, Copy)]
enum Flag {
    /// The game's own muteness.
    Mute,
    /// The game's own deafness.
    Deaf,
    /// Admin rights.
    Admin,
    /// Omnipresent hearing for the dead.
    GhostEars,
}

impl Flag {
    /// Name shown in the log.
    fn name(self) -> &'static str {
        match self {
            Self::Mute => "mute",
            Self::Deaf => "deaf",
            Self::Admin => "admin",
            Self::GhostEars => "ghost ears",
        }
    }
}

/// Describe a transmit intent in one phrase.
fn describe_talking(talking: Option<Talking>) -> String {
    match talking {
        Some(Talking::Local) => "speaking locally".to_owned(),
        Some(Talking::Radio(freq)) => {
            let name = CHANNELS
                .iter()
                .find(|channel| channel.freq == freq)
                .map_or("radio", |channel| channel.name);
            format!("on {name} ({})", freq.0)
        },
        None => "silent".to_owned(),
    }
}

/// One labelled line in the detail pane.
fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::new().fg(Color::DarkGray)),
        Span::raw(value),
    ])
}

/// Draw the key legend.
fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "tab/shift-tab select   arrows move   z level   space talk   1-4 radio (same key stops)   m mute",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "d deaf   g ghost   A admin   e mint code   k check auth   n join   x leave   p autopilot   q quit",
            Style::new().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(Paragraph::new(lines), area);
}
