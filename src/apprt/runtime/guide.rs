use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::apprt::services::GuideServices;
use crate::apprt::transport::ScopedEventSender;
use crate::contracts::guide::{GuideEffect, GuideEvent};

pub struct GuideRuntime {
    services: GuideServices,
    live: Arc<AtomicU64>,
    cancelled: Arc<AtomicU64>,
}

impl GuideRuntime {
    pub fn new(services: GuideServices) -> Self {
        Self {
            services,
            live: Arc::default(),
            cancelled: Arc::default(),
        }
    }

    pub fn dispatch(&self, effect: GuideEffect, events: ScopedEventSender) {
        match effect {
            GuideEffect::Load {
                job,
                subject,
                base,
                head,
            } => {
                let services = self.services.clone();
                thread::spawn(move || {
                    let guide = services.load(&subject, &base, &head).map(Arc::new);
                    events.send(GuideEvent::Loaded { job, guide });
                });
            }
            GuideEffect::Generate(job) => {
                let services = self.services.clone();
                let id = job.request.job;
                if id.0 <= self.cancelled.load(Ordering::Acquire) {
                    return;
                }
                let live = Arc::clone(&self.live);
                let cancelled = Arc::clone(&self.cancelled);
                live.store(id.0, Ordering::Release);
                thread::spawn(move || {
                    let current = || {
                        live.load(Ordering::Acquire) == id.0
                            && id.0 > cancelled.load(Ordering::Acquire)
                    };
                    let mut progress = |bytes, steps| {
                        events.send(GuideEvent::Progress {
                            job: id,
                            bytes,
                            steps,
                        })
                    };
                    match services.generate(*job, &current, &mut progress) {
                        Ok(Some(guide)) => {
                            events.send(GuideEvent::Generated {
                                job: id,
                                guide: Arc::new(guide),
                            });
                        }
                        Ok(None) => tracing::info!(job = id.0, "guide: cancelled"),
                        Err(message) => {
                            events.send(GuideEvent::Failed { job: id, message });
                        }
                    }
                });
            }
            GuideEffect::Cancel { job } => {
                self.cancelled.fetch_max(job.0, Ordering::AcqRel);
                let _ = self
                    .live
                    .compare_exchange(job.0, 0, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Provider;
    use crate::apprt::transport::RuntimeEventSender;
    use crate::contracts::guide::{GuideJob, GuideRequest, GuideSubject};
    use crate::contracts::mutations::AiBackend;
    use crate::contracts::review::Secret;
    use crate::contracts::shared::{JobId, WorkspaceEpoch};
    use crate::core::review::ReviewTarget;
    use crate::platform::guide_store::GuideStore;

    #[test]
    fn queued_generation_cannot_restart_after_cancellation() {
        let cache = tempfile::TempDir::new().unwrap();
        let runtime = GuideRuntime::new(GuideServices::new(GuideStore::in_dir(cache.path())));
        let (sender, receiver) = std::sync::mpsc::channel();
        let events = RuntimeEventSender::from_channel(sender).scoped(WorkspaceEpoch(1));
        runtime.dispatch(GuideEffect::Cancel { job: JobId(3) }, events.clone());
        runtime.dispatch(GuideEffect::Cancel { job: JobId(1) }, events.clone());
        assert_eq!(runtime.cancelled.load(Ordering::Acquire), 3);
        for id in [1, 2, 3] {
            runtime.dispatch(
                GuideEffect::Generate(Box::new(GuideJob {
                    request: GuideRequest {
                        job: JobId(id),
                        subject: GuideSubject::PullRequest(ReviewTarget::github(
                            "acme", "widgets", 1,
                        )),
                        repo_path: cache.path().into(),
                        base: "base".into(),
                        head: "head".into(),
                        title: None,
                        description: None,
                        files: Vec::new(),
                    },
                    backend: AiBackend::Api {
                        provider: Provider::Anthropic,
                        api_key: Secret::new("test-not-a-credential"),
                    },
                })),
                events.clone(),
            );
        }
        assert_eq!(runtime.live.load(Ordering::Acquire), 0);
        assert!(receiver.try_recv().is_err());
    }
}
