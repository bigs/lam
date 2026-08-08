//! Thread-affine V8 activation for a parked kernel.

use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread::{self, ThreadId};

use deno_core::v8::{self, UnsafeRawIsolatePtr};

use super::{EvalError, EvalValue, ExecutionArm, KernelInner};

/// A kernel whose V8 isolate is entered only while Lam is polling or
/// inspecting it.
///
/// `deno_core` enters an `OwnedIsolate` when it creates the runtime. Parking
/// removes that long-lived entry so other independent isolates can take turns
/// on the same local executor. The owning `JsRuntime` still makes this type
/// `!Send`; the explicit thread check documents and defends that invariant.
pub(super) struct Kernel {
    inner: ManuallyDrop<KernelInner>,
    isolate: UnsafeRawIsolatePtr,
    owner: ThreadId,
}

impl Kernel {
    pub(super) fn park(mut inner: KernelInner) -> Self {
        // SAFETY: `inner` owns this heap-allocated isolate for the lifetime of
        // the returned `Kernel`; moving the runtime handle does not move V8.
        let isolate = unsafe { inner.runtime.v8_isolate().as_raw_isolate_ptr() };

        // SAFETY: `JsRuntime` construction entered this isolate on the current
        // thread. We immediately park it and balance every later entry.
        unsafe { inner.runtime.v8_isolate().exit() };

        Self {
            inner: ManuallyDrop::new(inner),
            isolate,
            owner: thread::current().id(),
        }
    }

    pub(super) fn isolate_handle(&mut self) -> v8::IsolateHandle {
        self.assert_owner();
        let _entered = EnteredIsolate::new(self.isolate);
        self.inner.isolate_handle()
    }

    pub(super) async fn evaluate(
        &mut self,
        source: &str,
        cell_id: u64,
        execution_arm: Arc<ExecutionArm>,
    ) -> Result<EvalValue, EvalError> {
        let isolate = self.isolate;
        let owner = self.owner;
        let future = self.inner.evaluate(source, cell_id);
        ActivatedFuture::new(future, isolate, owner, execution_arm).await
    }

    fn assert_owner(&self) {
        assert_eq!(
            thread::current().id(),
            self.owner,
            "Lam isolate used from a thread other than its owner"
        );
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        self.assert_owner();

        // `KernelInner` drops the inspector before `JsRuntime`. Entering here
        // lets their ordinary destructors run; dropping the runtime then exits
        // and disposes its `OwnedIsolate`, so no separate exit follows.
        // SAFETY: this is the same live isolate owned by `inner`, on its owner
        // thread, and `inner` is dropped exactly once here.
        let isolate = unsafe { v8::Isolate::from_raw_isolate_ptr(self.isolate) };
        unsafe { isolate.enter() };
        unsafe { ManuallyDrop::drop(&mut self.inner) };
    }
}

/// Polls and drops a kernel future with its isolate entered.
///
/// Drop-time activation matters when an actor aborts an in-flight eval: CDP and
/// op futures may release V8-backed state even though they never return Ready.
struct ActivatedFuture<F> {
    future: ManuallyDrop<F>,
    isolate: UnsafeRawIsolatePtr,
    owner: ThreadId,
    execution_arm: Arc<ExecutionArm>,
}

impl<F> ActivatedFuture<F> {
    fn new(
        future: F,
        isolate: UnsafeRawIsolatePtr,
        owner: ThreadId,
        execution_arm: Arc<ExecutionArm>,
    ) -> Self {
        Self {
            future: ManuallyDrop::new(future),
            isolate,
            owner,
            execution_arm,
        }
    }

    fn assert_owner(&self) {
        assert_eq!(
            thread::current().id(),
            self.owner,
            "Lam isolate future migrated away from its owner thread"
        );
    }
}

impl<F: Future> Future for ActivatedFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: pinning `ActivatedFuture` pins the manually stored future;
        // it is never moved before being dropped in this type's `Drop` impl.
        let this = unsafe { self.get_unchecked_mut() };
        this.assert_owner();
        // Arm immediately before the entered poll so only continuous work in
        // this poll consumes the execution budget. The guard disarms when the
        // poll returns Ready or Pending, including panic paths.
        let _armed = this.execution_arm.arm();
        let _entered = EnteredIsolate::new(this.isolate);
        unsafe { Pin::new_unchecked(&mut *this.future) }.poll(cx)
    }
}

impl<F> Drop for ActivatedFuture<F> {
    fn drop(&mut self) {
        self.assert_owner();
        // Drop-time activation is not continuous eval work; leave the
        // execution watchdog disarmed so destructor cleanup cannot trip it.
        let _entered = EnteredIsolate::new(self.isolate);

        // SAFETY: `future` has not moved and is dropped exactly once here.
        unsafe { ManuallyDrop::drop(&mut self.future) };
    }
}

struct EnteredIsolate {
    isolate: v8::Isolate,
}

impl EnteredIsolate {
    fn new(isolate: UnsafeRawIsolatePtr) -> Self {
        // SAFETY: callers hold the owning kernel alive and have verified its
        // thread affinity. Each constructed guard balances this entry in Drop.
        let isolate = unsafe { v8::Isolate::from_raw_isolate_ptr(isolate) };
        unsafe { isolate.enter() };
        Self { isolate }
    }
}

impl Drop for EnteredIsolate {
    fn drop(&mut self) {
        // SAFETY: this guard owns the matching entry made in `new`.
        unsafe { self.isolate.exit() };
    }
}
