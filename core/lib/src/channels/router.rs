//! Internal Routing structs

use std::collections::HashMap;
use std::str::Utf8Error;
use std::{io::Cursor, sync::Arc};

use bytes::Bytes;
use futures::{Future, FutureExt};
use rocket_http::ext::IntoOwned;
use rocket_http::{Header, Status, hyper::upgrade::Upgraded, uri::Origin};
use rocket_http::hyper::{self, header::{CONNECTION, UPGRADE}, upgrade::OnUpgrade};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use websocket_codec::{ClientRequest, Opcode};

use crate::channels::WebSocketMessage;
use crate::channels::WebSocketStatus;
use crate::route::WebSocketData;
use crate::route::WebSocketEvent;
use crate::route::WsOutcome;
use crate::{Data, Request, Response, Rocket, Route, phase::Orbit};
use crate::router::{Collide, Collisions};
use yansi::Paint;

use super::WebSocket;
use super::rocket_multiplex::MAX_TOPIC_LENGTH;
use super::rocket_multiplex::MULTIPLEX_CONTROL_CHAR;
use super::rocket_multiplex::MULTIPLEX_CONTROL_STR;
use super::{WebSocketChannel, channel::InnerChannel};

async fn handle<Fut, T, F>(name: Option<&str>, run: F) -> Option<T>
    where F: FnOnce() -> Fut, Fut: Future<Output = T>,
{
    use std::panic::AssertUnwindSafe;

    macro_rules! panic_info {
        ($name:expr, $e:expr) => {{
            match $name {
                Some(name) => error_!("Handler {} panicked.", Paint::white(name)),
                None => error_!("A handler panicked.")
            };

            info_!("This is an application bug.");
            info_!("A panic in Rust must be treated as an exceptional event.");
            info_!("Panicking is not a suitable error handling mechanism.");
            info_!("Unwinding, the result of a panic, is an expensive operation.");
            info_!("Panics will severely degrade application performance.");
            info_!("Instead of panicking, return `Option` and/or `Result`.");
            info_!("Values of either type can be returned directly from handlers.");
            warn_!("A panic is treated as an internal server error.");
            $e
        }}
    }

    let run = AssertUnwindSafe(run);
    let fut = std::panic::catch_unwind(run)
        .map_err(|e| panic_info!(name, e))
        .ok()?;

    AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .map_err(|e| panic_info!(name, e))
        .ok()
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Protocol {
    Naked,
    Multiplexed,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
enum Event {
    Join,
    Message,
    Leave,
}

#[derive(Debug)]
pub struct WebSocketRouter {
    routes: HashMap<Event, Vec<Route>>,
}

impl WebSocketRouter {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    pub fn routes(&self) -> impl Iterator<Item = &Route> + Clone {
        self.routes.iter().flat_map(|(_, r)| r.iter())
    }

    pub fn add_route(&mut self, route: Route) {
        //if route.webSocket_handler.is_some() {
            //self.routes.push(route);
        //}
        match route.websocket_handler {
            WebSocketEvent::None => (),
            WebSocketEvent::Join(_) =>
                self.routes.entry(Event::Join).or_default().push(route),
            WebSocketEvent::Message(_) =>
                self.routes.entry(Event::Message).or_default().push(route),
            WebSocketEvent::Leave(_) =>
                self.routes.entry(Event::Leave).or_default().push(route),
        }
    }

    fn collisions<'a, I, T>(&self, items: I) -> impl Iterator<Item = (T, T)> + 'a
        where I: Iterator<Item = &'a T> + Clone + 'a, T: Collide + Clone + 'a,
    {
        items.clone().enumerate()
            .flat_map(move |(i, a)| {
                items.clone()
                    .skip(i + 1)
                    .filter(move |b| a.collides_with(b))
                    .map(move |b| (a.clone(), b.clone()))
            })
    }

    pub fn finalize(&self) -> Result<(), Collisions> {
        let routes: Vec<_> = self.collisions(self.routes()).collect();

        if !routes.is_empty() {
            return Err(Collisions { routes, catchers: vec![] })
        }

        Ok(())
    }

    async fn handle_message<'r, 'a: 'r, 'b: 'r>(
        &'a self,
        event: Event,
        req: Arc<WebSocket<'r>>,
        mut message: WebSocketData<'b>,
    ) -> Result<(), WebSocketStatus<'r>> {
        for route in self.routes.get(&event)
            .into_iter()
            .flat_map(|routes| routes.iter()) {
            if route.matches(req.request()) {
                req.request().set_route(route);

                let name = route.name.as_deref();
                // Note: unwrap_ref may panic. This shouldn't ever happen, becuase every handler
                // that would panic should have been filtered out by `add_route`
                let handler = route.websocket_handler.unwrap_ref();
                let res = handle(name, || handler.handle(req.clone(), message)).await;
                // Successfully ran
                match res {
                    Some(WsOutcome::Forward(d)) => message = d,
                    Some(WsOutcome::Failure(s)) => return Err(s),
                    Some(WsOutcome::Success(())) => return Ok(()),
                    None => return Err(super::INTERNAL_SERVER_ERROR),
                }
            }
        }
        // Join events that were never failed are attempted against the message
        // handlers as Join events. This will not run the data guards or the function, but it will
        // check all of the request and parameter guards
        if event == Event::Join {
            for route in self.routes.get(&Event::Message)
                .into_iter()
                .flat_map(|routes| routes.iter()) {
                if route.matches(req.request()) {
                    req.request().set_route(route);

                    let name = route.name.as_deref();
                    // Note: unwrap_ref may panic. This shouldn't ever happen, becuase every handler
                    // that would panic should have been filtered out by `add_route`
                    let handler = route.websocket_handler.unwrap_ref();
                    let res = handle(name, || handler.handle(req.clone(), message)).await;
                    // Successfully ran
                    match res {
                        Some(WsOutcome::Forward(d)) => message = d,
                        Some(WsOutcome::Failure(s)) => return Err(s),
                        Some(WsOutcome::Success(())) => return Ok(()),
                        None => return Err(super::INTERNAL_SERVER_ERROR),
                    }
                }
            }
        }
        Err(WebSocketStatus::internal(404))
    }

    pub fn is_upgrade(&self, hyper_request: &hyper::Request<hyper::Body>) -> bool {
        hyper_request.method() == hyper::Method::GET &&
            ClientRequest::parse(|n| hyper_request.headers()
                                 .get(n).map(|s| s.to_str().unwrap_or(""))
                                ).is_ok()
    }

    pub async fn handle(
        rocket: Arc<Rocket<Orbit>>,
        mut request: hyper::Request<hyper::Body>,
        h_addr: std::net::SocketAddr,
        tx: oneshot::Sender<hyper::Response<hyper::Body>>
    ) {
        let upgrade = hyper::upgrade::on(&mut request);
        let (h_parts, h_body) = request.into_parts();

        // Convert the Hyper request into a Rocket request.
        let req_res = Request::from_hyp(
            &rocket, h_parts.method, h_parts.headers, &h_parts.uri, h_addr
        );

        let mut req = match req_res {
            Ok(req) => req,
            Err(e) => {
                error!("Bad incoming request: {}", e);
                // TODO: We don't have a request to pass in, so we just
                // fabricate one. This is weird. We should let the user know
                // that we failed to parse a request (by invoking some special
                // handler) instead of doing this.
                let dummy = Request::new(&rocket, rocket_http::Method::Get, Origin::ROOT);
                let r = rocket.handle_error(Status::BadRequest, &dummy).await;
                rocket.send_response(r, tx).await;
                return;
            }
        };
        let mut data = Data::from(h_body);

        // Dispatch the request to get a response, then write that response out.
        let _token = rocket.preprocess_request(&mut req, &mut data).await;

        let protocol = Self::protocol(&req);

        //let mut response = None;
        let (websocket_channel, upgrade_tx) = WebSocketChannel::new();
        let inner_channel = InnerChannel::from_websocket(
            &websocket_channel,
            &rocket.broker,
            protocol,
        );

        let mut channels = vec![Arc::new(WebSocket::new(req, inner_channel))];

        let join = rocket.websocket_router.handle_message(
                Event::Join,
                channels[0].clone(),
                WebSocketData::Join,
            ).await;
        match join {
            Ok(()) => {
                let response = Self::create_reponse(channels[0].clone(), protocol);
                rocket.send_response(response, tx).await;
            },
            Err(s) => {
                let response = Self::handle_error(s.to_http().unwrap_or(Status::NotFound));
                rocket.send_response(response, tx).await;
                return;
            },
        }

        match protocol {
            Protocol::Naked => Self::websocket_task_naked(
                    &channels[0],
                    upgrade,
                    websocket_channel,
                    upgrade_tx
                ).await,
            Protocol::Multiplexed => {
                Self::websocket_task_multiplexed(
                    rocket.as_ref(),
                    &mut channels,
                    upgrade,
                    websocket_channel,
                    upgrade_tx
                ).await;
            },
        }
    }

    fn protocol(req: &Request<'_>) -> Protocol {
        if req.headers()
            .get("Sec-WebSocket-Protocol")
            .flat_map(|s| s.split(',').map(|s| s.trim()))
            .any(|s| s.eq_ignore_ascii_case("rocket-multiplex"))
        {
            Protocol::Multiplexed
        } else {
            Protocol::Naked
        }
    }

    fn create_reponse<'r>(req: Arc<WebSocket<'r>>, protocol: Protocol) -> Response<'r> {
        // Use websocket-codec to parse the client request
        let cl_req = match ClientRequest::parse(|n| req.request().headers().get_one(n)) {
            Ok(v) => v,
            Err(_e) => return Self::handle_error(Status::UpgradeRequired),
        };

        let mut response = Response::build();
        response.status(Status::SwitchingProtocols);
        response.header(Header::new(CONNECTION.as_str(), "upgrade"));
        response.header(Header::new(UPGRADE.as_str(), "websocket"));
        response.header(Header::new("Sec-WebSocket-Accept", cl_req.ws_accept()));
        if protocol == Protocol::Multiplexed {
            response.header(Header::new("Sec-WebSocket-Protocol", "rocket-multiplex"));
        }
        response.sized_body(None, Cursor::new("Switching to WebSocket"));
        response.finalize()
    }

    /// Construct a rocket response from the given hyper request
    fn handle_error<'_b>(status: Status) -> Response<'_b> {
        let mut response = Response::build();
        response.status(status);
        response.finalize()
    }

    // TODO run leave handler first, and fall back on this if no handler succeeds.
    async fn close_status(mut body: mpsc::Receiver<Bytes>) -> WebSocketStatus<'static> {
        if let Some(body) = body.recv().await {
            if let Ok(status) = WebSocketStatus::decode(body) {
                if status == super::OK {
                    super::OK
                } else if status == super::GOING_AWAY {
                    super::OK
                } else if status == super::EXTENSION_REQUIRED {
                    super::OK
                } else if status == super::UNKNOWN_MESSAGE_TYPE {
                    super::UNKNOWN_MESSAGE_TYPE
                } else if status == super::INVALID_DATA_TYPE {
                    super::INVALID_DATA_TYPE
                } else if status == super::POLICY_VIOLATION {
                    super::POLICY_VIOLATION
                } else if status == super::MESSAGE_TOO_LARGE {
                    super::MESSAGE_TOO_LARGE
                } else if status == super::INTERNAL_SERVER_ERROR {
                    super::INTERNAL_SERVER_ERROR
                } else if (3000..=4999).contains(&status.code()) {
                    super::OK
                } else {
                    super::PROTOCOL_ERROR
                }
            } else {
                super::PROTOCOL_ERROR
            }
        } else {
            super::OK
        }
    }

    async fn websocket_task_naked<'r, 'a: 'r>(
        request: &'a Arc<WebSocket<'r>>,
        on_upgrade: OnUpgrade,
        mut ws: WebSocketChannel,
        upgrade_tx: oneshot::Sender<Upgraded>,
    ) {
        let broker = request.rocket().broker();
        if let Ok(upgrade) = on_upgrade.await {
            let _e = upgrade_tx.send(upgrade);

            broker.subscribe(request.topic(), Protocol::Naked, &ws).await;
            while let Some(message) = ws.next().await {
                let data = match message.opcode() {
                    Opcode::Text => Data::from_ws(message, Some(false)),
                    Opcode::Binary => Data::from_ws(message, Some(true)),
                    Opcode::Ping => continue,// This should never happen
                    Opcode::Pong => continue,// This should never happen
                    Opcode::Close => {
                        if ws.should_send_close() {
                            let status = Self::close_status(message.into_parts().2).await;
                            WebSocketChannel::close(&ws.subscribe_handle(), status).await;
                        }
                        break;
                    },
                };
                let _res = request.rocket().websocket_router.handle_message(
                        Event::Message,
                        request.clone(),
                        WebSocketData::Message(data)
                    ).await;
            }
            broker.unsubscribe_all(&ws).await;
            let _e = request.rocket().websocket_router.handle_message(
                    Event::Leave,
                    request.clone(),
                    WebSocketData::Leave(super::OK)
                ).await;
        }
    }

    /// request is a vector of subscriptions to satisfy lifetime requirements
    ///
    /// # Panics
    /// Panics if request doesn't have exactly one request & origin pair
    async fn websocket_task_multiplexed<'r>(
        rocket: &'r Rocket<Orbit>,
        subscriptions: &'r mut Vec<Arc<WebSocket<'r>>>,
        on_upgrade: OnUpgrade,
        mut ws: WebSocketChannel,
        upgrade_tx: oneshot::Sender<Upgraded>,
    ) {
        if subscriptions.len() != 1 {
            panic!("WebSocket task requires exactly 1 request in the subscribtions vector");
        }
        let broker = rocket.broker();
        if let Ok(upgrade) = on_upgrade.await {
            let _e = upgrade_tx.send(upgrade);

            broker.subscribe(subscriptions[0].topic(), Protocol::Multiplexed, &ws).await;
            while let Some(message) = ws.next().await {
                let mut data = match message.opcode() {
                    Opcode::Text => Data::from_ws(message, Some(false)),
                    Opcode::Binary => Data::from_ws(message, Some(true)),
                    Opcode::Ping => continue,// This should never happen
                    Opcode::Pong => continue,// This should never happen
                    Opcode::Close => {
                        if ws.should_send_close() {
                            let status = Self::close_status(message.into_parts().2).await;
                            WebSocketChannel::close(&ws.subscribe_handle(), status).await;
                        }
                        break
                    },
                };
                let req = Self::multiplex_get_request(&mut data, &subscriptions).await;
                match req {
                    Ok(request) => {
                        let res = rocket.websocket_router.handle_message(
                            Event::Message,
                            request,
                            WebSocketData::Message(data)
                        ).await;
                        match res {
                            Ok(()) => (),
                            Err(_s) => (),
                        }
                    }
                    Err(MultiplexError::ControlMessage) =>
                        match Self::handle_control(data).await {
                            Err(message) => {
                                error_message(message, ws.subscribe_handle()).await;
                            }
                            Ok(MultiplexAction::Subscribe(topic)) => {
                                if !subscriptions.iter().any(|r| r.topic() == &topic) {
                                    let mut new_request = subscriptions[0].as_ref().clone();
                                    new_request.set_uri(topic);
                                    let new_request = Arc::new(new_request);
                                    let join = rocket.websocket_router.handle_message(
                                            Event::Join,
                                            new_request.clone(),
                                            WebSocketData::Join,
                                        ).await;
                                    match join {
                                        Ok(()) => {
                                            broker.subscribe(new_request.topic(), Protocol::Multiplexed, &ws).await;
                                            subscriptions.push(new_request);
                                        },
                                        Err(s) => {
                                            error_message(
                                                format!("ERR\u{b7}{}", s),
                                                ws.subscribe_handle()
                                            ).await;
                                        }
                                    }
                                }else {
                                    error_message(
                                        "ERR\u{b7}Already Subscribed",
                                        ws.subscribe_handle()
                                    ).await;
                                }
                            },
                            Ok(MultiplexAction::Unsubscribe(topic)) => {
                                if let Some(leave_req) = Self::remove_topic(subscriptions, topic) {
                                    broker.unsubscribe(leave_req.topic(), &ws).await;
                                    let _leave = rocket.websocket_router.handle_message(
                                        Event::Leave,
                                        leave_req.clone(),
                                        WebSocketData::Leave(super::OK)
                                    ).await;
                                    // TODO: handle errors in leave
                                } else {
                                    error_message(
                                        "ERR\u{b7}Not Subscribed",
                                        ws.subscribe_handle()
                                    ).await;
                                }
                            }
                            //_ => (),
                        }
                    Err(e) => {
                        e.send_message(ws.subscribe_handle()).await;
                    }
                }
            }
            broker.unsubscribe_all(&ws).await;
            let _e = rocket.websocket_router.handle_message(
                Event::Leave,
                subscriptions[0].clone(),
                WebSocketData::Leave(super::OK)
            ).await;
        }
    }

    fn remove_topic<'r>(
        subs: &mut Vec<Arc<WebSocket<'r>>>,
        topic: Origin<'_>
    ) -> Option<Arc<WebSocket<'r>>> {
        if let Some((index, _)) = subs.iter().enumerate().find(|(_, r)| r.topic() == &topic) {
            Some(subs.remove(index))
        }else {
            None
        }
    }

    async fn multiplex_get_request<'a, 'r>(
        data: &mut Data,
        subscribtions: &'a [Arc<WebSocket<'r>>]
    ) -> Result<Arc<WebSocket<'r>>, MultiplexError> {
        // Peek max_topic length
        let topic = data.peek(MAX_TOPIC_LENGTH + MULTIPLEX_CONTROL_CHAR.len()).await;
        if let Some((index, _)) = topic
            .windows(MULTIPLEX_CONTROL_CHAR.len())
            .enumerate()
            .find(|(_, c)| c == &MULTIPLEX_CONTROL_CHAR)
        {
            if index == 0 {
                return Err(MultiplexError::ControlMessage);
            }
            let raw = data.take(index + MULTIPLEX_CONTROL_CHAR.len()).await;
            // raw[..index] should contain everything EXCEPT the control character
            let topic = Origin::parse(std::str::from_utf8(&raw[..index])?)?;
            for r in subscribtions.iter() {
                if r.topic() == &topic {
                    return Ok(r.clone());
                }
            }
            // If there is no subscribtion to this topic, we ignore this message
            Err(MultiplexError::NotSubscribed)
        }else {
            Err(MultiplexError::TopicNotPresent)
        }
    }

    async fn handle_control<'r>(mut data: Data) -> Result<MultiplexAction, &'static str> {
        // Take the first 512 bytes of the message - which must be the entire message
        let message = String::from_utf8(data.take(512).await).map_err(|_| "INVALID\u{B7}Non UTF-8")?;
        let mut parts = message.split(MULTIPLEX_CONTROL_STR);
        let first = parts.next().ok_or("INVALID\u{B7}Improperly formatted message")?;
        if !first.is_empty() {// Err if the message did not start with the control char
            return Err("INVALID\u{B7}Improperly formatted message");
        }
        // .filter(|s| s != "") would acheive a similar effect, but I want the protocol to be more
        // strict. This could allow better optimization later, or we could loosen it without
        // breaking compatibility
        match parts.next() {
            Some("SUBSCRIBE") => {
                let topic = parts.next().ok_or("ERR\u{B7}Missing topic parameter")?;
                if parts.next().is_some() {
                    return Err("ERR\u{B7}To many arguments");
                }
                Ok(MultiplexAction::Subscribe(Origin::parse(topic)
                            .map_err(|_| "ERR\u{B7}Invalid topic Uri")?
                            .into_owned()))
            },
            Some("UNSUBSCRIBE") => {
                let topic = parts.next().ok_or("ERR\u{B7}Missing topic parameter")?;
                if parts.next().is_some() {
                    return Err("Err\u{B7}To many arguments");
                }
                Ok(MultiplexAction::Unsubscribe(Origin::parse(topic)
                            .map_err(|_| "ERR\u{B7}Invalid topic Uri")?
                            .into_owned()))
            },
            Some(_) => Err("INVALID\u{B7}Unkown control message"),
            None => Err("INVALID\u{B7}Improperly formatted message"),
        }
    }
}

