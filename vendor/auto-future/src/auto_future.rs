use ::std::fmt::Debug;
use ::std::fmt::Formatter;
use ::std::fmt::Result as FmtResult;
use ::std::future::Future;
use ::std::pin::Pin;
use ::std::task::Context;
use ::std::task::Poll;

pub struct AutoFuture<T> {
    inner: Pin<Box<dyn Future<Output = T>>>,
    is_done: bool,
}

impl<T> AutoFuture<T> {
    pub fn new<F>(raw_future: F) -> Self
    where
        F: Future<Output = T> + 'static,
    {
        Self {
            inner: Box::pin(raw_future),
            is_done: false,
        }
    }
}

impl<T> Future for AutoFuture<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.is_done {
            panic!("Polling future when this is already completed");
        }

        let inner_poll = self.inner.as_mut().poll(cx);
        match inner_poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(inner_result) => {
                self.is_done = true;
                Poll::Ready(inner_result)
            }
        }
    }
}

impl<T> Debug for AutoFuture<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(
            f,
            "AutoFuture {{ inner: Pin<Box<dyn Future<Output = T>>>, is_dome: {:?} }}",
            self.is_done
        )
    }
}
