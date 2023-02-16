//! Asynchronous scope.
//!
//! You can think of the scope as a lifetime 'env within a rust future, such that:
//! * within 'env you can spawn subtasks, which may return an error E.
//! * subtasks can spawn more subtasks.
//! * 'env has special semantics: at the end of 'env all spawned substasks are awaited for
//!    completion.
//! * if ANY of the subtasks returns an error, all the other subtasks are GRACEFULLY cancelled.
//!   It means that they are not just dropped, but rather they have a handle (aka Ctx) to be able
//!   to check at any time whether they were cancelled.
//!
//! ```
//!     let (send,recv) = channel();
//!     ...
//!     'env: {
//!         spawn<'env>(async {
//!             recv.await
//!             Err(e)
//!         });
//!
//!         for !is_cancelled() {
//!             // do some useful async unit of work
//!         }
//!         // do some graceful cleanup.
//!         Ok(())
//!     }
//! ```
//!
//! Since we cannot directly address lifetimes like that we simulate it via Scope and Ctx structs.
//! We cannot directly implement a function
//!   run : (Scope<'env> -> (impl 'env + Future)) -> (impl 'env + Future)
//! Because the compiler is not smart enough to deduce 'env for us.
//! Instead we first construct Scope<'env> explicitly, therefore fixing its lifetime,
//! and only then we pass a reference to it to another function.
//!
//! ```
//!     let (send,recv) = channel();
//!     ...
//!     {
//!         let s = Scope<'env>::new();
//!         s.run(ctx,|s||ctx| async {
//!             s.spawn(|ctx| async {
//!                 recv.await
//!                 Err(e)
//!             })
//!
//!             for !ctx.is_cancelled() {
//!                 // do some useful async unit of work
//!             }
//!             // do some graceful cleanup.
//!             Ok(())
//!         }).await
//!     }
//! ```
//!
//! We wrap these 2 steps into a macro "run!" to hide this hack and avoid incorrect use.
use crate::concurrency::asyncfn::{AsyncFn, BoxAsyncFn};
use crate::concurrency::ctx::{Ctx, CtxWithCancel, OrCanceled};
use crate::concurrency::signal;
use std::borrow::Borrow;
use std::future::Future;
use std::sync::{Arc, Weak};

#[cfg(test)]
mod tests;

// TODO(gprusak): Consider making the context implicit by moving it to a thread_local storage.

struct Output<E> {
    ctx: CtxWithCancel,
    send: crossbeam_channel::Sender<E>,
}

impl<E> Clone for Output<E> {
    fn clone(&self) -> Self {
        Self { ctx: self.ctx.clone(), send: self.send.clone() }
    }
}

impl<E> Output<E> {
    pub fn new(ctx: CtxWithCancel) -> (Self, crossbeam_channel::Receiver<E>) {
        let (send, recv) = crossbeam_channel::bounded(1);
        (Self { ctx, send }, recv)
    }

    pub fn send(&self, err: E) {
        if let Ok(_) = self.send.try_send(err) {
            self.ctx.cancel();
        }
    }
}

/// Internal representation of a scope.
struct Inner<E: 'static> {
    /// Signal sent once the scope is terminated (i.e. when Inner is dropped).
    ///
    /// Since all tasks keep a reference to the scope they belong to, all the tasks
    /// of the scope are complete when terminated is sent.
    terminated: signal::Once,
    /// Context of this scope.
    ///
    /// It is a child context of the parent scope.
    /// All tasks spawned in this scope are provided with this context.
    ctx: CtxWithCancel,
    /// The channel over which the first error reported by any completed task is sent.
    ///
    /// `output` channel has capacity 1, so that any subsequent attempts to send an error
    /// will fail. The channel is created in `internal::run` and the value is received from
    /// the channel only after the top-level scope has terminated.
    /// Thanks to the fact that we have a single channel per top-level scope:
    /// * if you await Service::terminate() from within some task, it will complete only
    ///   after the error is actually registered, so the awaiting task won't be able to cause a
    ///   race condition.
    output: Output<E>,
}

impl<E: 'static> Drop for Inner<E> {
    fn drop(&mut self) {
        self.terminated.send();
    }
}

impl<E: 'static> Inner<E> {
    /// Creates a new scope with the given context.
    ///
    /// After the scope terminates, the result will be sent to output.
    pub fn new(ctx: CtxWithCancel, output: Output<E>) -> Arc<Inner<E>> {
        Arc::new(Inner { terminated: signal::Once::new(), ctx, output })
    }
}

