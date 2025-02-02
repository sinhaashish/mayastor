//!
//! This allows us to send futures from within mayastor to the tokio
//! runtime to do whatever it needs to do. The tokio threads are
//! unaffinitized such that they do not run on any of our reactors.

use crate::core::Reactor;
use futures::{channel::oneshot, Future};
use once_cell::sync::Lazy;
use tokio::task::JoinHandle;

use super::Mthread;

/// spawn a future on the tokio runtime.
pub fn spawn(f: impl Future<Output = ()> + Send + 'static) {
    RUNTIME.spawn(f);
}

/// Spawn a future on the tokio runtime and await its completion.
pub async fn spawn_await(f: impl Future<Output = ()> + Send + 'static) {
    let (s, r) = oneshot::channel();

    RUNTIME.spawn(async move {
        f.await;

        if let Ok(r) = Reactor::spawn_at_primary(async move {
            s.send(()).ok();
        }) {
            r.await.ok();
        }
    });
    r.await.ok();
}

/// block on the given future until it completes
pub fn block_on(f: impl Future<Output = ()> + Send + 'static) {
    RUNTIME.block_on(f);
}

/// spawn a future that might block on a separate worker thread the
/// number of threads available is determined by max_blocking_threads
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    RUNTIME.spawn_blocking(f)
}

pub struct Runtime {
    rt: tokio::runtime::Runtime,
}

static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .max_blocking_threads(6)
        .on_thread_start(Mthread::unaffinitize)
        .build()
        .unwrap();

    Runtime {
        rt,
    }
});

impl Runtime {
    pub fn new(rt: tokio::runtime::Runtime) -> Self {
        Self {
            rt,
        }
    }
    fn block_on(&self, f: impl Future<Output = ()> + Send + 'static) {
        self.rt.block_on(f);
    }

    fn spawn(&self, f: impl Future<Output = ()> + Send + 'static) {
        let handle = self.rt.handle().clone();
        handle.spawn(f);
    }

    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let handle = self.rt.handle().clone();
        handle.spawn_blocking(|| {
            Mthread::unaffinitize();
            f()
        })
    }
}
