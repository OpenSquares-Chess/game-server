use chess::{Game, Board, ChessMove, Color, Piece, Square, Rank, File};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{StreamExt, SinkExt};
use serde::Deserialize;
use std::sync::Arc;
use std::str::FromStr;
use anyhow::Result;

#[derive(Deserialize)]
struct ConnectionRequest {
    room: u32,
    uuid: String
}

struct Player {
    uuid: String,
    color: Color,
    write_stream: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>
}

struct Room {
    players: Vec<Player>,
    game: Game
}

#[tokio::main]
async fn main() -> Result<()> {
    let rooms: Vec<Mutex<Room>> = (0..10).map(|_| Mutex::new(Room { players: Vec::new(), game: Game::new() })).collect();
    let rooms = Arc::new(rooms);

    let addr = "0.0.0.0:8080".to_string();
    let listener = TcpListener::bind(&addr).await?;
    println!("WebSocket server started on ws://{}", addr);

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
    room: &Mutex<Room>,
    player_uuid: String, color: Color
) -> Result<()> {
    while let Some(msg) = read.next().await {
        let msg = msg?;
        if msg.is_text() {
            let received_text = msg.to_text()?;
            println!("Received message: {}", received_text);
            let players_to_notify = {
                let mut room = room.lock().await;
                room.game.make_move(ChessMove::from_str(&received_text).unwrap());
                room.players.iter()
                    .map(|p| (p.uuid.clone(), Arc::clone(&p.write_stream)))
                    .collect::<Vec<_>>()
            };
            let current_position = pretty_board(&room.lock().await.game.current_position());
            for (uuid, write_stream) in players_to_notify {
                if uuid != player_uuid {
                    write_stream.lock().await.send(Message::Text(received_text.clone().into())).await?;
                }
                write_stream.lock().await.send(Message::Text(current_position.clone().into())).await?;
            }
        }
    }

    {
        let mut room = room.lock().await;
        room.players.retain(|p| p.uuid != player_uuid);
    }

    Ok(())
}

async fn handle_connection(stream: tokio::net::TcpStream, rooms: Arc<Vec<Mutex<Room>>>) -> Result<()> {
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
                    let player_uuid: String;
                    let color: Color;
                    {
                        let mut room = room.lock().await;
                        if room.players.len() == 2 {
                            drop(room);
                            write.lock().await.send(Message::Text("Room is full".into())).await?;
                            continue;
                        }
                        player_uuid = request.uuid.clone();
                        color = if room.players.len() == 0 { Color::White } else { Color::Black };
                        room.players.push(Player { uuid: request.uuid, color: color, write_stream: Arc::clone(&write) });
                        println!("Player {} joined room {}", player_uuid, room_id);
                        println!("Room {} has {} players", room_id, room.players.len());
                    }
                    let current_position = pretty_board(&room.lock().await.game.current_position());
                    write.lock().await.send(Message::Text(current_position.into())).await?;
                    handle_game(&mut read, room, player_uuid.clone(), color).await?;
                }
                Err(e) => {
                    println!("Error parsing connection request: {}", e);
                    write.lock().await.send(Message::Text("Invalid request".into())).await?;
                }
            }
        }
    }

    Ok(())
}

