//! Internal Routing structs

use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::str::Utf8Error;
use std::{io::Cursor, sync::Arc};

use bytes::Bytes;
use futures::{Future, FutureExt};
use rocket_http::ext::IntoOwned;
use rocket_http::{Header, Status, hyper::upgrade::Upgraded, uri::Origin};
use rocket_http::hyper::{self, header::{CONNECTION, UPGRADE}, upgrade::OnUpgrade};
use tokio::sync::oneshot;

use websocket_codec::{ClientRequest, Opcode};

use crate::channels::WebsocketMessage;
use crate::channels::channel::to_message;
use crate::route::WebsocketEvent;
use crate::route::WsOutcome;
use crate::{Data, Request, Response, Rocket, Route, phase::Orbit};
use crate::router::{Collide, Collisions};
use yansi::Paint;

use super::broker::Broker;
use super::{WebsocketChannel, channel::InnerChannel};

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
enum Protocol {
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
pub struct WebsocketRouter {
    routes: HashMap<Event, Vec<Route>>,
}

impl WebsocketRouter {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    pub fn routes(&self) -> impl Iterator<Item = &Route> + Clone {
        self.routes.iter().flat_map(|(_, r)| r.iter())
    }

    pub fn add_route(&mut self, route: Route) {
        //if route.websocket_handler.is_some() {
            //self.routes.push(route);
        //}
        match route.websocket_handler {
            WebsocketEvent::None => (),
            WebsocketEvent::Join(_) => self.routes.entry(Event::Join).or_default().push(route),
            WebsocketEvent::Message(_) => self.routes.entry(Event::Message).or_default().push(route),
            WebsocketEvent::Leave(_) => self.routes.entry(Event::Leave).or_default().push(route),
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

    fn route<'r, 'a: 'r>(
        &'a self,
        event: Event,
        req: &'r Arc<Request<'r>>,
        topic: &'r Option<Origin<'_>>,
    ) -> impl Iterator<Item = &'a Route> + 'r {
        // Note that routes are presorted by ascending rank on each `add`.
        self.routes.get(&event)
            .into_iter()
            .flat_map(move |routes| routes.iter().filter(move |r| r.matches_topic(Arc::as_ref(req), topic)))
    }

    async fn handle_message<'r, 'a: 'r>(
        &'a self,
        event: Event,
        req: Arc<Request<'r>>,
        topic: &'r Option<Origin<'_>>,
        mut message: Data,
    ) -> Result<(), Status> {
        let req_copy = req.clone();
        for route in self.routes.get(&event)
            .into_iter()
            .flat_map(|routes| routes.iter()) {
            if route.matches_topic(req.as_ref(), topic) {
                req.set_route(route);

                let name = route.name.as_deref();
                let handler = route.websocket_handler.unwrap_ref();
                let res = handle(name, || handler.handle(req.clone(), message)).await;
                // Successfully ran
                match res {
                    Some(WsOutcome::Forward(d)) => message = d,
                    Some(WsOutcome::Failure(s)) => return Err(s),
                    Some(WsOutcome::Success(())) => return Ok(()),
                    None => return Err(Status::InternalServerError),
                }
            }
        }
        Err(Status::NotFound)
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

        //let mut response = None;
        let (websocket_channel, upgrade_tx) = WebsocketChannel::new();
        req.local_cache(|| Some(InnerChannel::from_websocket(&websocket_channel, rocket.state().unwrap())));

        let protocol = Self::protocol(&req);
        
        let mut channels = vec![Arc::new(req)];
        
        let join = rocket.websocket_router.handle_message(Event::Join, channels[0].clone(), &None, Data::local(vec![])).await;
        match join {
            Ok(()) => {
                let response = Self::create_reponse(channels[0].clone(), protocol);
                rocket.send_response(response, tx).await;
            },
            Err(s) => {
                let response = Self::handle_error(s);
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

        //let req_copy = req.clone();
        //if rocket.websocket_router.route(Event::Message, &req_copy, &None).nth(0).is_some() {
            //let (response, protocol) = Self::create_reponse(&req_copy);
            //rocket.send_response(response, tx).await;
        //}else {
            //let response = Self::handle_error(Status::NotFound);
            //rocket.send_response(response, tx).await;
        //}
    }

    fn protocol(req: &Request<'_>) -> Protocol {
        if req.headers()
            .get("Sec-WebSocket-Protocol")
            .flat_map(|s| s.split(",").map(|s| s.trim()))
            .any(|s| s.eq_ignore_ascii_case("rocket-multiplex"))
        {
            Protocol::Multiplexed
        } else {
            Protocol::Naked
        }
    }

    fn create_reponse<'r>(req: Arc<Request<'r>>, protocol: Protocol) -> Response<'r> {
        // Use websocket-codec to parse the client request
        let cl_req = match ClientRequest::parse(|n| req.headers().get_one(n)) {
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
        response.sized_body(None, Cursor::new("Switching to websocket"));
        response.finalize()
    }

    /// Construct a rocket response from the given hyper request
    fn handle_error<'_b>(status: Status) -> Response<'_b> {
        let mut response = Response::build();
        response.status(status);
        response.finalize()
    }

    async fn websocket_task_naked<'r, 'a: 'r>(
        request: &'a Arc<Request<'r>>,
        on_upgrade: OnUpgrade,
        mut ws: WebsocketChannel,
        upgrade_tx: oneshot::Sender<Upgraded>,
    ) {
        let broker = request.rocket().state::<Broker>().unwrap().clone();
        if let Ok(upgrade) = on_upgrade.await {
            let _e = upgrade_tx.send(upgrade);
            
            broker.subscribe(request.uri(), &ws);
            while let Some(message) = ws.next().await {
                let data = match message.opcode() {
                    Opcode::Text => Data::from_ws(message, Some(false)),
                    Opcode::Binary => Data::from_ws(message, Some(true)),
                    Opcode::Ping => continue,
                    Opcode::Pong => continue,
                    Opcode::Close => break,
                };
                let res = request.state.rocket.websocket_router.handle_message(
                        Event::Message,
                        request.clone(),
                        &None,
                        data
                    ).await;
            }
            broker.unsubscribe_all(&ws);
            request.state.rocket.websocket_router.handle_message(Event::Leave, request.clone(), &None, Data::local(vec![])).await;
            // TODO implement Ping/Pong (not exposed to the user)
            // TODO handle Close correctly (we should reply with Close,
            // unless we initiated it)
        }
    }

