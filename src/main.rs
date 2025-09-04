use chess::{Board, MoveGen};

fn main() {
    let board = Board::default();
    let moves = MoveGen::new_legal(&board);
    println!("{:?}", moves.map(|m| m.to_string()).collect::<Vec<String>>());
}
