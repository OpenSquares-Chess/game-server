use chess::{Game, GameResult, ChessMove, Action, Color};
use rschess::pgn::Pgn;
use tokio::net::{TcpStream, TcpListener};
use tokio::sync::Mutex;
use tokio::time::{interval, timeout, Duration};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use futures_util::stream::SplitSink;
use futures_util::{StreamExt, SinkExt};
use jwtk::jwk::RemoteJwksVerifier;
use std::sync::Arc;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH, Instant};
use rand::Rng;
use anyhow::{anyhow, Result};
use serde::{Serialize, Deserialize};
use serde_with::{serde_as, DisplayFromStr};
use dotenv::dotenv;
use std::env;

mod messages;
use messages::{request::Request, response::Response};

type WebSocketSink = SplitSink<WebSocketStream<TcpStream>, Message>;

#[derive(Clone)]
struct Player {
    account_id: String
}

#[serde_as]
#[derive(Serialize, Deserialize)]
struct Keys {
    #[serde_as(as = "[DisplayFromStr; 2]")]
    keys: [u64; 2],
    timestamp: u64
} 

struct Room {
    id: usize,
    active: bool,
    players: [Option<Player>; 2],
    game: Game,
    keys: Option<Keys>,
    start_time: u64,
    last_move_time: u64,
    remaining_time: [u64; 2],
    write_streams: [Option<Arc<Mutex<WebSocketSink>>>; 2],
    player_timeout: Option<JoinHandle<Result<()>>>
}

static REDIS_APPEND_ROOM: &str = r#"
    local exists = redis.call('LPOS', KEYS[1], ARGV[1])
    if not exists then
        redis.call('LPUSH', KEYS[1], ARGV[1])
        return 1
    else
        return 0
    end
"#;

async fn reset_room(
    room: &Mutex<Room>,
    conn: redis::aio::ConnectionManager
) -> Result<()> {
    let room_id: usize;
    let keys: String;
    {
        let mut room = room.lock().await;
        room_id = room.id;
        room.active = false;
        room.keys = Some(Keys {
            keys: generate_chess_keys(),
            timestamp: 0
        });
        keys = serde_json::to_string(&room.keys)?;
        room.game = Game::new();
        room.players = [None, None];
        room.start_time = 0;
        room.last_move_time = 0;
        room.remaining_time = [180000, 180000];
        room.write_streams = [None, None];
        if let Some(handle) = room.player_timeout.take() {
            handle.abort();
        }
    }
    let mut conn = conn.clone();

    let mut cmd = redis::cmd("SET");
    cmd.arg(format!("room:{}:keys", room_id)).arg(keys);
    let _: bool = cmd.query_async(&mut conn).await?;

    let script = redis::Script::new(REDIS_APPEND_ROOM);
    let key = "rooms";
    let arg = room_id.to_string();
    let _: bool = script.key(key).arg(arg).invoke_async(&mut conn).await?;

    Ok(())
}

#[derive(Serialize)]
struct GameRecord {
    player_one_id: String,
    player_two_id: String,
    player_one_rating: i32,
    player_two_rating: i32,
    date: mongodb::bson::DateTime,
    pgn: String,
}

