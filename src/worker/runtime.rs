//! Shared Tokio runtime and lifecycle guards for background worker tasks.

use super::SharedState;
use crate::eams::EamsClient;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

pub(crate) fn spawn_task(future: impl std::future::Future<Output = ()> + Send + 'static) {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("Course-snatching-worker")
            .enable_all()
            .build()
            .expect("failed to create async runtime")
    });
    drop(runtime.spawn(future));
}

#[derive(Clone, Copy)]
pub(crate) enum Activity {
    Login,
    Refresh,
    Run,
}

pub(crate) struct ActivityGuard {
    state: Arc<SharedState>,
    activity: Activity,
    run_generation: Option<u64>,
}

impl ActivityGuard {
    pub(crate) fn new(state: Arc<SharedState>, activity: Activity) -> Self {
        Self {
            state,
            activity,
            run_generation: None,
        }
    }

    pub(crate) fn for_run(state: Arc<SharedState>, generation: u64) -> Self {
        Self {
            state,
            activity: Activity::Run,
            run_generation: Some(generation),
        }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        match self.activity {
            Activity::Login => self.state.logging_in.store(false, Ordering::Release),
            Activity::Refresh => self.state.refreshing.store(false, Ordering::Release),
            Activity::Run => {
                if let Some(generation) = self.run_generation {
                    self.state.release_run_if_owner(generation);
                }
            }
        }
        self.state.touch();
    }
}

pub(crate) struct BurstModeGuard(Arc<EamsClient>);

impl BurstModeGuard {
    pub(crate) fn new(client: Arc<EamsClient>) -> Self {
        Self(client)
    }
}

impl Drop for BurstModeGuard {
    fn drop(&mut self) {
        self.0.set_burst_mode(false);
    }
}