impl<E: 'static + Send> Inner<E> {
    /// Spawns a task in the scope, which owns a reference of to the scope, so that scope doesn't
    /// terminate before all tasks are completed.
    ///
    /// The reference to the scope can be an arbitrary
    /// type, so that a custom drop() behavior can be added. For example, see `StrongService` scope reference,
    /// which cancels the scope when dropped.
    pub fn spawn<M: 'static + Send + Sync + Borrow<Self>>(
        m: Arc<M>,
        f: BoxAsyncFn<'static, Ctx, Result<(), E>>,
    ) {
        let f = f((*m.as_ref().borrow().ctx).clone());
        tokio::spawn(async move {
            if let Err(err) = f.await {
                m.as_ref().borrow().output.send(err);
            }
        });
    }

    /// Spawns a new service in the scope.
    ///
    /// A service is a scope, which gets canceled when
    /// its handler (`Service`) is dropped. Service belongs to a scope, in a sense that
    /// a dedicated task is spawned on the scope which awaits for service to terminate and
    /// returns the service's result.
    pub fn new_service(self: Arc<Self>) -> Service<E> {
        let subscope = Inner::new(self.ctx.with_cancel(), self.output.clone());
        let terminated = subscope.terminated.clone();
        let service = Service(Arc::downgrade(&subscope));
        // Spawn a task on m which will await termination of the Service.
        // Note that this task doesn't keep a StrongService reference, so
        // it will not prevent cancellation of the parent scope.
        Inner::spawn(self, (|_| async move { Ok(terminated.recv().await) }).wrap());
        // Spawn a guard task in the Service which will prevent termination of the Service
        // until the context is not canceled. See `Service` for a list
        // of events canceling the Service.
        Inner::spawn(subscope, (|ctx: Ctx| async move { Ok(ctx.canceled().await) }).wrap());
        service
    }
}

/// Error returned when the `Service` has been already terminated
/// and therefore spawning a task/service in it is not possible.
#[derive(thiserror::Error, Debug)]
#[error("ErrTerminated")]
pub struct ErrTerminated;

/// A service is a subscope which doesn't keep the scope
/// alive, i.e. if all tasks spawned via `Scope::spawn` complete, the scope will
/// be cancelled (even though tasks in a service may be still running).
///
/// Note however that the scope won't be terminated until the tasks of the service complete.
/// Service is cancelled when the handle is dropped, so make sure to store it somewhere.
/// Service is cancelled when ANY of the tasks/services in the service returns an error.
/// Service is cancelled when the parent scope/service is cancelled.
/// Service is NOT cancelled just when all tasks within the service complete - in particular
/// a newly started service has no tasks.
/// Service is terminated when it is cancelled AND all tasks within the service complete.
pub struct Service<E: 'static>(Weak<Inner<E>>);

impl<E: 'static> Drop for Service<E> {
    fn drop(&mut self) {
        self.0.upgrade().map(|inner| inner.ctx.cancel());
    }
}

impl<E: 'static + Send + Sync> Service<E> {
    /// Checks if the referred scope has been terminated.
    pub fn is_terminated(&self) -> bool {
        self.0.upgrade().is_none()
    }

    /// Waits until the scope is terminated.
    ///
    /// Returns `ErrCanceled` iff `ctx` was canceled before that.
    pub fn terminated<'a>(
        &'a self,
        ctx: &'a Ctx,
    ) -> impl Future<Output = OrCanceled<()>> + Send + Sync + 'a {
        let terminated = self.0.upgrade().map(|inner| inner.terminated.clone());
        async move {
            if let Some(t) = terminated {
                ctx.wait(t.recv()).await
            } else {
                Ok(())
            }
        }
    }

    /// Cancels the scope's context and waits until the scope is terminated.
    ///
    /// Note that ErrCanceled is returned if the `ctx` passed as argument is canceled before that,
    /// not when scope's context is cancelled.
    pub fn terminate<'a>(
        &'a self,
        ctx: &'a Ctx,
    ) -> impl Future<Output = OrCanceled<()>> + Send + Sync + 'a {
        let terminated = self.0.upgrade().map(|inner| {
            inner.ctx.cancel();
            inner.terminated.clone()
        });
        async move {
            if let Some(t) = terminated {
                ctx.wait(t.recv()).await
            } else {
                Ok(())
            }
        }
    }

    /// Spawns a task in this scope.
    ///
    /// Returns ErrTerminated if the scope has already terminated.
    // TODO(gprusak): consider returning a handle to the task, which can be then explicitly
    // awaited.
    pub fn spawn(&self, f: impl AsyncFn<'static, Ctx, Result<(), E>>) -> Result<(), ErrTerminated> {
        self.0.upgrade().map(|m| Inner::spawn(m, f.wrap())).ok_or(ErrTerminated)
    }

    /// Spawns a service in this scope.
    ///
    /// Returns ErrTerminated if the scope has already terminated.
    pub fn new_service(&self) -> Result<Service<E>, ErrTerminated> {
        self.0.upgrade().map(|m| Inner::new_service(m)).ok_or(ErrTerminated)
    }
}

