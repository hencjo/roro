pub mod auth;
pub mod cli;
pub mod config;
pub mod error;
pub mod fs;
pub mod gc;
pub mod http;
pub mod store;
pub mod tls;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock precedes Unix epoch")
        .as_secs()
}
