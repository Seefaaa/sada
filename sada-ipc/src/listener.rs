//! Accepting one channel per client.

use serde::de::DeserializeOwned;

use crate::{ChannelConfig, Endpoint, Error, Receiver, Result, Sender, channel, transport};

/// A bound endpoint, handing out one channel per client that arrives.
#[derive(Debug)]
pub struct Listener {
    /// The platform listener.
    inner: transport::Listener,
    /// Endpoint it is bound to, which its errors name.
    endpoint: Endpoint,
    /// Configuration every channel it hands out is built with.
    config: ChannelConfig,
}

impl Listener {
    /// Bind `endpoint`, with the default configuration.
    pub fn bind(endpoint: &Endpoint) -> Result<Self> { Self::bind_with(endpoint, ChannelConfig::default()) }

    /// Bind `endpoint`.
    ///
    /// Must be called from within a tokio runtime. On unix a socket file an earlier process left behind is removed to
    /// make room, and the one this listener binds is removed when it is dropped.
    pub fn bind_with(endpoint: &Endpoint, config: ChannelConfig) -> Result<Self> {
        let inner = transport::Listener::bind(endpoint).map_err(|source| Error::Bind(endpoint.clone(), source))?;

        Ok(Self {
            inner,
            endpoint: endpoint.clone(),
            config,
        })
    }

    /// The endpoint this listener is bound to.
    #[must_use]
    pub fn endpoint(&self) -> &Endpoint { &self.endpoint }

    /// Wait for the next client and hand back the two ends of the channel to it.
    ///
    /// `Tx` is what this end sends and `Rx` what it receives, which are the peer's two the other way around.
    pub async fn accept<Tx, Rx>(&mut self) -> Result<(Sender<Tx>, Receiver<Rx>)>
    where
        Rx: DeserializeOwned + Send + 'static,
    {
        let stream = self
            .inner
            .accept()
            .await
            .map_err(|source| Error::Accept(self.endpoint.clone(), source))?;

        Ok(channel::spawn(stream, self.config))
    }
}
