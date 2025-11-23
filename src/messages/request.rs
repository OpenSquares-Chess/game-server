use serde::{Deserialize};
use serde_with::{serde_as, DisplayFromStr};

#[serde_as]
#[derive(Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Request {
    JoinRoom {
        room: u32,
        #[serde_as(as = "DisplayFromStr")]
        key: u64
    },

    TimeSync
}