async fn upload_match(room: &Mutex<Room>) -> Result<()> {
    let mut board = rschess::Board::default();
    {
        let room = room.lock().await;
        for action in room.game.actions() {
            match action {
                Action::MakeMove(m) => {
                    let uci = m.to_string();
                    board.make_move_uci(&uci)?;
                }
                Action::DeclareDraw => {
                    board.agree_draw()?;
                }
                _ => {}
            }
        }
        if room.game.result().is_none() {
            match room.game.side_to_move() {
                Color::White => {
                    board.resign(rschess::Color::White)?;
                }
                Color::Black => {
                    board.resign(rschess::Color::Black)?;
                }
            }
        }
        let pgn = Pgn::from_board(
            board,
            vec![
                ("Event", "?"),
                ("Site", "?"),
                ("Date", "????.??.??"),
                ("Round", "?"),
                ("White", "?"),
                ("Black", "?"),
            ]
            .into_iter()
            .map(|(t, v)| (t.to_owned(), v.to_owned()))
            .collect(),
        )?;
        let client = mongodb::Client::with_uri_str(format!(
            "mongodb+srv://{}:{}@cluster.czyii2a.mongodb.net/accounts?retryWrites=true&w=majority&appName=cluster",
            env::var("MONGO_USERNAME").expect("MONGO_USERNAME is not set"),
            env::var("MONGO_PASSWORD").expect("MONGO_PASSWORD is not set")
        )).await?;
        let db = client.database("accounts");
        let collection = db.collection("games");
        let game_record = GameRecord {
            player_one_id: room.players[0].as_ref().expect("player 1 missing").account_id.clone(),
            player_two_id: room.players[1].as_ref().expect("player 2 missing").account_id.clone(),
            player_one_rating: 0,
            player_two_rating: 0,
            date: mongodb::bson::DateTime::now(),
            pgn: pgn.to_string(),
        };
        collection.insert_one(game_record).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let rooms: Vec<Mutex<Room>> = (0..10).map(|i| Mutex::new(Room {
        id: i,
        active: false,
        players: [None, None],
        game: Game::new(),
        keys: None,
        start_time: 0,
        last_move_time: 0,
        remaining_time: [180000, 180000],
        write_streams: [None, None],
        player_timeout: None
    })).collect();
    let rooms = Arc::new(rooms);

    let clock = Instant::now();

    let jwks_url = match env::var("JWKS_URL") {
        Ok(jwks_url) => Some(jwks_url),
        Err(_) => None
    };

    let jwks_url = jwks_url.expect("JWKS_URL is not set");
    let cache_duration = Duration::from_secs(3600);
    let verifier = RemoteJwksVerifier::new(jwks_url, None, cache_duration);
    let verifier = Arc::new(verifier);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let client = redis::Client::open("redis://localhost:6379/?protocol=resp3")?;
    let config = redis::aio::ConnectionManagerConfig::new()
        .set_automatic_resubscription()
        .set_push_sender(tx);
    let mut conn = client.get_connection_manager_with_config(config).await?;

    broadcast_available_rooms(Arc::clone(&rooms), conn.clone()).await?;

    let rooms_ref = Arc::clone(&rooms);
    let conn_copy = conn.clone();
    let clock_copy = clock.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let _ = handle_match_found(
                msg,
                Arc::clone(&rooms_ref),
                conn_copy.clone(),
                clock_copy.clone()
            ).await;
        }
    });
    conn.subscribe("matchmaking:game").await?;

    listen_for_connections(rooms, verifier, conn, clock).await?;

    Ok(())
}

async fn listen_for_connections(
    rooms: Arc<Vec<Mutex<Room>>>,
    verifier: Arc<RemoteJwksVerifier>,
    conn: redis::aio::ConnectionManager,
    clock: Instant
) -> Result<()> {
    let addr = "0.0.0.0:8000".to_string();
    let listener = TcpListener::bind(&addr).await?;
    println!("Connection listener started on http://{}", addr);
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(handle_connection(
            stream,
            Arc::clone(&rooms),
            Arc::clone(&verifier),
            conn.clone(),
            clock.clone()
        ));
    }

    Ok(())
}

pub fn generate_chess_keys() -> [u64; 2] {
    let mut rng = rand::rng();
    let white_key: u64 = rng.random();
    let mut black_key: u64;
    loop {
        black_key = rng.random();
        if black_key != white_key {
            break;
        }
    }

    [white_key, black_key]
}

