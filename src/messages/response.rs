use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Response {
    InvalidToken {
        reason: String,
    },
    
    TokenValidated,

    InvalidKey,

    InvalidRequest {
        reason: String,
    },

    RoomNotActive,

    Move { 
        #[serde(rename = "move")]
        move_: String
    },

    InvalidMove,

    OutOfTurnMove,

    Fen { 
        fen: String, 
        timestamp: u64,
        white_time: u64,
        black_time: u64
    },

    Color { 
        color: String, 
    },

    Connected,

    GameOver{
        winner: String,
    },

    GameCanceled,

    TimeSync {
        timestamp: u64,
    },
}
