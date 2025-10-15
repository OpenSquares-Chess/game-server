use serde::{Deserialize};

#[derive(Deserialize)]
pub struct ConnectionRequest {
    pub room: u32
}