async fn broadcast_available_rooms(
    rooms: Arc<Vec<Mutex<Room>>>,
    mut conn: redis::aio::ConnectionManager
) -> Result<()> {
    for (index, room) in rooms.iter().enumerate() {
        let script = redis::Script::new(REDIS_APPEND_ROOM);
        let key = "rooms";
        let arg = index.to_string();
        let _: bool = script.key(key).arg(arg).invoke_async(&mut conn).await?;
        let keys: String;
        {
            let mut room = room.lock().await;
            room.keys = Some(Keys {
                keys: generate_chess_keys(),
                timestamp: 0
            });
            keys = serde_json::to_string(&room.keys)?;
        }
        let mut cmd = redis::cmd("SET");
        cmd.arg(format!("room:{}:keys", index)).arg(keys);
        let _: bool = cmd.query_async(&mut conn).await?;
    }

    Ok(())
}

async fn handle_match_found(
    info: redis::PushInfo,
    rooms: Arc<Vec<Mutex<Room>>>,
    mut conn: redis::aio::ConnectionManager,
    clock: Instant
) -> Result<()> {
    match info.kind {
        redis::PushKind::Message => {
            let message: &redis::Value = info.data.get(1)
                .ok_or(anyhow!("Invalid redis pubsub message"))?;
            let message = match message {
                redis::Value::SimpleString(s) => Ok(s.as_str()),
                redis::Value::BulkString(bytes) => Ok(std::str::from_utf8(&bytes)?),
                _ => Err(anyhow!("Invalid value type for matchmaking message")),
            }?;
            let room_id: usize = message.parse()?;
            rooms[room_id].lock().await.active = true;
            let rooms = Arc::clone(&rooms);
            let clock = clock.clone();
            tokio::spawn(async move {
                let mut cmd = redis::cmd("GET");
                cmd.arg(format!("room:{}:keys", room_id));
                let keys: String = cmd.query_async(&mut conn).await?;
                let keys: Keys = serde_json::from_str(&keys)?;
                let delay: std::time::Duration;
                {
                    let mut room = rooms[room_id].lock().await;
                    room.keys = Some(keys);

                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
                    let now: u64 = now.try_into()?;
                    let timestamp = room.keys.as_ref()
                        .ok_or(anyhow!("Room keys not found"))?.timestamp;

                    let clock_time: u64 = clock.elapsed().as_millis().try_into()?;
                    room.start_time = clock_time + timestamp - now;

                    let target: u64 = timestamp + 20000;
                    delay = if now < target {
                        std::time::Duration::from_millis(target - now)
                    } else {
                        std::time::Duration::from_millis(0)
                    }
                }
                tokio::time::sleep(delay).await;
                let room = &rooms[room_id];
                let player1: Option<Arc<Mutex<WebSocketSink>>>;
                let player2: Option<Arc<Mutex<WebSocketSink>>>;
                {
                    let room = room.lock().await;
                    if !room.game.actions().is_empty() {
                        return Ok(())
                    }
                    player1 = room.write_streams[0].as_ref().map(|p| Arc::clone(&p));
                    player2 = room.write_streams[1].as_ref().map(|p| Arc::clone(&p));
                }
                reset_room(&rooms[room_id], conn).await?;
                let response = Response::GameCanceled;
                let response = Message::Text(serde_json::to_string(&response)?.into());
                if let Some(player1) = player1 {
                    let _ = player1.lock().await.send(response.clone()).await;
                    let _ = player1.lock().await.send(Message::Close(None)).await;
                }
                if let Some(player2) = player2 {
                    let _ = player2.lock().await.send(response).await;
                    let _ = player2.lock().await.send(Message::Close(None)).await;
                }
                Ok::<_, anyhow::Error>(())
            });
            Ok(())
        }
        _ => Ok(())
    }
}

struct Auth {
    write: Arc<Mutex<WebSocketSink>>,
    verifier: Arc<RemoteJwksVerifier>,
    rooms: Arc<Vec<Mutex<Room>>>,
    conn: redis::aio::ConnectionManager,
    clock: Instant
}

#[derive(Deserialize)]
struct CustomClaims {
    account_id: String
}

