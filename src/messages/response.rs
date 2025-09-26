use serde::{Serialize};

#[derive(Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Move { 
        #[serde(rename = "move")]
        move_: String, 
    },

    OutOfTurnMove,

    Fen { 
        fen: String, 
    },

    Color { 
        color: String, 
    },

    Connected,

    RoomFull,
}
