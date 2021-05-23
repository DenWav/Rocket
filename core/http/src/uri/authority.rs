use std::fmt::{self, Display};
use std::convert::TryFrom;
use std::borrow::Cow;

use crate::ext::IntoOwned;
use crate::parse::{Extent, IndexedStr};
use crate::uri::{as_utf8_unchecked, error::Error};

/// A URI with an authority only: `user:pass@host:8000`.
///
/// # Structure
///
/// The following diagram illustrates the syntactic structure of an authority
/// URI:
///
/// ```text
/// username:password@some.host:8088
/// |---------------| |-------| |--|
///     user info        host   port
/// ```
///
/// Only the host part of the URI is required.
#[derive(Debug, Clone)]
pub struct Authority<'a> {
    pub(crate) source: Option<Cow<'a, str>>,
    user_info: Option<IndexedStr<'a>>,
    host: IndexedStr<'a>,
    port: Option<u16>,
}

impl IntoOwned for Authority<'_> {
    type Owned = Authority<'static>;

    fn into_owned(self) -> Authority<'static> {
        Authority {
            source: self.source.into_owned(),
            user_info: self.user_info.into_owned(),
            host: self.host.into_owned(),
            port: self.port
        }
    }
}

impl<'a> Authority<'a> {
    // SAFETY: `source` must be valid UTF-8.
    // CORRECTNESS: `host` must be non-empty.
    pub(crate) unsafe fn raw(
        source: Cow<'a, [u8]>,
        user_info: Option<Extent<&'a [u8]>>,
        host: Extent<&'a [u8]>,
        port: Option<u16>
    ) -> Authority<'a> {
        Authority {
            source: Some(as_utf8_unchecked(source)),
            user_info: user_info.map(IndexedStr::from),
            host: IndexedStr::from(host),
            port,
        }
    }

    #[cfg(test)]
    pub fn new(
        user_info: impl Into<Option<&'a str>>,
        host: &'a str,
        port: impl Into<Option<u16>>,
    ) -> Self {
        Authority::const_new(user_info.into(), host, port.into())
    }

    /// PRIVATE. Used by codegen.
    #[doc(hidden)]
    pub const fn const_new(user_info: Option<&'a str>, host: &'a str, port: Option<u16>) -> Self {
        Authority {
            source: None,
            user_info: match user_info {
                Some(info) => Some(IndexedStr::Concrete(Cow::Borrowed(info))),
                None => None
            },
            host: IndexedStr::Concrete(Cow::Borrowed(host)),
            port,
        }
    }

    /// Parses the string `string` into an `Authority`. Parsing will never
    /// allocate. Returns an `Error` if `string` is not a valid authority URI.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::http::uri::Authority;
    ///
    /// // Parse a valid authority URI.
    /// let uri = Authority::parse("user:pass@host").expect("valid URI");
    /// assert_eq!(uri.user_info(), Some("user:pass"));
    /// assert_eq!(uri.host(), "host");
    /// assert_eq!(uri.port(), None);
    ///
    /// // Invalid authority URIs fail to parse.
    /// Authority::parse("https://rocket.rs").expect_err("invalid authority");
    ///
    /// // Prefer to use `uri!()` when the input is statically known:
    /// let uri = uri!("user:pass@host");
    /// assert_eq!(uri.user_info(), Some("user:pass"));
    /// assert_eq!(uri.host(), "host");
    /// assert_eq!(uri.port(), None);
    /// ```
    pub fn parse(string: &'a str) -> Result<Authority<'a>, Error<'a>> {
        crate::parse::uri::authority_from_str(string)
    }

    /// Parses the string `string` into an `Authority`. Parsing will never
    /// allocate. This method should be used instead of
    /// [`Authority::parse()`](crate::uri::Authority::parse()) when the source URI is
    /// already a `String`. Returns an `Error` if `string` is not a valid authority
    /// URI.
    pub fn parse_owned(string: String) -> Result<Authority<'static>, Error<'static>> {
        // We create a copy of a pointer to `string` to escape the borrow
        // checker. This is so that we can "move out of the borrow" later.
        //
        // For this to be correct and safe, we need to ensure that:
        //
        //  1. No `&mut` references to `string` are created after this line.
        //  2. `string` isn't dropped while `copy_of_str` is live.
        //
        // These two facts can be easily verified. An `&mut` can't be created
        // because `string` isn't `mut`. Then, `string` is clearly not dropped
        // since it's passed in to `source`.
        // let copy_of_str = unsafe { &*(string.as_str() as *const str) };
        let copy_of_str = unsafe { &*(string.as_str() as *const str) };
        let authority = Authority::parse(copy_of_str)?;
        debug_assert!(authority.source.is_some(), "Origin source parsed w/o source");

        let authority = Authority {
            host: authority.host.into_owned(),
            user_info: authority.user_info.into_owned(),
            port: authority.port,
            // At this point, it's impossible for anything to be borrowing
            // `string` except for `source`, even though Rust doesn't know it.
            // Because we're replacing `source` here, there can't possibly be a
            // borrow remaining, it's safe to "move out of the borrow".
            source: Some(Cow::Owned(string)),
        };

        Ok(authority)
    }

    /// Returns the user info part of the authority URI, if there is one.
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

    /// Returns the host part of the authority URI.
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
    pub fn host(&self) -> &str {
        self.host.from_cow_source(&self.source)
    }

    /// Returns the port part of the authority URI, if there is one.
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
}

impl<'b> PartialEq<Authority<'b>> for Authority<'_> {
    fn eq(&self, other: &Authority<'b>) -> bool {
        self.user_info() == other.user_info()
            && self.host() == other.host()
            && self.port() == other.port()
    }
}

impl Display for Authority<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(user_info) = self.user_info() {
            write!(f, "{}@", user_info)?;
        }

        self.host().fmt(f)?;
        if let Some(port) = self.port {
            write!(f, ":{}", port)?;
        }

        Ok(())
    }
}

// Because inference doesn't take `&String` to `&str`.
impl<'a> TryFrom<&'a String> for Authority<'a> {
    type Error = Error<'a>;

    fn try_from(value: &'a String) -> Result<Self, Self::Error> {
        Authority::parse(value.as_str())
    }
}

impl<'a> TryFrom<&'a str> for Authority<'a> {
    type Error = Error<'a>;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        Authority::parse(value)
    }
}

#[cfg(feature = "serde")]
mod serde {
    use std::fmt;

    use super::Authority;
    use _serde::{ser::{Serialize, Serializer}, de::{Deserialize, Deserializer, Error, Visitor}};

    impl<'a> Serialize for Authority<'a> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&self.to_string())
        }
    }

    struct AuthorityVistor;

    impl<'a> Visitor<'a> for AuthorityVistor {
        type Value = Authority<'a>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "Expecting a valid URI string")
        }

        fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
            Authority::parse_owned(v.to_string()).map_err(Error::custom)
        }

        fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
            Authority::parse_owned(v).map_err(Error::custom)
        }

        fn visit_borrowed_str<E: Error>(self, v: &'a str) -> Result<Self::Value, E> {
            Authority::parse(v).map_err(Error::custom)
        }
    }

    impl<'a> Deserialize<'a> for Authority<'a> {
        fn deserialize<D: Deserializer<'a>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_str(AuthorityVistor)
        }
    }
}
