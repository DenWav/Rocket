use std::borrow::Cow;
use std::convert::TryFrom;

use crate::{ext::IntoOwned, parse::IndexedBytes};
use crate::parse::{Extent, IndexedStr};
use crate::uri::{Authority, Path, Query, Data, Error, as_utf8_unchecked, fmt};

/// A URI with a scheme, authority, path, and query.
///
/// # Structure
///
/// The following diagram illustrates the syntactic structure of an absolute
/// URI with all optional parts:
///
/// ```text
///  http://user:pass@domain.com:4444/foo/bar?some=query
///  |--|  |------------------------||------| |--------|
/// scheme          authority          path      query
/// ```
///
/// Only the scheme part of the URI is required.
///
/// # Normalization
///
/// Rocket prefers _normalized_ absolute URIs, an absolute URI with the
/// following properties:
///
///   * The path and query, if any, are normalized with no empty segments.
///   * If there is an authority, the path is empty or absolute with more than
///     one character.
///
/// The [`Absolute::is_normalized()`] method checks for normalization while
/// [`Absolute::into_normalized()`] normalizes any absolute URI.
///
/// As an example, the following URIs are all valid, normalized URIs:
///
/// ```rust
/// # extern crate rocket;
/// # use rocket::http::uri::Absolute;
/// # let valid_uris = [
/// "http://rocket.rs",
/// "scheme:/foo/bar",
/// "scheme:/foo/bar?abc",
/// # ];
/// # for uri in &valid_uris {
/// #     let uri = Absolute::parse(uri).unwrap();
/// #     assert!(uri.is_normalized(), "{} non-normal?", uri);
/// # }
/// ```
///
/// By contrast, the following are valid but non-normal URIs:
///
/// ```rust
/// # extern crate rocket;
/// # use rocket::http::uri::Absolute;
/// # let invalid = [
/// "http://rocket.rs/",    // trailing '/'
/// "ftp:/a/b/",            // trailing empty segment
/// "ftp:/a//c//d",         // two empty segments
/// "ftp:/a/b/?",           // empty path segment
/// "ftp:/?foo&",           // trailing empty query segment
/// # ];
/// # for uri in &invalid {
/// #   assert!(!Absolute::parse(uri).unwrap().is_normalized());
/// # }
/// ```
///
/// ## Serde
///
/// For convience, `Absolute` implements `Serialize` and `Deserialize`.
/// Because `Absolute` has a lifetime parameter, serde requires a borrow
/// attribute for the derive macro to work.
///
/// ```ignore
/// #[derive(Deserialize)]
/// struct Uris<'a> {
///     #[serde(borrow)]
///     absolute: Absolute<'a>,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Absolute<'a> {
    pub(crate) source: Option<Cow<'a, str>>,
    pub(crate) scheme: IndexedStr<'a>,
    pub(crate) user_info: Option<IndexedStr<'a>>,
    pub(crate) host: Option<IndexedStr<'a>>,
    pub(crate) port: Option<u16>,
    pub(crate) path: Data<'a, fmt::Path>,
    pub(crate) query: Option<Data<'a, fmt::Query>>,
}

impl IntoOwned for Absolute<'_> {
    type Owned = Absolute<'static>;

    fn into_owned(self) -> Self::Owned {
        Absolute {
            source: self.source.into_owned(),
            scheme: self.scheme.into_owned(),
            user_info: self.user_info.into_owned(),
            host: self.host.into_owned(),
            port: self.port,
            path: self.path.into_owned(),
            query: self.query.into_owned(),
        }
    }
}

impl<'a> Absolute<'a> {
    /// Parses the string `string` into an `Absolute`. Parsing will never
    /// allocate. Returns an `Error` if `string` is not a valid absolute URI.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::http::uri::Absolute;
    ///
    /// // Parse a valid authority URI.
    /// let uri = Absolute::parse("https://rocket.rs").expect("valid URI");
    /// assert_eq!(uri.scheme(), "https");
    /// assert_eq!(uri.host().unwrap(), "rocket.rs");
    /// assert_eq!(uri.path(), "");
    /// assert!(uri.query().is_none());
    ///
    /// // Prefer to use `uri!()` when the input is statically known:
    /// let uri = uri!("https://rocket.rs");
    /// assert_eq!(uri.scheme(), "https");
    /// assert_eq!(uri.host().unwrap(), "rocket.rs");
    /// assert_eq!(uri.path(), "");
    /// assert!(uri.query().is_none());
    /// ```
    pub fn parse(string: &'a str) -> Result<Absolute<'a>, Error<'a>> {
        crate::parse::uri::absolute_from_str(string)
    }

