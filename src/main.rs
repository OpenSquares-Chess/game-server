use chess::{Game, Board, ChessMove, Color, Piece, Square, Rank, File};
use tokio::net::{TcpStream, TcpListener};
use tokio::sync::Mutex;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{StreamExt, SinkExt};
use serde::Deserialize;
use std::sync::Arc;
use std::str::FromStr;
use rand::Rng;
use anyhow::Result;

#[derive(Deserialize)]
struct ConnectionRequest {
    room: u32,
    uuid: String
}

struct Player {
    uuid: String,
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
    listen_for_connections(Arc::clone(&rooms)).await?;

    Ok(())
}

async fn listen_for_connections(rooms: Arc<Vec<Mutex<Room>>>) -> Result<()> {
    let addr = "0.0.0.0:8080".to_string();
    let listener = TcpListener::bind(&addr).await?;
    println!("Connection listener started on http://{}", addr);
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(handle_connection(stream, Arc::clone(&rooms)));
    }

    Ok(())
}

fn pretty_board(board: &Board) -> String {
    fn piece_unicode(piece: &Piece, color: Color) -> char {
        match (piece, color) {
            (Piece::King, Color::White) => '♔',
            (Piece::Queen, Color::White) => '♕',
            (Piece::Rook, Color::White) => '♖',
            (Piece::Bishop, Color::White) => '♗',
            (Piece::Knight, Color::White) => '♘',
            (Piece::Pawn, Color::White) => '♙',
            (Piece::King, Color::Black) => '♚',
            (Piece::Queen, Color::Black) => '♛',
            (Piece::Rook, Color::Black) => '♜',
            (Piece::Bishop, Color::Black) => '♝',
            (Piece::Knight, Color::Black) => '♞',
            (Piece::Pawn, Color::Black) => '♟',
        }
    }

    let mut board_str = String::new();
    for rank in (0..8).rev() {
        board_str.push_str(&format!("{} ", rank + 1));
        for file in 0..8 {
            let square = Square::make_square(Rank::from_index(rank), File::from_index(file));
            let piece = board.piece_on(square);
            let color = board.color_on(square);
            let square_str = match piece {
                Some(piece) => piece_unicode(&piece, color.unwrap()).to_string(),
                None => ".".to_string(),
            };
            board_str.push_str(&square_str);
            board_str.push(' ');
        }
        board_str.push('\n');
    }
    board_str.push_str("  a b c d e f g h\n");
    board_str
}

async fn handle_game(
    read: &mut SplitStream<WebSocketStream<TcpStream>>,
    write: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    room: &Mutex<Room>,
    color: Color
) -> Result<()> {
    while let Some(msg) = read.next().await {
        let msg = msg?;
        if msg.is_text() {
            let received_text = msg.to_text()?;
            println!("Received message: {}", received_text);
            let current_position: String;
            let opponent_write: Option<Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>>;
            {
                let mut room = room.lock().await;
                if room.game.side_to_move() != color {
                    drop(room);
                    write.lock().await.send(Message::Text("not your turn".into())).await?;
                    continue;
                }
                room.game.make_move(ChessMove::from_str(&received_text).unwrap());
                current_position = pretty_board(&room.game.current_position());
                opponent_write = room.players[color.to_index() ^ 1].as_ref().map(|p| Arc::clone(&p.write_stream));
            }
            write.lock().await.send(Message::Text(current_position.clone().into())).await?;
            if let Some(opponent_write) = opponent_write {
                opponent_write.lock().await.send(Message::Text(received_text.into())).await?;
                opponent_write.lock().await.send(Message::Text(current_position.clone().into())).await?;
            }
        }
    }

    {
        let mut room = room.lock().await;
        room.players[color.to_index()] = None;
    }

    Ok(())
}

async fn handle_connection(stream: TcpStream, rooms: Arc<Vec<Mutex<Room>>>) -> Result<()> {
    let (write, mut read) = accept_async(stream).await?.split();
    let write = Arc::new(Mutex::new(write));
    println!("WebSocket connection established");

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if msg.is_text() {
            let received_text = msg.to_text()?;
            match serde_json::from_str::<ConnectionRequest>(&received_text) {
                Ok(request) => {
                    println!("Received connection request for room {}", request.room);
                    let room_id = request.room as usize;
                    let room = &rooms[room_id];
                    let color: Color;
                    let current_position: String;
                    {
                        let mut room = room.lock().await;
                        match (room.players[0].is_some(), room.players[1].is_some()) {
                            (true, true) => {
                                drop(room);
                                write.lock().await.send(Message::Text("room is full".into())).await?;
                                continue;
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
                            uuid: request.uuid.clone(),
                            write_stream: Arc::clone(&write)
                        });
                        println!("Player {} joined room {}", request.uuid, room_id);
                        println!("Room {} has {} players", room_id, room.players.len());
                        current_position = pretty_board(&room.game.current_position());
                    }
                    write.lock().await.send(Message::Text("connected".into())).await?;
                    write.lock().await.send(Message::Text(current_position.into())).await?;
                    handle_game(&mut read, Arc::clone(&write), room, color).await?;
                }
                Err(e) => {
                    println!("Error parsing connection request: {}", e);
                    write.lock().await.send(Message::Text("invalid request".into())).await?;
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
            stream.send(Message::Text("{\"room\": 0, \"uuid\": \"test\"}".into())).await.unwrap();
            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "connected");
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
            stream.send(Message::Text("{\"room\": 0, \"uuid\": \"test\"}".into())).await.unwrap();
            thread_barrier.wait().await;
            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "connected");
        });

        let thread_barrier = Arc::clone(&barrier);
        let handle2 = tokio::spawn(async move {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
            stream.send(Message::Text("{\"room\": 0, \"uuid\": \"test2\"}".into())).await.unwrap();
            thread_barrier.wait().await;
            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "connected");
        });

        let thread_barrier = Arc::clone(&barrier);
        let handle3 = tokio::spawn(async move {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
            thread_barrier.wait().await;
            stream.send(Message::Text("{\"room\": 0, \"uuid\": \"test3\"}".into())).await.unwrap();
            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "room is full");
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

        async fn make_move(uuid: &str) {
            let (mut stream, _) = connect_async("ws://localhost:8080").await.unwrap();
            stream.send(Message::Text(format!("{{\"room\": 0, \"uuid\": \"{uuid}\"}}").into())).await.unwrap();
            let response = stream.next().await.unwrap().unwrap();
            assert_eq!(response.to_text().unwrap(), "connected");
            // let response = stream.next().await.unwrap().unwrap();
            // let color = if response.to_text().unwrap() == "white" { Color::White } else { Color::Black };
            // if color == Color::White {
            //     stream.send(Message::Text("e2e4".into())).await.unwrap();
            //     let response = stream.next().await.unwrap().unwrap();
            //     assert_eq!(response.to_text().unwrap(), "e2e4");
            // }
            // stream.send(Message::Text("e2e4".into())).await.unwrap();
        }
        let handle = tokio::spawn(make_move("test"));
        let handle2 = tokio::spawn(make_move("test2"));
        assert!(handle.await.is_ok());
        assert!(handle2.await.is_ok());
    }
}


