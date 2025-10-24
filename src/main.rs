use chess::{Game, ChessMove, Color};
use tokio::net::{TcpStream, TcpListener};
use tokio::sync::Mutex;
use tokio::time::{interval, timeout, Duration};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use futures_util::stream::SplitSink;
use futures_util::{StreamExt, SinkExt};
use jwtk::jwk::RemoteJwksVerifier;
use std::sync::Arc;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use rand::Rng;
use anyhow::{anyhow, Result};
use serde::{Serialize, Deserialize};
use serde_with::{serde_as, DisplayFromStr};

mod messages;
use messages::{received::ConnectionRequest, response::Response};

#[derive(Clone)]
struct Player {
    _uuid: String,
    write_stream: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>
}

#[serde_as]
#[derive(Serialize, Deserialize)]
struct Keys {
    #[serde_as(as = "[DisplayFromStr; 2]")]
    keys: [u64; 2],
    timestamp: u64
} 

struct Room {
    active: bool,
    players: [Option<Player>; 2],
    game: Game,
    keys: Option<Keys>
}

#[tokio::main]
async fn main() -> Result<()> {
    let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
        active: false,
        players: [None, None],
        game: Game::new(),
        keys: None
    })).collect();
    let rooms = Arc::new(rooms);

    let jwks_url = "https://auth.opensquares.xyz/realms/opensquares/protocol/openid-connect/certs".to_string();
    let cache_duration = Duration::from_secs(3600);
    let verifier = RemoteJwksVerifier::new(jwks_url, None, cache_duration);
    let verifier = Arc::new(verifier);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let client = redis::Client::open("redis://host.docker.internal:6379/?protocol=resp3")?;
    let config = redis::aio::ConnectionManagerConfig::new()
        .set_automatic_resubscription()
        .set_push_sender(tx);
    let mut conn = client.get_connection_manager_with_config(config).await?;

    broadcast_available_rooms(Arc::clone(&rooms), conn.clone()).await?;

    let rooms_ref = Arc::clone(&rooms);
    let conn_copy = conn.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let _ = handle_match_found(msg, Arc::clone(&rooms_ref), conn_copy.clone()).await;
        }
    });
    conn.subscribe("matchmaking:game").await?;

    listen_for_connections(rooms, verifier).await?;

    Ok(())
}

async fn listen_for_connections(
    rooms: Arc<Vec<Mutex<Room>>>,
    verifier: Arc<RemoteJwksVerifier>
) -> Result<()> {
    let addr = "0.0.0.0:8080".to_string();
    let listener = TcpListener::bind(&addr).await?;
    println!("Connection listener started on http://{}", addr);
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(handle_connection(stream, Arc::clone(&rooms), Arc::clone(&verifier)));
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
        let script = redis::Script::new(r#"
            local exists = redis.call('LPOS', KEYS[1], ARGV[1])
            if not exists then
                redis.call('RPUSH', KEYS[1], ARGV[1])
                return 1
            else
                return 0
            end
        "#);
        let key = format!("rooms");
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
    mut conn: redis::aio::ConnectionManager
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
                    let target: u64 = timestamp + 20000;
                    delay = if now < target {
                        std::time::Duration::from_millis(target - now)
                    } else {
                        std::time::Duration::from_millis(0)
                    }
                }
                tokio::time::sleep(delay).await;
                let player1: Option<Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>>;
                let player2: Option<Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>>;
                let keys: String;
                {
                    let mut room = rooms[room_id].lock().await;
                    if !room.game.actions().is_empty() {
                        return Ok(())
                    }
                    room.active = false;
                    room.keys = Some(Keys {
                        keys: generate_chess_keys(),
                        timestamp: 0
                    });
                    keys = serde_json::to_string(&room.keys)?;
                    room.game = Game::new();
                    player1 = room.players[0].as_ref().map(|p| Arc::clone(&p.write_stream));
                    player2 = room.players[1].as_ref().map(|p| Arc::clone(&p.write_stream));
                    room.players = [None, None];
                }
                let mut cmd = redis::cmd("SET");
                cmd.arg(format!("room:{}:keys", room_id)).arg(keys);
                let _: bool = cmd.query_async(&mut conn).await?;
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
    write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    verifier: Arc<RemoteJwksVerifier>,
    rooms: Arc<Vec<Mutex<Room>>>
}

impl Auth {
    fn new(
        write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
        verifier: Arc<RemoteJwksVerifier>,
        rooms: Arc<Vec<Mutex<Room>>>
    ) -> Auth {
        Auth {
            write,
            verifier,
            rooms
        }
    }

