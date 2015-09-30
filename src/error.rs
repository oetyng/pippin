//! Internal error structs used by Pippin

use std::{io, error, fmt, result, string};

/// Our custom result type
pub type Result<T> = result::Result<T, Error>;

/// Our custom compound error type
pub enum Error {
    Read(ReadError),
    Arg(ArgError),
    Io(io::Error),
    Utf8(string::FromUtf8Error)
}

/// For read errors; adds a read position
pub struct ReadError {
    msg: &'static str,
    pos: usize
}

/// Any error where an invalid argument was supplied
pub struct ArgError {
    msg: &'static str
}

impl Error {
    pub fn read(msg: &'static str, pos: usize) -> Error {
        Error::Read(ReadError { msg: msg, pos: pos })
    }
    pub fn arg(msg: &'static str) -> Error {
        Error::Arg(ArgError { msg: msg })
    }
}

// Important impls for compound type
impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::Read(ref e) => e.msg,
            Error::Arg(ref e) => e.msg,
            Error::Io(ref e) => e.description(),
            Error::Utf8(ref e) => e.description(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        match *self {
            Error::Read(ref e) => write!(f, "Position {}: {}", e.pos, e.msg),
            Error::Arg(ref e) => write!(f, "Invalid argument: {}", e.msg),
            Error::Io(ref e) => e.fmt(f),
            Error::Utf8(ref e) => e.fmt(f),
        }
    }
}
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        match *self {
            Error::Read(ref e) => write!(f, "Position {}: {}", e.pos, e.msg),
            Error::Arg(ref e) => write!(f, "Invalid argument: {}", e.msg),
            Error::Io(ref e) => e.fmt(f),
            Error::Utf8(ref e) => e.fmt(f),
        }
    }
}

// From impls
impl From<ReadError> for Error {
    fn from(e: ReadError) -> Error { Error::Read(e) }
}
impl From<ArgError> for Error {
    fn from(e: ArgError) -> Error { Error::Arg(e) }
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error { Error::Io(e) }
}
impl From<string::FromUtf8Error> for Error {
    fn from(e: string::FromUtf8Error) -> Error { Error::Utf8(e) }
}