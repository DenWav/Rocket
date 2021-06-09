use std::fmt::Debug;

use futures::future::BoxFuture;
use rocket_http::uri::Origin;

use crate::{Request, Rocket, Orbit, request::{FromRequest, Outcome}};

use super::{Broker, channel::InnerChannel};


/// WebSocket equavalent of `Request`
pub struct WebSocket<'r> {
    request: Request<'r>,
    inner_channel: InnerChannel,
}

impl<'r> WebSocket<'r> {
    /// Create a new WebSocket object
    pub(crate) fn new(request: Request<'r>, inner_channel: InnerChannel) -> Self {
        Self { request, inner_channel }
    }

    /// Gets the internal request
    pub fn request(&self) -> &Request<'r> {
        &self.request
    }

    /// Gets the topic Uri for this request.
    pub fn topic(&self) -> &Origin<'r> {
        self.request.uri()
    }

    /// Gets a reference to the current Rocket instance
    pub fn rocket(&self) -> &Rocket<Orbit> {
        self.request.rocket()
    }

    /// Gets a handle to the broker
    pub fn broker(&self) -> Broker {
        self.request.rocket().broker()
    }

    /// Gets the internal WebSocket channel
    pub(crate) fn inner_channel(&self) -> InnerChannel {
        self.inner_channel.clone()
    }

    /// Calls set_uri on the inner reuest. This also has the effect of setting the topic uri
    pub fn set_uri(&mut self, origin: Origin<'r>) {
        self.request.set_uri(origin);
    }
}

impl WebSocket<'_> {
    pub(crate) fn clone(&self) -> Self {
        Self {
            request: self.request.clone(),
            inner_channel: self.inner_channel.clone(),
        }
    }
}

/// WebSocket equavalent of `FromRequest`. See `FromRequest` for more details on the inner
/// workings.
///
/// A defualt implementation of `FromWebSocket` is provided for any type that implements
/// `FromRequest`, which just calls the `FromRequest` implementation on the WebSocket type
#[crate::async_trait]
pub trait FromWebSocket<'r>: Sized {
    /// The associated error to be returned if derivation fails.
    type Error: Debug;

    /// Derives an instance of `Self` from the incoming webSocket metadata.
    ///
    /// If the derivation is successful, an outcome of `Success` is returned. If
    /// the derivation fails in an unrecoverable fashion, `Failure` is returned.
    /// `Forward` is returned to indicate that the request should be forwarded
    /// to other matching routes, if any.
    async fn from_websocket(request: &'r WebSocket<'_>) -> Outcome<Self, Self::Error>;
}

impl<'r, T: FromRequest<'r>> FromWebSocket<'r> for T {
    type Error = <T as FromRequest<'r>>::Error;

    #[must_use]
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn from_websocket<'life0, 'async_trait>(
        request: &'r WebSocket<'life0>,
    ) -> BoxFuture<'async_trait, Outcome<Self, Self::Error>>
        where 'r: 'async_trait,
              'life0: 'async_trait,
              Self: 'async_trait
    {
        <T as FromRequest<'r>>::from_request(request.request())
    }
}