//! One shared `notify::RecommendedWatcher` per `gait_dir`, rooted recursively
//! at `gait_dir/.active/` (the only tree that can produce new session-log
//! events — persisted sessions under `gait_dir/sessions/` are frozen).
//! Subscribers register the exact file path they care about; the hub fans
//! matching events out to each one over its own channel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, RwLock};

use crate::WatchError;

#[derive(Debug, Clone)]
pub struct HubEvent {
    pub path: PathBuf,
    pub kind: notify::EventKind,
}

struct Subscriber {
    id: u64,
    path: PathBuf,
    sender: mpsc::UnboundedSender<HubEvent>,
}

pub struct SessionWatchHub {
    gait_dir: PathBuf,
    canonical_active_dir: PathBuf,
    _watcher: RecommendedWatcher,
    subscribers: Arc<RwLock<Vec<Subscriber>>>,
    next_id: AtomicU64,
}

pub struct SubscriptionHandle {
    id: u64,
    subscribers: Arc<RwLock<Vec<Subscriber>>>,
    receiver: mpsc::UnboundedReceiver<HubEvent>,
}

impl SessionWatchHub {
    /// Creates `gait_dir/.active/` if it doesn't exist yet (a fresh
    /// `gait_dir` with no sessions started has no such directory), then
    /// watches it recursively.
    pub fn new(gait_dir: PathBuf) -> Result<Self, WatchError> {
        let active_dir = gait_dir.join(".active");
        std::fs::create_dir_all(&active_dir)?;
        // `notify`'s macOS (FSEvents) backend reports canonicalized paths in
        // every event, regardless of the (possibly symlinked) path it was
        // asked to watch — e.g. tempfile's tmpdir()s are under `/var/...`,
        // a symlink to `/private/var/...`, and FSEvents events come back
        // with the resolved `/private/var/...` form. If we registered the
        // watch and stored subscriber paths using the raw, non-canonical
        // form, `sub.path == evt.path` would never match and no subscriber
        // would ever receive an event. Canonicalizing the watch root once
        // here, and rebasing every subscribed path onto it in `subscribe`,
        // keeps both sides consistent on every platform.
        let canonical_active_dir = active_dir.canonicalize()?;

        let subscribers = Arc::new(RwLock::new(Vec::<Subscriber>::new()));
        let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();

        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(&canonical_active_dir, RecursiveMode::Recursive)?;

        let subscribers_for_task = subscribers.clone();
        tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                let Ok(event) = res else { continue };
                for path in &event.paths {
                    let evt = HubEvent {
                        path: path.clone(),
                        kind: event.kind.clone(),
                    };
                    let subs = subscribers_for_task.read().await;
                    for sub in subs.iter() {
                        if sub.path == evt.path {
                            let _ = sub.sender.send(evt.clone());
                        }
                    }
                }
            }
        });

        Ok(Self {
            gait_dir,
            canonical_active_dir,
            _watcher: watcher,
            subscribers,
            next_id: AtomicU64::new(0),
        })
    }

    /// Rebase a path under `gait_dir/.active/` onto the hub's canonicalized
    /// watch root, so it matches the (canonicalized) paths `notify` reports
    /// — see the comment in `new` for why this is necessary. `path` need
    /// not exist yet (unlike `Path::canonicalize`, which requires it to):
    /// only the watch root itself, which `new` already guarantees exists,
    /// needs to be resolved.
    fn canonicalize_watched_path(&self, path: &Path) -> PathBuf {
        let active_dir = self.gait_dir.join(".active");
        match path.strip_prefix(&active_dir) {
            Ok(rest) => self.canonical_active_dir.join(rest),
            Err(_) => path.to_path_buf(),
        }
    }

    /// Subscribe to events on an exact file path (a session's live log path,
    /// from `gait_state::session::live::log_path`).
    pub async fn subscribe(&self, path: PathBuf) -> SubscriptionHandle {
        let path = self.canonicalize_watched_path(&path);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.write().await.push(Subscriber {
            id,
            path,
            sender: tx,
        });
        SubscriptionHandle {
            id,
            subscribers: self.subscribers.clone(),
            receiver: rx,
        }
    }

    #[cfg(test)]
    fn gait_dir_for_test(&self) -> &Path {
        &self.gait_dir
    }

    #[cfg(test)]
    async fn subscriber_count_for_test(&self) -> usize {
        self.subscribers.read().await.len()
    }
}

