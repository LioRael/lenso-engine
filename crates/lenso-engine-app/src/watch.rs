use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::Context;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

const DEBOUNCE: Duration = Duration::from_millis(150);
const FALLBACK_INTERVAL: Duration = Duration::from_millis(750);
const PROJECT_FILES: [&str; 5] = [
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "bun.lock",
    "tsconfig.json",
];

/// Event-driven Plugin source watcher with a metadata-polling fallback.
#[derive(Debug)]
pub struct SourceWatcher {
    root: PathBuf,
    mode: WatchMode,
}

#[derive(Debug)]
enum WatchMode {
    Notifications {
        _watcher: RecommendedWatcher,
        receiver: mpsc::UnboundedReceiver<notify::Result<Event>>,
    },
    Polling {
        fingerprint: SourceFingerprint,
        rebuild_pending: bool,
    },
}

type SourceFingerprint = Vec<(PathBuf, u64, SystemTime, [u8; 32])>;

impl SourceWatcher {
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        let root = root.to_path_buf();
        let mode = match notification_watcher(&root) {
            Ok(mode) => mode,
            Err(error) => {
                eprintln!(
                    "Plugin file notifications are unavailable ({error:#}); using bounded polling fallback"
                );
                WatchMode::Polling {
                    fingerprint: source_fingerprint(&root)?,
                    rebuild_pending: false,
                }
            }
        };
        Ok(Self { root, mode })
    }

    pub async fn changed(&mut self) -> anyhow::Result<()> {
        loop {
            match &mut self.mode {
                WatchMode::Notifications { receiver, .. } => {
                    let Some(event) = receiver.recv().await else {
                        self.use_polling_fallback(true)?;
                        continue;
                    };
                    match event {
                        Ok(event) if event_is_relevant(&self.root, &event) => {
                            if let Some(failure) =
                                debounce_notifications(&self.root, receiver).await
                            {
                                eprintln!(
                                    "Plugin file notification {failure}; using bounded polling fallback"
                                );
                                // The event that opened this debounce window already
                                // requests a rebuild, so only future changes need polling.
                                self.use_polling_fallback(false)?;
                            }
                            return Ok(());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            eprintln!(
                                "Plugin file notification failed ({error}); using bounded polling fallback"
                            );
                            self.use_polling_fallback(true)?;
                        }
                    }
                }
                WatchMode::Polling {
                    fingerprint,
                    rebuild_pending,
                } => {
                    if std::mem::take(rebuild_pending) {
                        return Ok(());
                    }
                    tokio::time::sleep(FALLBACK_INTERVAL).await;
                    let next = source_fingerprint(&self.root)?;
                    if &next != fingerprint {
                        *fingerprint = next;
                        return Ok(());
                    }
                }
            }
        }
    }

    fn use_polling_fallback(&mut self, rebuild_pending: bool) -> anyhow::Result<()> {
        self.mode = WatchMode::Polling {
            fingerprint: source_fingerprint(&self.root)?,
            // A notification channel can fail after the filesystem changed but
            // before it delivered a usable event. Rebuild once from the new
            // polling baseline so that transition cannot swallow that edit.
            rebuild_pending,
        };
        Ok(())
    }
}

fn notification_watcher(root: &Path) -> anyhow::Result<WatchMode> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .context("create Plugin file notification watcher")?;
    watcher
        .watch(root, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch Plugin project root {}", root.display()))?;
    let source = root.join("src");
    if source.is_dir() {
        watcher
            .watch(&source, RecursiveMode::Recursive)
            .with_context(|| format!("watch Plugin source {}", source.display()))?;
    }
    Ok(WatchMode::Notifications {
        _watcher: watcher,
        receiver,
    })
}

async fn debounce_notifications(
    root: &Path,
    receiver: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
) -> Option<NotificationFailure> {
    loop {
        match tokio::time::timeout(DEBOUNCE, receiver.recv()).await {
            Ok(Some(Ok(event))) if event_is_relevant(root, &event) => {}
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => return Some(NotificationFailure::Error(error)),
            Ok(None) => return Some(NotificationFailure::Closed),
            Err(_) => return None,
        }
    }
}

#[derive(Debug)]
enum NotificationFailure {
    Error(notify::Error),
    Closed,
}

impl std::fmt::Display for NotificationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error(error) => write!(formatter, "failed ({error})"),
            Self::Closed => formatter.write_str("channel closed"),
        }
    }
}

fn event_is_relevant(root: &Path, event: &Event) -> bool {
    event.paths.iter().any(|path| is_watched_path(root, path))
}

fn is_watched_path(root: &Path, path: &Path) -> bool {
    path.starts_with(root.join("src"))
        || path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .is_some_and(|path| PROJECT_FILES.contains(&path))
}

fn source_fingerprint(root: &Path) -> anyhow::Result<SourceFingerprint> {
    let mut pending = vec![root.join("src")];
    let mut files = PROJECT_FILES
        .iter()
        .map(|path| root.join(path))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    while let Some(directory) = pending.pop() {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let metadata = fs::metadata(&path)
                .with_context(|| format!("inspect watched Plugin source {}", path.display()))?;
            let digest = Sha256::digest(
                fs::read(&path)
                    .with_context(|| format!("read watched Plugin source {}", path.display()))?,
            )
            .into();
            Ok((path, metadata.len(), metadata.modified()?, digest))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_filters_build_outputs_but_keeps_author_inputs() {
        let root = Path::new("/project");
        assert!(is_watched_path(root, Path::new("/project/src/plugin.rs")));
        assert!(is_watched_path(root, Path::new("/project/Cargo.toml")));
        assert!(is_watched_path(root, Path::new("/project/package.json")));
        assert!(!is_watched_path(
            root,
            Path::new("/project/target/debug/plugin")
        ));
        assert!(!is_watched_path(
            root,
            Path::new("/project/dist/plugin.lenso-plugin")
        ));
    }

    #[test]
    fn polling_fallback_detects_same_length_source_rewrites() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let source = root.path().join("src/plugin.rs");
        fs::write(&source, "first").unwrap();
        let before = source_fingerprint(root.path()).unwrap();

        fs::write(source, "other").unwrap();
        let after = source_fingerprint(root.path()).unwrap();

        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn notification_error_inside_debounce_switches_the_watcher_to_polling() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let (mut watcher, sender) = notification_source_watcher(root.path());
        sender
            .send(Ok(
                Event::new(notify::EventKind::Any).add_path(root.path().join("src/plugin.rs"))
            ))
            .unwrap();
        sender
            .send(Err(notify::Error::generic("injected notification failure")))
            .unwrap();

        watcher.changed().await.unwrap();

        assert!(matches!(
            watcher.mode,
            WatchMode::Polling {
                rebuild_pending: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn notification_channel_close_inside_debounce_switches_the_watcher_to_polling() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let (mut watcher, sender) = notification_source_watcher(root.path());
        sender
            .send(Ok(
                Event::new(notify::EventKind::Any).add_path(root.path().join("src/plugin.rs"))
            ))
            .unwrap();
        drop(sender);

        watcher.changed().await.unwrap();

        assert!(matches!(
            watcher.mode,
            WatchMode::Polling {
                rebuild_pending: false,
                ..
            }
        ));
    }

    fn notification_source_watcher(
        root: &Path,
    ) -> (SourceWatcher, mpsc::UnboundedSender<notify::Result<Event>>) {
        let native = notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap();
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            SourceWatcher {
                root: root.to_path_buf(),
                mode: WatchMode::Notifications {
                    _watcher: native,
                    receiver,
                },
            },
            sender,
        )
    }
}
