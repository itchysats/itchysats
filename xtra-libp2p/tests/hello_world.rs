use anyhow::Context as _;
use anyhow::Result;
use async_trait::async_trait;
use asynchronous_codec::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use xtra::Actor;
use xtra::Context;
use xtra_libp2p::NewInboundSubstream;
use xtra_productivity::xtra_productivity;

pub const PROTOCOL_NAME: &str = "/hello-world/1.0.0";

#[derive(Default)]
pub struct HelloWorld;

#[xtra_productivity]
impl HelloWorld {
    async fn handle(&mut self, msg: NewInboundSubstream, ctx: &mut Context<Self>) {
        tracing::info!("New hello world stream from {}", msg.peer);

        tokio_extras::spawn_fallible(
            &ctx.address().unwrap(),
            hello_world_listener(msg.stream),
            move |e| async move {
                tracing::warn!("Hello world protocol with peer {} failed: {}", msg.peer, e);
            },
        );
    }
}

#[async_trait]
impl Actor for HelloWorld {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

pub async fn hello_world_dialer(
    stream: xtra_libp2p::Substream,
    name: &'static str,
) -> Result<String> {
    let mut stream = asynchronous_codec::Framed::new(stream, asynchronous_codec::LengthCodec);

    stream.send(Bytes::from(name)).await?;
    let bytes = stream.next().await.context("Expected message")??;
    let message = String::from_utf8(bytes.to_vec())?;

    Ok(message)
}

pub async fn hello_world_listener(stream: xtra_libp2p::Substream) -> Result<()> {
    let mut stream =
        asynchronous_codec::Framed::new(stream, asynchronous_codec::LengthCodec).fuse();

    let bytes = stream.select_next_some().await?;
    let name = String::from_utf8(bytes.to_vec())?;

    stream.send(Bytes::from(format!("Hello {name}!"))).await?;

    Ok(())
}
