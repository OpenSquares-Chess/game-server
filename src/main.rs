use chess::{Game, ChessMove, Color};
use tokio::net::{TcpStream, TcpListener};
use tokio::sync::Mutex;
use tokio::time::{interval, timeout, Duration};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{StreamExt, SinkExt};
use jwtk::jwk::RemoteJwksVerifier;
use std::sync::Arc;
use std::str::FromStr;
use rand::Rng;
use anyhow::{anyhow, Result};

mod messages;
use messages::{received::ConnectionRequest, response::Response};

struct Player {
    _uuid: String,
    write_stream: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>
}

struct Room {
    players: [Option<Player>; 2],
    game: Game
}

#[tokio::main]
async fn main() -> Result<()> {
    let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
        players: [None, None],
        game: Game::new()
    })).collect();
    let rooms = Arc::new(rooms);

    let jwks_url = "https://auth.opensquares.xyz/realms/opensquares/protocol/openid-connect/certs".to_string();
    let cache_duration = Duration::from_secs(3600);
    let verifier = RemoteJwksVerifier::new(jwks_url, None, cache_duration);
    let verifier = Arc::new(verifier);

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

async fn handle_game(
    msg: Message,
    write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    room: &Mutex<Room>,
    color: Color
) -> Result<()> {
    if msg.is_text() {
        let received_text = msg.to_text()?;
        let current_position: String;
        let opponent_write: Option<Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>>;
        {
            let mut room = room.lock().await;
            if room.game.side_to_move() != color {
                drop(room);
                let response = Response::OutOfTurnMove;
                let response = Message::Text(serde_json::to_string(&response)?.into());
                write.lock().await.send(response).await?;
                return Ok(());
            }
            let chess_move = ChessMove::from_str(&received_text);
            if chess_move.is_err() || !room.game.make_move(chess_move.unwrap()) {
                drop(room);
                let response = Response::InvalidMove;
                let response = Message::Text(serde_json::to_string(&response)?.into());
                write.lock().await.send(response).await?;
                return Ok(());
            }
            current_position = format!("{}", &room.game.current_position());
            opponent_write = room.players[color.to_index() ^ 1].as_ref().map(|p| Arc::clone(&p.write_stream));
        }
        let response = Response::Fen { fen: current_position.clone() };
        let response = Message::Text(serde_json::to_string(&response)?.into());
        write.lock().await.send(response).await?;
        if let Some(opponent_write) = opponent_write {
            let response = Response::Move { move_: received_text.into() };
            let response = Message::Text(serde_json::to_string(&response)?.into());
            opponent_write.lock().await.send(response).await?;

            let response = Response::Fen { fen: current_position.clone() };
            let response = Message::Text(serde_json::to_string(&response)?.into());
            opponent_write.lock().await.send(response).await?;
        }
    }

    Ok(())
}

async fn handle_lobby(
    msg: Message,
    heartbeat_interval: &mut tokio::time::Interval,
    read: &mut SplitStream<WebSocketStream<TcpStream>>,
    write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    rooms: Arc<Vec<Mutex<Room>>>,
    sub_id: String
) -> Result<()> {
    if msg.is_text() {
        let received_text = msg.to_text()?;
        match serde_json::from_str::<ConnectionRequest>(&received_text) {
            Ok(request) => {
                let room_id = request.room as usize;
                let room = &rooms[room_id];
                let color: Color;
                let current_position: String;
                {
                    let mut room = room.lock().await;
                    match (room.players[0].is_some(), room.players[1].is_some()) {
                        (true, true) => {
                            drop(room);
                            let response = Response::RoomFull;
                            let response = Message::Text(serde_json::to_string(&response)?.into());
                            write.lock().await.send(response).await?;
                            return Ok(());
                        }
                        (true, false) => {
                            color = Color::Black;
                        }
                        (false, true) => {
                            color = Color::White;
                        }
                        (false, false) => {
                            color = if rand::rng().random_bool(0.5) { Color::White } else { Color::Black };
                        }
                    }
                    room.players[color.to_index()] = Some(Player {
                        _uuid: sub_id.clone(),
                        write_stream: Arc::clone(&write)
                    });
                    current_position = format!("{}", &room.game.current_position());
                }

                let _ = async || -> Result<()> {
                    let reponse = Response::Connected;
                    let reponse = Message::Text(serde_json::to_string(&reponse)?.into());
                    write.lock().await.send(reponse).await?;

                    let color_str = if color == Color::White { "white" } else { "black" };
                    let response = Response::Color { color: color_str.into() };
                    let response = Message::Text(serde_json::to_string(&response)?.into());
                    write.lock().await.send(response).await?;

                    let response = Response::Fen { fen: current_position.clone() };
                    let response = Message::Text(serde_json::to_string(&response)?.into());
                    write.lock().await.send(response).await?;

                    loop {
                        tokio::select! {
                            _ = heartbeat_interval.tick() => {
                                write.lock().await.send(Message::Ping(vec![].into())).await?;
                            }
                            msg = timeout(Duration::from_secs(60), read.next()) => {
                                match msg {
                                    Ok(Some(Ok(msg))) => handle_game(
                                        msg,
                                        Arc::clone(&write),
                                        room,
                                        color
                                    ).await?,
                                    _ => break
                                }
                            }
                        }
                    }

                    Ok(())
                }().await;

                {
                    let mut room = room.lock().await;
                    room.players[color.to_index()] = None;
                    if room.players[0].is_none() && room.players[1].is_none() {
                        room.game = Game::new();
                    }
                }
            }
            Err(_e) => {
                let response = Response::InvalidRequest;
                let response = Message::Text(serde_json::to_string(&response)?.into());
                write.lock().await.send(response).await?;
            }
        }
    }

    Ok(())
}

