use async_trait::async_trait;
use std::time::Duration;
use xtra::address;
use xtra::Message;

#[async_trait]
pub trait SendInterval<A, M>
where
    M: Message,
    A: xtra::Handler<M>,
{
    /// Similar to xtra::Context::notify_interval, however it uses `send`
    /// instead of `do_send` under the hood.
    /// The crucial difference is that this function waits until previous
    /// handler returns before scheduling a new one, thus preventing them from
    /// piling up.
    /// As a bonus, this function is non-fallible.
    async fn send_interval<F>(self, duration: Duration, constructor: F)
    where
        F: Send + Sync + Fn() -> M,
        M: Message<Result = ()>,
        A: xtra::Handler<M>;
}

#[async_trait]
impl<A, M> SendInterval<A, M> for address::Address<A>
where
    M: Message,
    A: xtra::Handler<M>,
{
    async fn send_interval<F>(self, duration: Duration, constructor: F)
    where
        F: Send + Sync + Fn() -> M,
    {
        while self.send(constructor()).await.is_ok() {
            tokio::time::sleep(duration).await
        }
        let type_name = std::any::type_name::<M>();

        tracing::warn!(
            "Task for periodically sending message {type_name} stopped because actor shut down"
        );
    }
}