    /// Parses the string `string` into an `Absolute`. Parsing will never
    /// May allocate on error.
    ///
    /// This method should be used instead of [`Absolute::parse()`] when
    /// the source URI is already a `String`. Returns an `Error` if `string` is
    /// not a valid absolute URI.
    ///
    /// # Example
    ///
    /// ```rust
    /// # extern crate rocket;
    /// use rocket::http::uri::Absolute;
    ///
    /// let source = format!("https://rocket.rs/foo/{}/three", 2);
    /// let uri = Absolute::parse_owned(source).expect("valid URI");
    /// assert_eq!(uri.scheme(), "https");
    /// assert_eq!(uri.host(), "rocket.rs");
    /// assert_eq!(uri.path(), "/foo/2/three");
    /// assert!(uri.query().is_none());
    /// ```
    pub fn parse_owned(string: String) -> Result<Absolute<'static>, Error<'static>> {
        let absolute = Absolute::parse(&string).map_err(|e| e.into_owned())?;
        debug_assert!(absolute.source.is_some(), "Origin source parsed w/o source");

        let absolute = Absolute {
            scheme: absolute.scheme.into_owned(),
            user_info: absolute.user_info.into_owned(),
            host: absolute.host.into_owned(),
            port: absolute.port,
            query: absolute.query.into_owned(),
            path: absolute.path.into_owned(),
            source: Some(Cow::Owned(string)),
        };

        Ok(absolute)
    }

    /// Returns the scheme part of the absolute URI.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// let uri = uri!("ftp://127.0.0.1");
    /// assert_eq!(uri.scheme(), "ftp");
    /// ```
    #[inline(always)]
    pub fn scheme(&self) -> &str {
        self.scheme.from_cow_source(&self.source)
    }

    /// Returns the user info part of the absolute URI, if there is one.
    ///
    /// # Example
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// let uri = uri!("username:password@host");
    /// assert_eq!(uri.user_info(), Some("username:password"));
    /// ```
    pub fn user_info(&self) -> Option<&str> {
        self.user_info.as_ref().map(|u| u.from_cow_source(&self.source))
    }

    /// Returns the host part of the absolute URI.
    ///
    ///
    /// If the host was provided in brackets (such as for IPv6 addresses), the
    /// brackets will not be part of the returned string.
    ///
    /// # Example
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    ///
    /// let uri = uri!("domain.com:123");
    /// assert_eq!(uri.host(), "domain.com");
    ///
    /// let uri = uri!("username:password@host:123");
    /// assert_eq!(uri.host(), "host");
    ///
    /// let uri = uri!("username:password@[1::2]:123");
    /// assert_eq!(uri.host(), "[1::2]");
    /// ```
    #[inline(always)]
    pub fn host(&self) -> Option<&str> {
        self.host.as_ref().map(|host| host.from_cow_source(&self.source))
    }