impl Auth {
    fn new(
        write: Arc<Mutex<WebSocketSink>>,
        verifier: Arc<RemoteJwksVerifier>,
        rooms: Arc<Vec<Mutex<Room>>>,
        conn: redis::aio::ConnectionManager,
        clock: Instant
    ) -> Auth {
        Auth {
            write,
            verifier,
            rooms,
            conn,
            clock
        }
    }

    async fn next(
        self,
        msg: Message
    ) -> Result<ServerState> {
        match msg {
            Message::Text(text) => {
                match self.verifier.verify::<CustomClaims>(&text).await {
                    Ok(header_and_claims) => {
                        let audience = &header_and_claims.claims().aud;
                        if audience.iter().find(|aud| *aud == "game-server").is_none() {
                            return Ok(ServerState::Auth(self));
                        }
                        let response = Response::TokenValidated;
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        Ok(ServerState::Lobby(Lobby {
                            account_id: header_and_claims.claims().extra.account_id.clone(),
                            write: self.write,
                            rooms: self.rooms,
                            conn: self.conn,
                            clock: self.clock
                        }))
                    }
                    Err(err) => {
                        let response = Response::InvalidToken { reason: err.to_string() };
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        Ok(ServerState::Auth(self))
                    }
                }
            }
            _ => {
                Ok(ServerState::Auth(self))
            }
        }
    }
}

struct Lobby {
    account_id: String,
    write: Arc<Mutex<WebSocketSink>>,
    rooms: Arc<Vec<Mutex<Room>>>,
    conn: redis::aio::ConnectionManager,
    clock: Instant
}

impl Lobby {
    async fn next(
        self,
        msg: Message
    ) -> Result<ServerState> {
        match msg {
            Message::Text(text) => {
                match serde_json::from_str::<Request>(&text) {
                    Ok(Request::JoinRoom { room, key }) => {
                        let room_id = room as usize;
                        let room = &self.rooms[room_id];
                        let color: Color;
                        let prev_write: Option<Arc<Mutex<WebSocketSink>>>;
                        let current_position: String;
                        let start_time: u64;
                        {
                            let mut room = room.lock().await;
                            if !room.active {
                                drop(room);
                                let response = Response::RoomNotActive;
                                let response = Message::Text(serde_json::to_string(&response)?.into());
                                self.write.lock().await.send(response).await?;
                                return Ok(ServerState::Lobby(self));
                            }
                            if Some(key) == room.keys.as_ref().map(|k| k.keys[0]) {
                                color = Color::White;
                            } else if Some(key) == room.keys.as_ref().map(|k| k.keys[1]) {
                                color = Color::Black;
                            } else {
                                drop(room);
                                let response = Response::InvalidKey;
                                let response = Message::Text(serde_json::to_string(&response)?.into());
                                self.write.lock().await.send(response).await?;
                                return Ok(ServerState::Lobby(self));
                            }
                            prev_write = room.write_streams[color.to_index()].clone();
                            room.players[color.to_index()] = Some(Player {
                                account_id: self.account_id.clone()
                            });
                            room.write_streams[color.to_index()] = Some(self.write.clone());
                            current_position = format!("{}", room.game.current_position());
                            start_time = room.start_time;
                        }

                        if let Some(prev_write) = prev_write {
                            let _ = prev_write.lock().await.send(Message::Close(None)).await;
                        }

                        let result: Result<()> = async {
                            let response = Response::Connected;
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            self.write.lock().await.send(response).await?;

                            let color_str = if color == Color::White { "white" } else { "black" };
                            let response = Response::Color { color: color_str.into() };
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            self.write.lock().await.send(response).await?;

                            let response = Response::Fen {
                                fen: current_position.clone(),
                                timestamp: start_time,
                                white_time: 20000,
                                black_time: 0
                            };
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            self.write.lock().await.send(response).await?;

                            Ok(())
                        }.await;

                        match result {
                            Ok(_) => {}
                            Err(err) => {
                                let mut room = room.lock().await;
                                let room_keys = room.keys.as_ref();
                                if let Some(room_keys) = room_keys
                                    && room_keys.keys[color.to_index()] != key {
                                    return Err(err);
                                }
                                room.write_streams[color.to_index()] = None;
                                if room.write_streams[0].is_none()
                                    && room.write_streams[1].is_none() {
                                    drop(room);
                                    reset_room(&self.rooms[room_id], self.conn).await?;
                                }
                                return Err(err);
                            }
                        }

                        Ok(ServerState::Game(GameInner {
                            key: key,
                            write: self.write,
                            rooms: self.rooms,
                            room_id,
                            color,
                            conn: self.conn,
                            clock: self.clock
                        }))
                    }
                    Ok(Request::TimeSync) => {
                        let elapsed = self.clock.elapsed().as_millis();
                        let elapsed: u64 = elapsed.try_into()?;
                        let response = Response::TimeSync { timestamp: elapsed };
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        Ok(ServerState::Lobby(self))
                    }
                    Err(err) => {
                        let response = Response::InvalidRequest { reason: err.to_string() };
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        Ok(ServerState::Lobby(self))
                    }
                }
            }
            _ => {
                Ok(ServerState::Lobby(self))
            }
        }
    }
}

