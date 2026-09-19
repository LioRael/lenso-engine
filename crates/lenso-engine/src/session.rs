//! Embeddable incremental session. Failed refreshes preserve the last generation.
use crate::{Engine, Generation, Snapshot};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[derive(Debug)]
pub struct Session {
    engine: Engine,
    sources: Vec<PathBuf>,
    last_input: Option<String>,
    current: Option<Generation>,
}
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Updated { generation: Generation },
    Failed { message: String },
    Stopped,
}
impl Session {
    pub fn new(engine: Engine, sources: Vec<PathBuf>) -> anyhow::Result<Self> {
        if sources.is_empty() {
            anyhow::bail!("at least one source is required");
        }
        Ok(Self {
            engine,
            sources,
            last_input: None,
            current: None,
        })
    }
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
    pub fn invalidate(&mut self) {
        self.last_input = None;
    }
    pub fn current(&self) -> Option<&Generation> {
        self.current.as_ref()
    }
    pub fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let mut snapshot = Snapshot::default();
        for source in &self.sources {
            for (path, bytes) in Snapshot::read(source)?.files() {
                snapshot.insert(path.clone(), bytes.clone())?;
            }
        }
        Ok(snapshot)
    }
    pub fn refresh(&mut self, cancelled: &Arc<AtomicBool>) -> anyhow::Result<Option<Generation>> {
        let snapshot = self.snapshot()?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&snapshot)?));
        if self.last_input.as_ref() == Some(&digest) {
            return Ok(None);
        }
        let plan = self.engine.plan(snapshot)?;
        let generation = self.engine.execute(&plan, cancelled)?;
        self.last_input = Some(digest);
        self.current = Some(generation.clone());
        Ok(Some(generation))
    }
    /// Replace the complete admitted set when workflow/plugin lock changes.
    pub fn replace_engine(&mut self, engine: Engine) {
        self.engine = engine;
        self.last_input = None;
    }
    pub fn watch(
        &mut self,
        interval: Duration,
        cancelled: &Arc<AtomicBool>,
        mut emit: impl FnMut(Event),
    ) {
        let mut last_error = None;
        while !cancelled.load(Ordering::SeqCst) {
            match self.refresh(cancelled) {
                Ok(Some(generation)) => {
                    last_error = None;
                    emit(Event::Updated { generation });
                }
                Ok(None) => {}
                Err(error) => {
                    let message = format!("{error:#}");
                    if last_error.as_ref() != Some(&message) {
                        emit(Event::Failed {
                            message: message.clone(),
                        });
                        last_error = Some(message);
                    }
                }
            }
            let deadline = std::time::Instant::now() + interval.max(Duration::from_millis(25));
            while std::time::Instant::now() < deadline && !cancelled.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        emit(Event::Stopped);
    }
}
