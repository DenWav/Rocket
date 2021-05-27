use crate::tokio::io::AsyncReadExt;
use crate::data::data_stream::DataStream;
use crate::data::{ByteUnit, StreamReader};

/// The number of bytes to read into the "peek" buffer.
pub const PEEK_BYTES: usize = 512;

/// Type representing the body data of a request.
///
/// This type is the only means by which the body of a request can be retrieved.
/// This type is not usually used directly. Instead, data guards (types that
/// implement [`FromData`](crate::data::FromData)) are created indirectly via
/// code generation by specifying the `data = "<var>"` route parameter as
/// follows:
///
/// ```rust
/// # #[macro_use] extern crate rocket;
/// # type DataGuard = rocket::data::Data;
/// #[post("/submit", data = "<var>")]
/// fn submit(var: DataGuard) { /* ... */ }
/// # fn main() { }
/// ```
///
/// Above, `DataGuard` can be any type that implements `FromData`. Note that
/// `Data` itself implements `FromData`.
///
/// # Reading Data
///
/// Data may be read from a `Data` object by calling either the
/// [`open()`](Data::open()) or [`peek()`](Data::peek()) methods.
///
/// The `open` method consumes the `Data` object and returns the raw data
/// stream. The `Data` object is consumed for safety reasons: consuming the
/// object ensures that holding a `Data` object means that all of the data is
/// available for reading.
///
/// The `peek` method returns a slice containing at most 512 bytes of buffered
/// body data. This enables partially or fully reading from a `Data` object
/// without consuming the `Data` object.
pub struct Data {
    buffer: Vec<u8>,
    is_complete: bool,
    stream: StreamReader,
    ws_binary: Option<bool>,
}

impl Data {
    /// Create a `Data` from a recognized `stream`.
    pub(crate) fn from<S: Into<StreamReader>>(stream: S) -> Data {
        // TODO.async: This used to also set the read timeout to 5 seconds.
        // Such a short read timeout is likely no longer necessary, but some
        // kind of idle timeout should be implemented.

        let stream = stream.into();
        let buffer = Vec::with_capacity(PEEK_BYTES / 8);
        Data { buffer, stream, is_complete: false, ws_binary: None }
    }

    /// Create a `Data` from a recognized `stream`.
    pub(crate) fn from_ws<S: Into<StreamReader>>(stream: S, ws_binary: Option<bool>) -> Data {
        // TODO.async: This used to also set the read timeout to 5 seconds.
        // Such a short read timeout is likely no longer necessary, but some
        // kind of idle timeout should be implemented.

        let stream = stream.into();
        let buffer = Vec::with_capacity(PEEK_BYTES / 8);
        Data { buffer, stream, is_complete: false, ws_binary }
    }

    /// This creates a `data` object from a local data source `data`.
    #[inline]
    pub(crate) fn local(data: Vec<u8>) -> Data {
        Data {
            buffer: data,
            stream: StreamReader::empty(),
            is_complete: true,
            ws_binary: None,
        }
    }

    /// Returns the raw data stream, limited to `limit` bytes.
    ///
    /// The stream contains all of the data in the body of the request,
    /// including that in the `peek` buffer. The method consumes the `Data`
    /// instance. This ensures that a `Data` type _always_ represents _all_ of
    /// the data in a request.
    ///
    /// # Example
    ///
    /// ```rust
    /// use rocket::data::{Data, ToByteUnit};
    ///
    /// # const SIZE_LIMIT: u64 = 2 << 20; // 2MiB
    /// fn handler(data: Data) {
    ///     let stream = data.open(2.mebibytes());
    /// }
    /// ```
    pub fn open(self, limit: ByteUnit) -> DataStream {
        DataStream::new(self.buffer, self.stream, limit.into())
    }

