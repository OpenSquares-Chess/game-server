use serde::{Deserialize};

#[derive(Deserialize)]
pub struct ConnectionRequest {
    pub room: u32,
    pub uuid: String
}
