use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::apprt::services::GuideServices;
use crate::apprt::transport::ScopedEventSender;
use crate::contracts::guide::{GuideEffect, GuideEvent};

pub struct GuideRuntime {
    services: GuideServices,
    live: Arc<AtomicU64>,
}

impl GuideRuntime {
    pub fn new(services: GuideServices) -> Self {
        Self {
            services,
            live: Arc::default(),
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
                let live = Arc::clone(&self.live);
                live.store(id.0, Ordering::Release);
                thread::spawn(move || {
                    let current = || live.load(Ordering::Acquire) == id.0;
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
                let _ = self
                    .live
                    .compare_exchange(job.0, 0, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }
}
