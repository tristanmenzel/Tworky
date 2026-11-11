pub const COMMIT: &str = env!("TWORKY_COMMIT");
/// The first 7 characters of `COMMIT`, plus `-dirty` when built with uncommitted changes.
pub const SHORT: &str = env!("TWORKY_SHORT");
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("TWORKY_SHORT"), ")");

pub fn dirty() -> bool {
    env!("TWORKY_DIRTY") == "true"
}