    /// Retrieve at most `num` bytes from the `peek` buffer without consuming
    /// `self`.
    ///
    /// The peek buffer contains at most 512 bytes of the body of the request.
    /// The actual size of the returned buffer is the `max` of the request's
    /// body, `num` and `512`. The [`peek_complete`](#method.peek_complete)
    /// method can be used to determine if this buffer contains _all_ of the
    /// data in the body of the request.
    ///
    /// # Examples
    ///
    /// In a data guard:
    ///
    /// ```rust
    /// use rocket::request::{self, Request, FromRequest};
    /// use rocket::data::{self, Data, FromData};
    /// # struct MyType;
    /// # type MyError = String;
    ///
    /// #[rocket::async_trait]
    /// impl<'r> FromData<'r> for MyType {
    ///     type Error = MyError;
    ///
    ///     async fn from_data(
    ///         req: &'r Request<'_>,
    ///         mut data: Data
    ///     ) -> data::Outcome<Self, Self::Error> {
    ///         if data.peek(2).await != b"hi" {
    ///             return data::Outcome::Forward(data)
    ///         }
    ///
    ///         /* .. */
    ///         # unimplemented!()
    ///     }
    /// }
    /// ```
    ///
    /// In a fairing:
    ///
    /// ```
    /// use rocket::{Rocket, Request, Data, Response};
    /// use rocket::fairing::{Fairing, Info, Kind};
    /// # struct MyType;
    ///
    /// #[rocket::async_trait]
    /// impl Fairing for MyType {
    ///     fn info(&self) -> Info {
    ///         Info {
    ///             name: "Data Peeker",
    ///             kind: Kind::Request
    ///         }
    ///     }
    ///
    ///     async fn on_request(&self, req: &mut Request<'_>, data: &mut Data) {
    ///         if data.peek(2).await == b"hi" {
    ///             /* do something; body data starts with `"hi"` */
    ///         }
    ///
    ///         /* .. */
    ///         # unimplemented!()
    ///     }
    /// }
    /// ```
    pub async fn peek(&mut self, num: usize) -> &[u8] {
        let num = std::cmp::min(PEEK_BYTES, num);
        let mut len = self.buffer.len();
        if len >= num {
            return &self.buffer[..num];
        }

        while len < num {
            match self.stream.read_buf(&mut self.buffer).await {
                Ok(0) => { self.is_complete = true; break },
                Ok(n) => len += n,
                Err(e) => {
                    error_!("Failed to read into peek buffer: {:?}.", e);
                    break;
                }
            }
        }

        &self.buffer[..std::cmp::min(len, num)]
    }

    /// Returns true if the `peek` buffer contains all of the data in the body
    /// of the request. Returns `false` if it does not or if it is not known if
    /// it does.
    ///
    /// # Example
    ///
    /// ```rust
    /// use rocket::data::Data;
    ///
    /// async fn handler(mut data: Data) {
    ///     if data.peek_complete() {
    ///         println!("All of the data: {:?}", data.peek(512).await);
    ///     }
    /// }
    /// ```
    #[inline(always)]
    pub fn peek_complete(&self) -> bool {
        self.is_complete
    }

    /// Returns Some if this data was created from a websocket, and None otherwise
    ///
    /// The inner boolean is true when the websocket message was sent as binary, while
    /// it is false if the type was text
    pub fn websocket_is_binary(&self) -> Option<bool> {
        self.ws_binary
    }

    /// Takes the first `num` bytes from this request. This violates the premise in `peek`, that
    /// `Data` always contains all the data, however this is nessecary to implement Websocket
    /// Multiplexing. For that reason, this method is pub(crate), so it cannot be used outside of
    /// Rocket itself.
    ///
    /// In the case of Websocket Multiplexing, the premise outlined in `peek` doesn't really apply:
    /// the data actually carries header information (the topic).
    pub(crate) async fn take(&mut self, num: usize) -> Vec<u8> {
        let tmp = self.peek(num).await.len();
        let mut tmp = self.buffer.split_off(tmp);
        std::mem::swap(&mut self.buffer, &mut tmp);
        tmp
    }
}