    async fn next(
        self,
        msg: Message
    ) -> Result<ServerState> {
        match msg {
            Message::Text(text) => {
                match self.verifier.verify::<()>(&text).await {
                    Ok(header_and_claims) => {
                        let audience = &header_and_claims.claims().aud;
                        if audience.iter().find(|aud| *aud == "game-server").is_none() {
                            return Ok(ServerState::Auth(self));
                        }
                        let response = Response::TokenValidated;
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        Ok(ServerState::Lobby(Lobby {
                            sub_id: header_and_claims.claims().sub.clone().expect("sub not found"),
                            write: self.write,
                            rooms: self.rooms
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
    sub_id: String,
    write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    rooms: Arc<Vec<Mutex<Room>>>
}

impl Lobby {
    async fn next(
        self,
        msg: Message
    ) -> Result<ServerState> {
        match msg {
            Message::Text(text) => {
                match serde_json::from_str::<ConnectionRequest>(&text) {
                    Ok(request) => {
                        let room_id = request.room as usize;
                        let room = &self.rooms[room_id];
                        let color: Color;
                        let current_position: String;
                        let prev_player: Option<Player>;
                        {
                            let mut room = room.lock().await;
                            if !room.active {
                                drop(room);
                                let response = Response::RoomNotActive;
                                let response = Message::Text(serde_json::to_string(&response)?.into());
                                self.write.lock().await.send(response).await?;
                                return Ok(ServerState::Lobby(self));
                            }
                            if Some(request.key) == room.keys.as_ref().map(|k| k.keys[0]) {
                                color = Color::White;
                            } else if Some(request.key) == room.keys.as_ref().map(|k| k.keys[1]) {
                                color = Color::Black;
                            } else {
                                drop(room);
                                let response = Response::InvalidKey;
                                let response = Message::Text(serde_json::to_string(&response)?.into());
                                self.write.lock().await.send(response).await?;
                                return Ok(ServerState::Lobby(self));
                            }
                            prev_player = room.players[color.to_index()].clone();
                            room.players[color.to_index()] = Some(Player {
                                _uuid: self.sub_id.clone(),
                                write_stream: self.write.clone()
                            });
                            current_position = format!("{}", room.game.current_position());
                        }

                        if let Some(player) = prev_player {
                            let _ = player.write_stream.lock().await.send(Message::Close(None)).await;
                        }

                        let result: Result<()> = async {
                            let response = Response::Connected;
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            self.write.lock().await.send(response).await?;

                            let color_str = if color == Color::White { "white" } else { "black" };
                            let response = Response::Color { color: color_str.into() };
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            self.write.lock().await.send(response).await?;

                            let response = Response::Fen { fen: current_position.clone() };
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            self.write.lock().await.send(response).await?;

                            Ok(())
                        }.await;

                        match result {
                            Ok(_) => {}
                            Err(err) => {
                                let mut room = room.lock().await;
                                room.players[color.to_index()] = None;
                                if room.players[0].is_none() && room.players[1].is_none() {
                                    room.active = false;
                                    room.game = Game::new();
                                }
                                return Err(err);
                            }
                        }

                        Ok(ServerState::Game(GameInner {
                            sub_id: self.sub_id,
                            key: request.key,
                            write: self.write,
                            rooms: self.rooms,
                            room_id: room_id,
                            color
                        }))
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
    sub_id: String,
    key: u64,
    write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    rooms: Arc<Vec<Mutex<Room>>>,
    room_id: usize,
    color: Color
}

impl GameInner {
    async fn try_next(
        self,
        msg: Message
    ) -> Result<ServerState> {
        match msg {
            Message::Text(text) => {
                let current_position: String;
                let opponent_write: Option<Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>>;
                {
                    let mut room = self.rooms[self.room_id].lock().await;
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
                    if chess_move.is_err() || !room.game.make_move(chess_move.unwrap()) {
                        drop(room);
                        let response = Response::InvalidMove;
                        let response = Message::Text(serde_json::to_string(&response)?.into());
                        self.write.lock().await.send(response).await?;
                        return Ok(ServerState::Game(self));
                    }
                    current_position = format!("{}", &room.game.current_position());
                    opponent_write = room.players[self.color.to_index() ^ 1]
                        .as_ref()
                        .map(|p| Arc::clone(&p.write_stream));
                }
                let response = Response::Fen { fen: current_position.clone() };
                let response = Message::Text(serde_json::to_string(&response)?.into());
                self.write.lock().await.send(response).await?;
                if let Some(opponent_write) = opponent_write {
                    let response = Response::Move { move_: text.to_string() };
                    let response = Message::Text(serde_json::to_string(&response)?.into());
                    opponent_write.lock().await.send(response).await?;

                    let response = Response::Fen { fen: current_position.clone() };
                    let response = Message::Text(serde_json::to_string(&response)?.into());
                    opponent_write.lock().await.send(response).await?;
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
        let room_id = self.room_id;
        let color = self.color;
        match self.try_next(msg).await {
            Ok(state) => Ok(state),
            Err(err) => {
                let mut room = rooms[room_id].lock().await;
                room.players[color.to_index()] = None;
                if room.players[0].is_none() && room.players[1].is_none() {
                    room.active = false;
                    room.game = Game::new();
                }
                Err(err)
            }
        }
    }

    async fn exit(self) {
        let mut room = self.rooms[self.room_id].lock().await;
        room.players[self.color.to_index()] = None;
        if room.players[0].is_none() && room.players[1].is_none() {
            room.active = false;
            room.game = Game::new();
        }
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
    verifier: Arc<RemoteJwksVerifier>
) -> Result<()> {
    let (write, mut read) = accept_async(stream).await?.split();
    let write = Arc::new(Mutex::new(write));

    let mut hearbeat_interval = interval(Duration::from_secs(25));
    let mut state = ServerState::Auth(Auth::new(Arc::clone(&write), Arc::clone(&verifier), Arc::clone(&rooms)));
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
        ServerState::Game(game) => game.exit().await,
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