/// Wrapper of a scope reference which cancels the scope when dropped.
///
/// Used by Scope to cancel the scope as soon as all tasks spawned via
/// `Scope::spawn` complete.
struct StrongService<E: 'static>(Arc<Inner<E>>);

impl<E: 'static> Borrow<Inner<E>> for StrongService<E> {
    fn borrow(&self) -> &Inner<E> {
        &*self.0
    }
}

impl<E: 'static> Drop for StrongService<E> {
    fn drop(&mut self) {
        self.0.ctx.cancel()
    }
}

/// Scope represents a concurrent computation bounded by lifetime 'env.
///
/// It should be created only via `run!` macro.
/// Scope is cancelled when the provided context is cancelled.
/// Scope is cancelled when any of the tasks in the scope returns an error.
/// Scope is cancelled when all the tasks in the scope complete.
/// Scope is terminated when it is cancelled AND all tasks in the scope complete.
pub struct Scope<'env, E: 'static>(
    /// Scope is equivalent to a strong service, but bounds
    Weak<StrongService<E>>,
    /// Makes Scope<'env,E> invariant in 'env.
    std::marker::PhantomData<fn(&'env ()) -> &'env ()>,
);

impl<'env, E: 'static + Send + Sync> Scope<'env, E> {
    pub fn spawn(&self, f: impl AsyncFn<'env, Ctx, Result<(), E>>) {
        let f = f.wrap();
        let f =
            unsafe { std::mem::transmute::<BoxAsyncFn<'env, _, _>, BoxAsyncFn<'static, _, _>>(f) };
        Inner::spawn(self.0.upgrade().unwrap(), f);
    }

    /// Spawns a service.
    ///
    /// Returns a handle to the service, which allows spawning new tasks within the service.
    pub fn new_service(&self) -> Service<E> {
        Inner::new_service(self.0.upgrade().unwrap().0.clone())
    }
}

/// must_complete wraps a future, so that it panic if it is dropped before completion.
///
/// Possibility of future abort at every await makes the control flow unnecessarily complicated.
/// In fact, only few basic futures (like io primitives) actually need to be abortable, so
/// that they can be put together into a tokio::select block. All the higher level logic
/// would greatly benefit (in terms of readability and bug-resistance) from being non-abortable.
/// Rust doesn't support linear types as of now, so best we can do is a runtime check.
pub fn must_complete<Fut: Future>(fut: Fut) -> impl Future<Output = Fut::Output> {
    let guard = MustCompleteGuard;
    async move {
        let res = fut.await;
        let _ = std::mem::ManuallyDrop::new(guard);
        res
    }
}

struct MustCompleteGuard;

impl Drop for MustCompleteGuard {
    fn drop(&mut self) {
        panic!("dropped a non-abortable future before completion");
    }
}

/// Should be used only via run! macro.
#[doc(hidden)]
pub mod internal {
    use super::*;

    pub fn new_scope<'env, E: 'static>() -> Scope<'env, E> {
        Scope(Weak::new(), std::marker::PhantomData)
    }

    pub async fn run<'env, E, F, G>(
        scope: &'env mut Scope<'env, E>,
        ctx: &Ctx,
        f: F,
    ) -> Result<(), E>
    where
        E: 'static + Send + Sync,
        F: 'env + FnOnce(&'env Scope<'env, E>) -> G,
        G: AsyncFn<'env, Ctx, Result<(), E>>,
    {
        must_complete(async move {
            let (output, recv) = Output::new(ctx.with_cancel());
            let service = Arc::new(StrongService(Inner::new(output.ctx.clone(), output)));
            let terminated = service.0.terminated.clone();
            scope.0 = Arc::downgrade(&service);
            scope.spawn(f(scope));
            // each task spawned on `scope` keeps its own reference to `service`.
            // As soon as all references to `service` are dropped, scope will be cancelled.
            drop(service);
            terminated.recv().await;
            match recv.try_recv() {
                Ok(err) => Err(err),
                Err(_) => Ok(()),
            }
        })
        .await
    }
}

/// A future running a task within a scope (see `Scope`).
///
/// `await` is called within the macro instantiation, so `run!` can be called only in an async context.
/// Dropping this future while incomplete will panic (immediate-await doesn't prevent that: it can happen
/// if you drop the outer future).
#[macro_export]
macro_rules! run {
    ($ctx:expr,$f:expr) => {{
        $crate::concurrency::scope::internal::run(
            // We pass a created scope via argument (rather than construct it within `run()`
            // So that rust compiler fixes the lifespan of the Scope, rather than trying to
            // reason about it - which is not smart enough to do.
            &mut $crate::concurrency::scope::internal::new_scope(),
            $ctx,
            $f,
        )
        .await
    }};
}

pub use run;
