//! The tokio runtime that database I/O runs on.
//!
//! GPUI has its own executor, and tokio-postgres needs tokio. Database work
//! is spawned here and its result sent back over a oneshot channel, which a
//! GPUI task can await.

use std::future::Future;
use std::sync::OnceLock;

use anyhow::anyhow;
use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tabula-db")
            .enable_all()
            .build()
            .expect("couldn't start the tokio runtime")
    })
}

/// Run `fut` on the tokio runtime. The returned future can be awaited from
/// any executor (GPUI's included).
pub fn spawn<F, T>(fut: F) -> impl Future<Output = anyhow::Result<T>> + Send + 'static
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    async move { rx.await.map_err(|_| anyhow!("the database task was dropped"))? }
}

/// Run `fut` to completion on the runtime from synchronous code.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    runtime().block_on(fut)
}