struct GameInner {
    key: u64,
    write: Arc<Mutex<WebSocketSink>>,
    rooms: Arc<Vec<Mutex<Room>>>,
    room_id: usize,
    color: Color,
    conn: redis::aio::ConnectionManager,
    clock: Instant
}

impl GameInner {
    async fn try_next(
        self,
        msg: Message
    ) -> Result<ServerState> {
        match msg {
            Message::Text(text) => {
                let timestamp: u64;
                let white_time: u64;
                let black_time: u64;
                let current_position: String;
                let opponent_write: Option<Arc<Mutex<WebSocketSink>>>;
                let game_result: Option<GameResult>;
                {
                    let mut room = self.rooms[self.room_id].lock().await;

                    let is_first_move = room.game.actions().is_empty();

                    let now = self.clock.elapsed().as_millis();
                    let now: u64 = now.try_into()?;
                    let remaining_time = if is_first_move {
                        room.remaining_time[self.color.to_index()]
                    } else {
                        let elapsed = now - room.last_move_time;
                        let remaining_time = room.remaining_time[self.color.to_index()];
                        if elapsed > 2000 {
                            remaining_time.saturating_sub(elapsed - 2000)
                        } else {
                            remaining_time + 2000 - elapsed
                        }
                    };

                    if remaining_time == 0 {
                        drop(room);
                        return Ok(ServerState::Game(self));
                    }
                    
                    let room_key = room.keys.as_ref().map(|k| k.keys[self.color.to_index()]);
                    if room_key != Some(self.key) {
                        drop(room);
                        let response = Response::InvalidKey;
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        return Err(anyhow!("Player session invalid"));
                    }
                    if room.game.side_to_move() != self.color {
                        drop(room);
                        let response = Response::OutOfTurnMove;
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        return Ok(ServerState::Game(self));
                    }

                    let chess_move = ChessMove::from_str(&text);
                    let move_successful = {
                        match chess_move {
                            Ok(mv) => room.game.make_move(mv),
                            Err(_) => false
                        }
                    };
                    if !move_successful {
                        drop(room);
                        let response = Response::InvalidMove;
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        return Ok(ServerState::Game(self));
                    }

                    if let Some(player_timeout) = room.player_timeout.take() {
                        player_timeout.abort();
                    }

                    room.remaining_time[self.color.to_index()] = remaining_time;
                    room.last_move_time = now;
                    timestamp = room.last_move_time;
                    white_time = room.remaining_time[0];
                    black_time = room.remaining_time[1];

                    if room.game.can_declare_draw() {
                        room.game.declare_draw();
                    }

                    current_position = format!("{}", &room.game.current_position());
                    opponent_write = room.write_streams[self.color.to_index() ^ 1]
                        .as_ref()
                        .map(|p| Arc::clone(&p));
                    game_result = room.game.result();

                    if game_result.is_none() {
                        let remaining_time = room.remaining_time[self.color.to_index() ^ 1];
                        let color = self.color;
                        let rooms = Arc::clone(&self.rooms);
                        let room_id = self.room_id;
                        let write = Arc::clone(&self.write);
                        let opponent_write = opponent_write.clone();
                        let conn = self.conn.clone();
                        let handle = tokio::spawn(async move {
                            tokio::time::sleep(tokio::time::Duration::from_millis(remaining_time)).await;
                            rooms[room_id].lock().await.player_timeout = None;
                            let winner = if color == Color::White { "white" } else { "black" };
                            let response = Response::GameOver {
                                winner: winner.to_string(),
                                timeout: true
                            };
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            let _ = write.lock().await.send(response.clone()).await;
                            let _ = write.lock().await.send(Message::Close(None)).await;
                            if let Some(opponent_write) = opponent_write {
                                let _ = opponent_write.lock().await.send(response).await;
                                let _ = opponent_write.lock().await.send(Message::Close(None)).await;
                            }
                            let _ = upload_match(&rooms[room_id]).await;
                            reset_room(&rooms[room_id], conn).await?;
                            Ok::<_, anyhow::Error>(())
                        });
                        room.player_timeout = Some(handle);
                    }
                }

                let response = Response::Fen {
                    fen: current_position.clone(),
                    timestamp,
                    white_time,
                    black_time
                };
                let response = Message::Text(serde_json::to_string(&response)?.into());
                let _ = self.write.lock().await.send(response).await;
                if let Some(opponent_write) = opponent_write.clone() {
                    let response = Response::Move { move_: text.to_string() };
                    let response = Message::Text(serde_json::to_string(&response)?.into());
                    let _ = opponent_write.lock().await.send(response).await;

                    let response = Response::Fen {
                        fen: current_position.clone(),
                        timestamp,
                        white_time,
                        black_time
                    };
                    let response = Message::Text(serde_json::to_string(&response)?.into());
                    let _ = opponent_write.lock().await.send(response).await;
                }

                if let Some(game_result) = game_result {
                    let response = match game_result {
                        GameResult::WhiteCheckmates => Some(Response::GameOver {
                            winner: "white".to_string(),
                            timeout: false
                        }),
                        GameResult::BlackCheckmates => Some(Response::GameOver {
                            winner: "black".to_string(),
                            timeout: false
                        }),
                        GameResult::Stalemate => Some(Response::GameOver {
                            winner: "draw".to_string(),
                            timeout: false
                        }),
                        GameResult::DrawDeclared => Some(Response::GameOver {
                            winner: "draw".to_string(),
                            timeout: false
                        }),
                        _ => None
                    };
                    if let Some(response) = response {
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        let _ = self.write.lock().await.send(response.clone()).await;
                        let _ = self.write.lock().await.send(Message::Close(None)).await;
                        if let Some(opponent_write) = opponent_write {
                            let _ = opponent_write.lock().await.send(response).await;
                            let _ = opponent_write.lock().await.send(Message::Close(None)).await;
                        }
                        let _ = upload_match(&self.rooms[self.room_id]).await;
                        reset_room(&self.rooms[self.room_id], self.conn.clone()).await?;
                    }
                }

                Ok(ServerState::Game(self))
            }
            _ => {
                Ok(ServerState::Game(self))
            }
        }
    }