    /// Returns the port part of the absolute URI, if there is one.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// // With a port.
    /// let uri = uri!("username:password@host:123");
    /// assert_eq!(uri.port(), Some(123));
    ///
    /// let uri = uri!("domain.com:8181");
    /// assert_eq!(uri.port(), Some(8181));
    ///
    /// // Without a port.
    /// let uri = uri!("username:password@host");
    /// assert_eq!(uri.port(), None);
    /// ```
    #[inline(always)]
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Returns the path part. May be empty.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// let uri = uri!("ftp://rocket.rs/foo/bar");
    /// assert_eq!(uri.path(), "/foo/bar");
    ///
    /// let uri = uri!("ftp://rocket.rs");
    /// assert!(uri.path().is_empty());
    /// ```
    #[inline(always)]
    pub fn path(&self) -> Path<'_> {
        Path { source: &self.source, data: &self.path }
    }

    /// Returns the query part with the leading `?`. May be empty.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// let uri = uri!("ftp://rocket.rs/foo?bar");
    /// assert_eq!(uri.query().unwrap(), "bar");
    ///
    /// let uri = uri!("ftp://rocket.rs");
    /// assert!(uri.query().is_none());
    /// ```
    #[inline(always)]
    pub fn query(&self) -> Option<Query<'_>> {
        self.query.as_ref().map(|data| Query { source: &self.source, data })
    }

    /// Removes the query part of this URI, if there is any.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// let mut uri = uri!("ftp://rocket.rs/foo?bar");
    /// assert_eq!(uri.query().unwrap(), "bar");
    ///
    /// uri.clear_query();
    /// assert!(uri.query().is_none());
    /// ```
    #[inline(always)]
    pub fn clear_query(&mut self) {
        self.set_query(None);
    }

    /// Returns `true` if `self` is normalized. Otherwise, returns `false`.
    ///
    /// See [Normalization](#normalization) for more information on what it
    /// means for an absolute URI to be normalized. Note that `uri!()` always
    /// returns a normalized version of its static input.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::http::uri::Absolute;
    ///
    /// assert!(uri!("http://rocket.rs").is_normalized());
    /// assert!(uri!("http://rocket.rs///foo////bar").is_normalized());
    ///
    /// assert!(Absolute::parse("http:/").unwrap().is_normalized());
    /// assert!(Absolute::parse("http://").unwrap().is_normalized());
    /// assert!(Absolute::parse("http://foo.rs/foo/bar").unwrap().is_normalized());
    /// assert!(Absolute::parse("foo:bar").unwrap().is_normalized());
    ///
    /// assert!(!Absolute::parse("git://rocket.rs/").unwrap().is_normalized());
    /// assert!(!Absolute::parse("http:/foo//bar").unwrap().is_normalized());
    /// assert!(!Absolute::parse("foo:bar?baz&&bop").unwrap().is_normalized());
    /// ```
    pub fn is_normalized(&self) -> bool {
        let normalized_query = self.query().map_or(true, |q| q.is_normalized());
        if self.host().is_some() && !self.path().is_empty() {
            self.path().is_normalized(true)
                && self.path() != "/"
                && normalized_query
        } else {
            self.path().is_normalized(false) && normalized_query
        }
    }

    /// Normalizes `self` in-place. Does nothing if `self` is already
    /// normalized.
    ///
    /// # Example
    ///
    /// ```rust
    /// use rocket::http::uri::Absolute;
    ///
    /// let mut uri = Absolute::parse("git://rocket.rs/").unwrap();
    /// assert!(!uri.is_normalized());
    /// uri.normalize();
    /// assert!(uri.is_normalized());
    ///
    /// let mut uri = Absolute::parse("http:/foo//bar").unwrap();
    /// assert!(!uri.is_normalized());
    /// uri.normalize();
    /// assert!(uri.is_normalized());
    ///
    /// let mut uri = Absolute::parse("foo:bar?baz&&bop").unwrap();
    /// assert!(!uri.is_normalized());
    /// uri.normalize();
    /// assert!(uri.is_normalized());
    /// ```
    pub fn normalize(&mut self) {
        if self.host().is_some() && !self.path().is_empty() {
            if self.path() == "/" {
                self.set_path("");
            } else if !self.path().is_normalized(true) {
                self.path = self.path().to_normalized(true);
            }
        } else {
            self.path = self.path().to_normalized(false);
        }

        if let Some(query) = self.query() {
            if !query.is_normalized() {
                self.query = query.to_normalized();
            }
        }
    }

    /// Normalizes `self`. This is a no-op if `self` is already normalized.
    ///
    /// # Example
    ///
    /// ```rust
    /// use rocket::http::uri::Absolute;
    ///
    /// let mut uri = Absolute::parse("git://rocket.rs/").unwrap();
    /// assert!(!uri.is_normalized());
    /// assert!(uri.into_normalized().is_normalized());
    ///
    /// let mut uri = Absolute::parse("http:/foo//bar").unwrap();
    /// assert!(!uri.is_normalized());
    /// assert!(uri.into_normalized().is_normalized());
    ///
    /// let mut uri = Absolute::parse("foo:bar?baz&&bop").unwrap();
    /// assert!(!uri.is_normalized());
    /// assert!(uri.into_normalized().is_normalized());
    /// ```
    pub fn into_normalized(mut self) -> Self {
        self.normalize();
        self
    }

    // TODO: add methods
}

