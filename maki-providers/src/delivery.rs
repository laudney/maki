use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[derive(Default)]
struct State {
    verdict: Option<bool>,
    rejected: Vec<Box<dyn FnOnce() + Send>>,
}

/// A tool's delivery verdict, independent of its execution lifetime.
#[derive(Clone, Default)]
pub struct ResultDelivery(Arc<Mutex<State>>);

impl ResultDelivery {
    pub fn accept(&self) {
        self.settle(true);
    }

    pub fn on_reject(&self, callback: impl FnOnce() + Send + 'static) {
        let mut state = self.lock();
        match state.verdict {
            Some(false) => {
                drop(state);
                callback();
            }
            Some(true) => {}
            None => state.rejected.push(Box::new(callback)),
        }
    }

    pub fn reject(&self) {
        self.settle(false);
    }

    fn settle(&self, verdict: bool) {
        let mut state = self.lock();
        if state.verdict.is_some() {
            return;
        }
        state.verdict = Some(verdict);
        let callbacks = std::mem::take(&mut state.rejected);
        drop(state);
        if !verdict {
            for callback in callbacks {
                callback();
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for ResultDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResultDelivery")
            .field("verdict", &self.lock().verdict)
            .finish_non_exhaustive()
    }
}

/// The message owns delivery. Dropping every copy without acceptance rejects
/// it, while the tool that produced it holds only the separate verdict handle.
pub struct DeliveryReceipt {
    delivery: ResultDelivery,
    is_live: Box<dyn Fn() -> bool + Send + Sync>,
}

impl DeliveryReceipt {
    pub fn new(
        delivery: ResultDelivery,
        is_live: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            delivery,
            is_live: Box::new(is_live),
        }
    }

    pub fn is_live(&self) -> bool {
        (self.is_live)()
    }

    pub fn accept(&self) {
        if self.is_live() {
            self.delivery.accept();
        } else {
            self.delivery.reject();
        }
    }
}

impl fmt::Debug for DeliveryReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.delivery.fmt(f)
    }
}

impl PartialEq for DeliveryReceipt {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.delivery.0, &other.delivery.0)
    }
}

impl Drop for DeliveryReceipt {
    fn drop(&mut self) {
        self.delivery.reject();
    }
}