    async fn next(
        self,
        msg: Message
    ) -> Result<ServerState> {
        let rooms = Arc::clone(&self.rooms);
        let key = self.key.clone();
        let room_id = self.room_id;
        let color = self.color;
        let conn = self.conn.clone();
        match self.try_next(msg).await {
            Ok(state) => Ok(state),
            Err(err) => {
                let mut room = rooms[room_id].lock().await;
                let room_keys = room.keys.as_ref();
                if let Some(room_keys) = room_keys
                    && room_keys.keys[color.to_index()] != key {
                    return Err(err);
                }
                room.write_streams[color.to_index()] = None;
                if room.write_streams[0].is_none() && room.write_streams[1].is_none() {
                    drop(room);
                    reset_room(&rooms[room_id], conn).await?
                }
                Err(err)
            }
        }
    }

    async fn exit(self) -> Result<()> {
        let mut room = self.rooms[self.room_id].lock().await;
        let room_keys = room.keys.as_ref();
        if let Some(room_keys) = room_keys
            && room_keys.keys[self.color.to_index()] != self.key {
            return Ok(());
        }
        room.write_streams[self.color.to_index()] = None;
        if room.write_streams[0].is_none() && room.write_streams[1].is_none() {
            drop(room);
            reset_room(&self.rooms[self.room_id], self.conn).await?;
        }
        Ok(())
    }
}

