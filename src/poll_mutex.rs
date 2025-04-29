use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{Mutex, MutexGuard, OwnedMutexGuard};

pub(crate) struct PollMutex<T> {
    mutex: Arc<Mutex<T>>,
    fut: Option<Pin<Box<dyn Future<Output=OwnedMutexGuard<T>> + Send>>>
}

pub(crate) enum MaybeOwnedGuard<'a, T> {
    Borrowed(MutexGuard<'a, T>),
    Owned(OwnedMutexGuard<T>)
}

impl<T> Deref for MaybeOwnedGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            MaybeOwnedGuard::Borrowed(borrow) => borrow,
            MaybeOwnedGuard::Owned(own) => own,
        }
    }
}

impl<T> DerefMut for MaybeOwnedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            MaybeOwnedGuard::Borrowed(borrow) => borrow,
            MaybeOwnedGuard::Owned(own) => own,
        }
    }
}

impl<T: Send + 'static> PollMutex<T> {
    pub fn new(mutex: Arc<Mutex<T>>) -> Self {
        Self {
            mutex,
            fut: None,
        }
    }
    
    pub fn poll_lock(&mut self, cx: &mut Context<'_>) -> Poll<OwnedMutexGuard<T>> {
        let future = match self.fut {
            Some(ref mut fut) => fut,
            None => {
                if let Ok(lock) = Arc::clone(&self.mutex).try_lock_owned() {
                    return Poll::Ready(lock);
                }

                self.fut.insert(Box::pin(Arc::clone(&self.mutex).lock_owned()))
            }
        };
        
        match future.as_mut().poll(cx) {
            Poll::Ready(lock) => {
                self.fut = None;
                Poll::Ready(lock)
            }
            Poll::Pending => Poll::Pending
        }
    }
    
    pub fn take_token(&mut self) -> Self {
        Self {
            mutex: Arc::clone(&self.mutex),
            fut: self.fut.take(),
        }
    }
    
    pub async fn lock(&mut self) -> MaybeOwnedGuard<T> {
        match self.fut.take() {
            Some(fut) => MaybeOwnedGuard::Owned(fut.await),
            None => MaybeOwnedGuard::Borrowed(self.mutex.lock().await),
        }
    }
}

impl<T> Clone for PollMutex<T> {
    fn clone(&self) -> Self {
        Self {
            mutex: Arc::clone(&self.mutex),
            fut: None
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.fut = None;
        
        if Arc::ptr_eq(&self.mutex, &source.mutex) {
            return;
        }
        self.mutex = Arc::clone(&source.mutex);
    }
}
