use crate::config::JoshConfig;

pub mod config;
pub mod josh;
pub mod sync;
pub mod utils;

#[derive(Clone)]
pub struct SyncContext {
    pub config: JoshConfig,
}