enum ServerState {
    Auth(Auth),
    Lobby(Lobby),
    Game(GameInner),
}

async fn handle_connection(
    stream: TcpStream,
    rooms: Arc<Vec<Mutex<Room>>>,
    verifier: Arc<RemoteJwksVerifier>,
    conn: redis::aio::ConnectionManager,
    clock: Instant
) -> Result<()> {
    let (write, mut read) = accept_async(stream).await?.split();
    let write = Arc::new(Mutex::new(write));

    let mut hearbeat_interval = interval(Duration::from_secs(25));
    let mut state = ServerState::Auth(Auth::new(
        Arc::clone(&write),
        verifier,
        rooms,
        conn,
        clock
    ));
    loop {
        tokio::select! {
            _ = hearbeat_interval.tick() => {
                write.lock().await.send(Message::Ping(vec![].into())).await?;
            }
            msg = timeout(Duration::from_secs(60), read.next()) => {
                match msg {
                    Ok(Some(Ok(msg))) => {
                        match state {
                            ServerState::Auth(auth) => {
                                state = auth.next(msg).await?;
                            }
                            ServerState::Lobby(lobby) => {
                                state = lobby.next(msg).await?;
                            }
                            ServerState::Game(game) => {
                                state = game.next(msg).await?;
                            }
                        }
                    },
                    Ok(Some(Err(e))) => return Err(e.into()),
                    Ok(None) => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    match state {
        ServerState::Game(game) => game.exit().await?,
        _ => (),
    }

    Ok(())
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use tokio_tungstenite::connect_async;
//     use serial_test::serial;
//     use tokio::sync::Barrier;
//     #[tokio::test]
//     #[serial]
//     async fn test_join_room() {
//         let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
//             players: [None, None],
//             game: Game::new()
//         })).collect();
//         let rooms = Arc::new(rooms);
//         tokio::spawn(listen_for_connections(Arc::clone(&rooms)));
//         let handle = tokio::spawn(async {
//             let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
// 
//             // ignore first message (ping)
//             let _ = stream.next().await.unwrap().unwrap();
// 
//             stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test\"}".into())).await.unwrap();
// 
//             let response = stream.next().await.unwrap().unwrap();
//             assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");
//         });
//         assert!(handle.await.is_ok());
//     }
//     
//     #[tokio::test]
//     #[serial]
//     async fn test_full_room() {
//         let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
//             players: [None, None],
//             game: Game::new()
//         })).collect();
//         let rooms = Arc::new(rooms);
//         tokio::spawn(listen_for_connections(Arc::clone(&rooms)));
// 
//         let barrier = Arc::new(Barrier::new(3));
// 
//         let thread_barrier = Arc::clone(&barrier);
//         let handle = tokio::spawn(async move {
//             let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
// 
//             // ignore first message (ping)
//             let _ = stream.next().await.unwrap().unwrap();
// 
//             stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test\"}".into())).await.unwrap();
// 
//             thread_barrier.wait().await;
//             let response = stream.next().await.unwrap().unwrap();
//             assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");
//         });
// 
//         let thread_barrier = Arc::clone(&barrier);
//         let handle2 = tokio::spawn(async move {
//             let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
// 
//             // ignore first message (ping)
//             let _ = stream.next().await.unwrap().unwrap();
// 
//             stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test2\"}".into())).await.unwrap();
// 
//             thread_barrier.wait().await;
//             let response = stream.next().await.unwrap().unwrap();
//             assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");
//         });
// 
//         let thread_barrier = Arc::clone(&barrier);
//         let handle3 = tokio::spawn(async move {
//             let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
// 
//             // ignore first message (ping)
//             let _ = stream.next().await.unwrap().unwrap();
// 
//             thread_barrier.wait().await;
//             stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test3\"}".into())).await.unwrap();
// 
//             let response = stream.next().await.unwrap().unwrap();
//             assert_eq!(response.to_text().unwrap(), "{\"type\":\"room_full\"}");
//         });
// 
//         assert!(handle.await.is_ok());
//         assert!(handle2.await.is_ok());
//         assert!(handle3.await.is_ok());
//     }
// 
//     #[tokio::test]
//     #[serial]
//     async fn test_make_moves() {
//         let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
//             players: [None, None],
//             game: Game::new()
//         })).collect();
//         let rooms = Arc::new(rooms);
//         tokio::spawn(listen_for_connections(Arc::clone(&rooms)));
// 
//         let barrier = Arc::new(Barrier::new(2));
//         async fn make_move(uuid: &str, barrier: Arc<Barrier>) {
//             let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
// 
//             // ignore first message (ping)
//             let _ = stream.next().await.unwrap().unwrap();
// 
//             stream.send(Message::Text(format!("{{\"room\": 0,\"uuid\":\"{uuid}\"}}").into())).await.unwrap();
// 
//             let response = stream.next().await.unwrap().unwrap();
//             assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");
// 
//             let response = stream.next().await.unwrap().unwrap();
//             let response = serde_json::from_str::<Response>(response.to_text().unwrap()).unwrap();
//             let color = match response {
//                 Response::Color { color } => color,
//                 _ => panic!("Unexpected response type for color"),
//             };
// 
//             let response = stream.next().await.unwrap().unwrap();
//             assert_eq!(
//                 response.to_text().unwrap(),
//                 "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\"}"
//             );
//             if color == "white" {
//                 barrier.wait().await;
//                 stream.send(Message::Text("e2e4".into())).await.unwrap();
// 
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(
//                     response.to_text().unwrap(),
//                     "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1\"}"
//                 );
// 
//                 stream.send(Message::Text("e7e5".into())).await.unwrap();
// 
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(response.to_text().unwrap(), "{\"type\":\"out_of_turn_move\"}");
// 
//                 barrier.wait().await;
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(response.to_text().unwrap(), "{\"type\":\"move\",\"move\":\"e7e5\"}");
// 
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(
//                     response.to_text().unwrap(),
//                     "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 1\"}"
//                 );
//             } else if color == "black" {
//                 stream.send(Message::Text("e2e4".into())).await.unwrap();
// 
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(response.to_text().unwrap(), "{\"type\":\"out_of_turn_move\"}");
// 
//                 barrier.wait().await;
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(response.to_text().unwrap(), "{\"type\":\"move\",\"move\":\"e2e4\"}");
// 
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(
//                     response.to_text().unwrap(),
//                     "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1\"}"
//                 );
// 
//                 barrier.wait().await;
//                 stream.send(Message::Text("e7e5".into())).await.unwrap();
// 
//                 let response = stream.next().await.unwrap().unwrap();
//                 assert_eq!(
//                     response.to_text().unwrap(),
//                     "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 1\"}"
//                 );
//             } else {
//                 panic!("invalid color");
//             }
//         }
//         let handle = tokio::spawn(make_move("test", Arc::clone(&barrier)));
//         let handle2 = tokio::spawn(make_move("test2", Arc::clone(&barrier)));
//         assert!(handle.await.is_ok());
//         assert!(handle2.await.is_ok());
//     }
// }

