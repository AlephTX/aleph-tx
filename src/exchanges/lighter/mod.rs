pub mod error;
pub mod ffi;
pub mod trading;

pub(crate) fn valid_market_id(id: i64) -> bool {
    // v1.0.12 reserves 255 as the null market index.
    (0..(1 << 48)).contains(&id) && id != 255
}