/// PRIVATE API.
#[doc(hidden)]
impl<'a> Absolute<'a> {
    /// PRIVATE. Used by parser.
    ///
    /// SAFETY: `source` must be valid UTF-8.
    /// CORRECTNESS: `scheme` must be non-empty.
    #[inline]
    pub(crate) unsafe fn raw(
        source: Cow<'a, [u8]>,
        scheme: Extent<&'a [u8]>,
        authority: Option<Authority<'a>>,
        path: Extent<&'a [u8]>,
        query: Option<Extent<&'a [u8]>>,
    ) -> Absolute<'a> {
        Absolute {
            scheme: scheme.into(),
            user_info: authority.as_ref().map(|a|
                a.user_info().map(|u| IndexedBytes::unchecked_from(u.as_bytes(), &source))
            ).flatten().map(|u| u.coerce()),
            host: authority.as_ref().map(|a| IndexedBytes::unchecked_from(a.host().as_bytes(), &source))
                .map(|h| h.coerce()),
            port: authority.as_ref().map(|a| a.port()).flatten(),
            path: Data::raw(path),
            query: query.map(Data::raw),
            source: Some(as_utf8_unchecked(source)),
        }
    }

    /// PRIVATE. Used by tests.
    #[cfg(test)]
    pub fn new(
        scheme: &'a str,
        user_info: impl Into<Option<&'a str>>,
        host: impl Into<Option<&'a str>>,
        port: impl Into<Option<u16>>,
        path: &'a str,
        query: impl Into<Option<&'a str>>,
    ) -> Absolute<'a> {
        assert!(!scheme.is_empty());
        Absolute::const_new(
            scheme,
            user_info.into(),
            host.into(),
            port.into(),
            path,
            query.into())
    }

    /// PRIVATE. Used by codegen.
    #[doc(hidden)]
    pub const fn const_new(
        scheme: &'a str,
        user_info: Option<&'a str>,
        host: Option<&'a str>,
        port: Option<u16>,
        path: &'a str,
        query: Option<&'a str>,
    ) -> Absolute<'a> {
        //debug_assert!(host.is_some() || (user_info.is_none() && port.is_some()));
        Absolute {
            source: None,
            scheme: IndexedStr::Concrete(Cow::Borrowed(scheme)),
            user_info: match user_info {
                Some(info) => Some(IndexedStr::Concrete(Cow::Borrowed(info))),
                None => None
            },
            host: match host {
                Some(host) => Some(IndexedStr::Concrete(Cow::Borrowed(host))),
                None => None,
            },
            port,
            path: Data {
                value: IndexedStr::Concrete(Cow::Borrowed(path)),
                decoded_segments: state::Storage::new(),
            },
            query: match query {
                Some(query) => Some(Data {
                    value: IndexedStr::Concrete(Cow::Borrowed(query)),
                    decoded_segments: state::Storage::new(),
                }),
                None => None,
            },
        }
    }

    // TODO: Have a way to get a validated `path` to do this. See `Path`?
    pub(crate) fn set_path<P>(&mut self, path: P)
        where P: Into<Cow<'a, str>>
    {
        self.path = Data::new(path.into());
    }

    // TODO: Have a way to get a validated `query` to do this. See `Query`?
    pub(crate) fn set_query<Q: Into<Option<Cow<'a, str>>>>(&mut self, query: Q) {
        self.query = query.into().map(Data::new);
    }
}

impl<'a> TryFrom<&'a String> for Absolute<'a> {
    type Error = Error<'a>;

    fn try_from(value: &'a String) -> Result<Self, Self::Error> {
        Absolute::parse(value.as_str())
    }
}

impl<'a> TryFrom<&'a str> for Absolute<'a> {
    type Error = Error<'a>;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        Absolute::parse(value)
    }
}

impl<'a, 'b> PartialEq<Absolute<'b>> for Absolute<'a> {
    fn eq(&self, other: &Absolute<'b>) -> bool {
        self.scheme() == other.scheme()
            && self.user_info() == other.user_info()
            && self.host() == other.host()
            && self.port() == other.port()
            && self.path() == other.path()
            && self.query() == other.query()
    }
}

impl std::fmt::Display for Absolute<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:", self.scheme())?;
        
        if let Some(host) = self.host() {
            write!(f, "//")?;
            if let Some(user_info) = self.user_info() {
                write!(f, "{}@", user_info)?;
            }

            write!(f, "{}", host)?;
            if let Some(port) = self.port {
                write!(f, ":{}", port)?;
            }
        }

        write!(f, "{}", self.path())?;
        if let Some(query) = self.query() {
            write!(f, "?{}", query)?;
        }

        Ok(())
    }
}

#[cfg(feature = "serde")]
mod serde {
    use std::fmt;

    use super::Absolute;
    use _serde::{ser::{Serialize, Serializer}, de::{Deserialize, Deserializer, Error, Visitor}};

    impl<'a> Serialize for Absolute<'a> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&self.to_string())
        }
    }

    struct AbsoluteVistor;

    impl<'a> Visitor<'a> for AbsoluteVistor {
        type Value = Absolute<'a>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "absolute Uri")
        }

        fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
            Absolute::parse_owned(v.to_string()).map_err(Error::custom)
        }

        fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
            Absolute::parse_owned(v).map_err(Error::custom)
        }

        fn visit_borrowed_str<E: Error>(self, v: &'a str) -> Result<Self::Value, E> {
            Absolute::parse(v).map_err(Error::custom)
        }
    }

    impl<'a, 'de: 'a> Deserialize<'de> for Absolute<'a> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_str(AbsoluteVistor)
        }
    }
}