enum MultiplexAction {
    Subscribe(Origin<'static>),
    Unsubscribe(Origin<'static>),
}

enum MultiplexError {
    TopicNotPresent,
    NotSubscribed,
    ControlMessage,
    Utf8Error(Utf8Error),
    UrlError(rocket_http::uri::error::Error<'static>),
}

impl MultiplexError {
    async fn send_message(self, sender: mpsc::Sender<WebSocketMessage>) {
        match self {
            Self::TopicNotPresent => error_message(
                    "ERR\u{B7}Topic not present",
                    sender
                ).await,
            Self::NotSubscribed => error_message(
                    "ERR\u{B7}Not subscribed to topic",
                    sender
                ).await,
            Self::ControlMessage => error_message(
                    "ERR\u{B7}Unexpected control message",
                    sender
                ).await,
            Self::Utf8Error(_e) => error_message(
                    "ERR\u{B7}Topic was not valid utf8",
                    sender
                ).await,
            Self::UrlError(_e) => error_message(
                    "ERR\u{B7}Topic was not a valid url",
                    sender
                ).await,
        }
    }
}

async fn error_message(bytes: impl Into<Bytes>, sender: mpsc::Sender<WebSocketMessage>) {
    let (tx, rx) = mpsc::channel(2);
    let _e = sender.send(WebSocketMessage::new(false, rx)).await;
    let _e = tx.send(bytes.into()).await;
}

impl From<Utf8Error> for MultiplexError {
    fn from(e: Utf8Error) -> Self {
        Self::Utf8Error(e)
    }
}

impl<'a> From<rocket_http::uri::error::Error<'a>> for MultiplexError {
    fn from(e: rocket_http::uri::error::Error<'a>) -> Self {
        Self::UrlError(e.into_owned())
    }
}