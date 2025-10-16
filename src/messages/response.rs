use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Response {
    InvalidToken {
        reason: String,
    },
    
    TokenValidated,

    InvalidRequest,

    Move { 
        #[serde(rename = "move")]
        move_: String,
    },

    InvalidMove,

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
