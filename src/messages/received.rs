use serde::{Deserialize};
use serde_with::{serde_as, DisplayFromStr};

#[serde_as]
#[derive(Deserialize)]
pub struct ConnectionRequest {
    pub room: u32,
    #[serde_as(as = "DisplayFromStr")]
    pub key: u64
}
