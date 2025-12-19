use enginefs::EngineFS;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<EngineFS>,
}