    /// request is a vector of subscriptions to satisfy lifetime requirements
    ///
    /// # Panics
    /// Panics if request doesn't have exactly one request & origin pair
    async fn websocket_task_multiplexed<'r>(
        rocket: &'r Rocket<Orbit>,
        subscriptions: &'r mut Vec<Arc<Request<'r>>>,
        on_upgrade: OnUpgrade,
        mut ws: WebsocketChannel,
        upgrade_tx: oneshot::Sender<Upgraded>,
    ) {
        // Unsafe code to escape the borrow checker, and allow us to mutate the
        // list of subscribtions
        // Safety: Honestly, I'm not sure this is actually safe
        //   The basic idea is that calls to `handle_message` release their borrows
        //   once they have been awaited, which I'm pretty sure they do. However, the
        //   borrow checker can't figure that out, hence this unsafe code.
        //let mut_subs = Pin::new(unsafe { &mut *(subscriptions as *mut Vec<_>) });
        if subscriptions.len() != 1 {
            panic!("Websocket task requires exactly 1 request in the subscribtions vector");
        }
        let broker = rocket.state::<Broker>().unwrap().clone();
        if let Ok(upgrade) = on_upgrade.await {
            let _e = upgrade_tx.send(upgrade);
            
            broker.subscribe(subscriptions[0].uri(), &ws);
            while let Some(message) = ws.next().await {
                let mut data = match message.opcode() {
                    Opcode::Text => Data::from_ws(message, Some(false)),
                    Opcode::Binary => Data::from_ws(message, Some(true)),
                    Opcode::Ping => continue,
                    Opcode::Pong => continue,
                    Opcode::Close => break,
                };
                let req = Self::multiplex_get_request(&mut data, &subscriptions).await;
                match req {
                    Ok(request) => {
                        let res = rocket.websocket_router.handle_message(
                            Event::Message,
                            request,
                            &None,
                            data
                        ).await;
                        match res {
                            Ok(()) => (),
                            Err(_s) => (),
                        }
                    }
                    Err(MultiplexError::ControlMessage) => 
                        match Self::handle_control(data, subscriptions, &broker).await {
                            Err(message) => {
                                let _e = ws.subscribe_handle().send(to_message(Cursor::new(message))).await;
                            }
                            Ok(MultiplexAction::Subscribe(topic)) => {
                                if !subscriptions.iter().any(|r| r.uri() == &topic) {
                                    let mut new_request = subscriptions[0].as_ref().clone();
                                    new_request.set_uri(topic);
                                    let new_request = Arc::new(new_request);
                                    let join = rocket.websocket_router.handle_message(Event::Join, new_request.clone(), &None, Data::local(vec![])).await;
                                    match join {
                                        Ok(()) => subscriptions.push(new_request),
                                        Err(s) => {
                                            let _e = ws.subscribe_handle().send(to_message(Cursor::new(format!("ERR\u{b7}{}", s)))).await;
                                        }
                                    }
                                }else {
                                    let _e = ws.subscribe_handle().send(to_message(Cursor::new("ERR\u{b7}Already Subscribed"))).await;
                                }
                            },
                            Ok(MultiplexAction::Unsubscribe(topic)) => {
                                if let Some(leave_req) = Self::remove_topic(subscriptions, topic) {
                                    let leave = rocket.websocket_router.handle_message(Event::Leave, leave_req.clone(), &None, Data::local(vec![])).await;
                                } else {
                                    let _e = ws.subscribe_handle().send(to_message(Cursor::new("ERR\u{b7}Not Subscribed"))).await;
                                }
                            }
                            _ => (),
                        }
                    Err(e) => {
                    }
                }
            }
            broker.unsubscribe_all(&ws);
            rocket.websocket_router.handle_message(Event::Leave, subscriptions[0].clone(), &None, Data::local(vec![])).await;
            // TODO implement Ping/Pong (not exposed to the user)
            // TODO handle Close correctly (we should reply with Close,
            // unless we initiated it)
        }
    }