impl SubscriptionHandle {
    pub async fn next(&mut self) -> Option<HubEvent> {
        self.receiver.recv().await
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        let id = self.id;
        let subs = self.subscribers.clone();
        tokio::spawn(async move {
            subs.write().await.retain(|s| s.id != id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn subscriber_receives_event_on_watched_file_change() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(
            gait_dir
                .join(".active")
                .join("s1")
                .join("sessions")
                .join("s1"),
        )
        .unwrap();
        let log_path = gait_dir
            .join(".active")
            .join("s1")
            .join("sessions")
            .join("s1")
            .join("log.jsonl");
        std::fs::write(&log_path, "").unwrap();

        let hub = SessionWatchHub::new(gait_dir).unwrap();
        let mut sub = hub.subscribe(log_path.clone()).await;

        std::fs::write(&log_path, "line one\n").unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("timed out waiting for hub event")
            .expect("subscription closed");
        // Compare against the canonicalized path, not `log_path` itself:
        // `notify`'s macOS backend reports canonicalized paths (see the
        // comment on `SessionWatchHub::new`), so a symlinked tempdir (as
        // macOS's default temp dir is) makes a raw `log_path` comparison
        // here fail even though the hub correctly matched the same file.
        assert_eq!(event.path, log_path.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn subscriber_does_not_receive_events_for_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path().to_path_buf();
        let active = gait_dir.join(".active");
        std::fs::create_dir_all(active.join("s1").join("sessions").join("s1")).unwrap();
        std::fs::create_dir_all(active.join("s2").join("sessions").join("s2")).unwrap();
        let watched = active
            .join("s1")
            .join("sessions")
            .join("s1")
            .join("log.jsonl");
        let other = active
            .join("s2")
            .join("sessions")
            .join("s2")
            .join("log.jsonl");
        std::fs::write(&watched, "").unwrap();
        std::fs::write(&other, "").unwrap();

        let hub = SessionWatchHub::new(gait_dir).unwrap();
        let mut sub = hub.subscribe(watched.clone()).await;

        std::fs::write(&other, "unrelated write\n").unwrap();
        // No event should arrive for `other`; confirm by timing out.
        let result = tokio::time::timeout(Duration::from_millis(500), sub.next()).await;
        assert!(
            result.is_err(),
            "subscriber for s1 must not see s2's file event"
        );
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive_same_event() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(
            gait_dir
                .join(".active")
                .join("s1")
                .join("sessions")
                .join("s1"),
        )
        .unwrap();
        let log_path = gait_dir
            .join(".active")
            .join("s1")
            .join("sessions")
            .join("s1")
            .join("log.jsonl");
        std::fs::write(&log_path, "").unwrap();

        let hub = SessionWatchHub::new(gait_dir).unwrap();
        let mut sub1 = hub.subscribe(log_path.clone()).await;
        let mut sub2 = hub.subscribe(log_path.clone()).await;

        std::fs::write(&log_path, "line one\n").unwrap();

        let e1 = tokio::time::timeout(Duration::from_secs(2), sub1.next()).await;
        let e2 = tokio::time::timeout(Duration::from_secs(2), sub2.next()).await;
        assert!(
            e1.is_ok() && e1.unwrap().is_some(),
            "sub1 must receive the event"
        );
        assert!(
            e2.is_ok() && e2.unwrap().is_some(),
            "sub2 must receive the event"
        );
    }

    #[tokio::test]
    async fn dropping_subscription_removes_it_from_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(gait_dir.join(".active")).unwrap();
        let hub = SessionWatchHub::new(gait_dir).unwrap();

        let path = hub.gait_dir_for_test().join(".active").join("x.jsonl");
        let sub1 = hub.subscribe(path.clone()).await;
        let sub2 = hub.subscribe(path.clone()).await;
        assert_eq!(hub.subscriber_count_for_test().await, 2);

        drop(sub1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hub.subscriber_count_for_test().await, 1);

        drop(sub2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hub.subscriber_count_for_test().await, 0);
    }

    #[tokio::test]
    async fn new_creates_active_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path().to_path_buf();
        assert!(!gait_dir.join(".active").exists());
        let _hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
        assert!(gait_dir.join(".active").is_dir());
    }
}