async fn handle_auth(
    msg: Message,
    write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    verifier: Arc<RemoteJwksVerifier>
) -> Result<String> {
    if msg.is_text() {
        match verifier.verify::<()>(msg.to_text()?).await {
            Ok(header_and_claims) => {
                let audience = &header_and_claims.claims().aud;
                if audience.iter().find(|aud| *aud == "game-server").is_none() {
                    write.lock().await.send(Message::Text("invalid audience".into())).await?;
                    return Err(anyhow!("invalid audience"));
                }
                let response = Response::TokenValidated;
                let response = Message::Text(serde_json::to_string(&response)?.into());
                write.lock().await.send(response).await?;
                return Ok(header_and_claims.claims().sub.clone().expect("sub not found"));
            }
            Err(e) => {
                let response = Response::InvalidToken { reason: e.to_string() };
                let response = Message::Text(serde_json::to_string(&response)?.into());
                write.lock().await.send(response).await?;
                return Err(e.into());
            }
        }
    }

    Err(anyhow!("message not text"))
}

async fn handle_connection(
    stream: TcpStream,
    rooms: Arc<Vec<Mutex<Room>>>,
    verifier: Arc<RemoteJwksVerifier>
) -> Result<()> {
    let (write, mut read) = accept_async(stream).await?.split();
    let write = Arc::new(Mutex::new(write));

    let mut hearbeat_interval = interval(Duration::from_secs(25));

    let sub_id;
    loop {
        tokio::select! {
            _ = hearbeat_interval.tick() => {
                write.lock().await.send(Message::Ping(vec![].into())).await?;
            }
            msg = timeout(Duration::from_secs(60), read.next()) => {
                match msg {
                    Ok(Some(Ok(msg))) => {
                        let result = handle_auth(
                            msg,
                            Arc::clone(&write),
                            Arc::clone(&verifier)
                        ).await;
                        if result.is_ok() {
                            sub_id = result.unwrap();
                            break;
                        }
                    },
                    _ => return Ok(()),
                }
            }
        }
    }

    loop {
        tokio::select! {
            _ = hearbeat_interval.tick() => {
                write.lock().await.send(Message::Ping(vec![].into())).await?;
            }
            msg = timeout(Duration::from_secs(60), read.next()) => {
                match msg {
                    Ok(Some(Ok(msg))) => handle_lobby(
                        msg,
                        &mut hearbeat_interval,
                        &mut read,
                        Arc::clone(&write),
                        Arc::clone(&rooms),
                        sub_id.clone()
                    ).await?,
                    _ => break,
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::connect_async;
    use serial_test::serial;
    use tokio::sync::Barrier;
    #[tokio::test]
    #[serial]
    async fn test_join_room() {
        let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
            players: [None, None],
            game: Game::new()
        })).collect();
        let rooms = Arc::new(rooms);
        tokio::spawn(listen_for_connections(Arc::clone(&rooms)));
        let handle = tokio::spawn(async {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();

            // ignore first message (ping)
            let _ = stream.next().await.unwrap().unwrap();

            stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test\"}".into())).await.unwrap();

            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");
        });
        assert!(handle.await.is_ok());
    }
    
    #[tokio::test]
    #[serial]
    async fn test_full_room() {
        let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
            players: [None, None],
            game: Game::new()
        })).collect();
        let rooms = Arc::new(rooms);
        tokio::spawn(listen_for_connections(Arc::clone(&rooms)));

        let barrier = Arc::new(Barrier::new(3));

        let thread_barrier = Arc::clone(&barrier);
        let handle = tokio::spawn(async move {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();

            // ignore first message (ping)
            let _ = stream.next().await.unwrap().unwrap();

            stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test\"}".into())).await.unwrap();

            thread_barrier.wait().await;
            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");
        });

        let thread_barrier = Arc::clone(&barrier);
        let handle2 = tokio::spawn(async move {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();

            // ignore first message (ping)
            let _ = stream.next().await.unwrap().unwrap();

            stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test2\"}".into())).await.unwrap();

            thread_barrier.wait().await;
            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");
        });

        let thread_barrier = Arc::clone(&barrier);
        let handle3 = tokio::spawn(async move {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();

            // ignore first message (ping)
            let _ = stream.next().await.unwrap().unwrap();

            thread_barrier.wait().await;
            stream.send(Message::Text("{\"room\": 0,\"uuid\":\"test3\"}".into())).await.unwrap();

            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "{\"type\":\"room_full\"}");
        });

        assert!(handle.await.is_ok());
        assert!(handle2.await.is_ok());
        assert!(handle3.await.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_make_moves() {
        let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room {
            players: [None, None],
            game: Game::new()
        })).collect();
        let rooms = Arc::new(rooms);
        tokio::spawn(listen_for_connections(Arc::clone(&rooms)));

        let barrier = Arc::new(Barrier::new(2));
        async fn make_move(uuid: &str, barrier: Arc<Barrier>) {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();

            // ignore first message (ping)
            let _ = stream.next().await.unwrap().unwrap();

            stream.send(Message::Text(format!("{{\"room\": 0,\"uuid\":\"{uuid}\"}}").into())).await.unwrap();

            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "{\"type\":\"connected\"}");

            let response = stream.next().await.unwrap().unwrap();
            let response = serde_json::from_str::<Response>(response.to_text().unwrap()).unwrap();
            let color = match response {
                Response::Color { color } => color,
                _ => panic!("Unexpected response type for color"),
            };

            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(
                response.to_text().unwrap(),
                "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\"}"
            );
            if color == "white" {
                barrier.wait().await;
                stream.send(Message::Text("e2e4".into())).await.unwrap();

                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(
                    response.to_text().unwrap(),
                    "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1\"}"
                );

                stream.send(Message::Text("e7e5".into())).await.unwrap();

                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(response.to_text().unwrap(), "{\"type\":\"out_of_turn_move\"}");

                barrier.wait().await;
                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(response.to_text().unwrap(), "{\"type\":\"move\",\"move\":\"e7e5\"}");

                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(
                    response.to_text().unwrap(),
                    "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 1\"}"
                );
            } else if color == "black" {
                stream.send(Message::Text("e2e4".into())).await.unwrap();

                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(response.to_text().unwrap(), "{\"type\":\"out_of_turn_move\"}");

                barrier.wait().await;
                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(response.to_text().unwrap(), "{\"type\":\"move\",\"move\":\"e2e4\"}");

                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(
                    response.to_text().unwrap(),
                    "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1\"}"
                );

                barrier.wait().await;
                stream.send(Message::Text("e7e5".into())).await.unwrap();

                let response = stream.next().await.unwrap().unwrap();
                assert_eq!(
                    response.to_text().unwrap(),
                    "{\"type\":\"fen\",\"fen\":\"rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 1\"}"
                );
            } else {
                panic!("invalid color");
            }
        }
        let handle = tokio::spawn(make_move("test", Arc::clone(&barrier)));
        let handle2 = tokio::spawn(make_move("test2", Arc::clone(&barrier)));
        assert!(handle.await.is_ok());
        assert!(handle2.await.is_ok());
    }
}