    fn remove_topic<'r>(subs: &mut Vec<Arc<Request<'r>>>, topic: Origin<'_>) -> Option<Arc<Request<'r>>> {
        if let Some((index, _)) = subs.iter().enumerate().find(|(_, r)| r.uri() == &topic) {
            Some(subs.remove(index))
        }else {
            None
        }
    }

    async fn multiplex_get_request<'a, 'r>(data: &mut Data, subscribtions: &'a Vec<Arc<Request<'r>>>) -> Result<Arc<Request<'r>>, MultiplexError> {
        // Peek max_topic length
        let topic = data.peek(MAX_TOPIC_LENGTH + MULTIPLEX_CONTROL_CHAR.len()).await;
        if let Some((index, _)) = topic.windows(MULTIPLEX_CONTROL_CHAR.len()).enumerate().find(|(_, c)| c == &MULTIPLEX_CONTROL_CHAR) {
            if index == 0 {
                return Err(MultiplexError::ControlMessage);
            }
            let raw = data.take(index + MULTIPLEX_CONTROL_CHAR.len()).await;
            // raw[..index] should contain everything EXCEPT the control character
            let topic = Origin::parse(std::str::from_utf8(&raw[..index])?)?;
            for r in subscribtions.iter() {
                if r.uri() == &topic {
                    return Ok(r.clone());
                }
            }
            // If there is no subscribtion to this topic, we ignore this message
            Err(MultiplexError::NotSubscribed)
        }else {
            Err(MultiplexError::TopicNotPresent)
        }
    }

    async fn handle_control<'r>(mut data: Data, subscribtions: &mut Vec<Arc<Request<'r>>>, broker: &Broker) -> Result<MultiplexAction, &'static str> {
        // Take the first 512 bytes of the message - which must be the entire message
        let message = String::from_utf8(data.take(512).await).unwrap();
        let mut parts = message.split(MULTIPLEX_CONTROL_STR);
        let first = parts.next().ok_or("INVALID\u{B7}Improperly formatted message")?;
        if first != "" {// Err if the message did not start with the control char
            return Err("INVALID\u{B7}Improperly formatted message");
        }
        // .filter(|s| s != "") would acheive a similar effect, but I want the protocol to be more
        // strict. This could allow better optimization later, or we could loosen it without
        // breaking compatibility
        match parts.next() {
            Some("SUBSCRIBE") => {
                let topic = parts.next().ok_or("ERR\u{B7}Missing topic parameter")?;
                if parts.next().is_some() {
                    return Err("Err\u{B7}To many arguments");
                }
                Ok(MultiplexAction::Subscribe(Origin::parse(topic).map_err(|_| "ERR\u{B7}Invalid topic Uri")?.into_owned()))
            },
            Some("UNSUBSCRIBE") => {
                let topic = parts.next().ok_or("ERR\u{B7}Missing topic parameter")?;
                if parts.next().is_some() {
                    return Err("Err\u{B7}To many arguments");
                }
                Ok(MultiplexAction::Unsubscribe(Origin::parse(topic).map_err(|_| "ERR\u{B7}Invalid topic Uri")?.into_owned()))
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

/// Maximum length of topic URLs, with the possible exception of the original URL used to connect.
///
/// TODO: investigate the exception, and potentially handle it
const MAX_TOPIC_LENGTH: usize = 100;

/// Control character for seperating information in 'rocket-mutltiplex'
///
/// U+00B7 (MIDDLE DOT) is a printable, valid UTF8 character, but it is never valid within a URL.
/// To include it, or any other invalid character in a URL, it must be percent-encoded. This means
/// there is no ambiguity between a URL containing this character, and a URL terminated by this
/// character
const MULTIPLEX_CONTROL_STR: &'static str = "\u{B7}";
const MULTIPLEX_CONTROL_CHAR: &'static [u8] = MULTIPLEX_CONTROL_STR.as_bytes();

// Full rocket-multiplex protocol description:
//
// Rocket uses the Origin URL of a websocket request as a topic identifier. The rocket-multiplex
// proprotocol allows sending messages to multiple topics using a single websocket connection.
//
// Topic URLS are limited to `MAX_TOPIC_LENGTH = 100`, to prevent potential DoS attacks.
//
// # Messages: Data & Control
//
// ## Data 
//
// Data messages start with the topic URL they should be sent to, followed by `'\u{00B7}'`.
// This is followed by the contents of the message. The length of the message is not limited by
// this protocol, although it is likely limited by Rocket in other ways.
//
// Data messages sent to a topic the client is not subscribed to result in an error being sent to
// the client.
//
//
// # Control
//
// Control messages are limited to 512 bytes total, although there should never be
// a reason to create a longer message. Control messages take the form:
//
// `S ACTION S (PARAM S)*`
//
// Where `S` = `\u{00B7}`, `ACTION` is one of the following actions, and `PARAM` is one of the
// positional paramaters associated with the aciton.
//
// # Actions
//
// - Subscribe: `SUBSCRIBE`, [Topic]; subscribed the client to a specific topic URL, as if the client had
// opened a second websocket connection to the topic URL
// - Unsubscribe: `UNSUBSCRIBE`, [TOPIC]; unsubscribes the client from a specific topic URL, as if
// the client has closed the second websocket connection to the topic URL
// - Unsubscribe all: There is no specific unsubscribe all action, although closing the websocket
// connection is treated as an unsubscribe all
//
// - Ok: `OK`, []; Sent as a response to an action, this indicates that the action
// succeeded.
// - Err: `ERR`, [ACTION, PARAMS]; Sent as a response to an action, this indicates that the action
// failed.
// - Invalid message: `INVALID`, [REASON]; Sent as a response to an message the client is not allowed to
// send. Currently, this is only sent in response to a message to a topic the client is not
// subscribed to.