use crate::term::encode::EncodeError;
use std::fmt;

#[derive(Debug)]
pub enum StoreError {
    /// A term could not be encoded (bad escape, invalid UTF-8).
    Encode(EncodeError),
    /// A string passed as a term key is not a canonical key.
    InvalidKey(String),
    Dictionary(s_dict::DictError),
    DictionaryLoad(s_dict::LoadError),
    Graph(s_graph::TreeError),
    Parse(s_parser::ParseError),
    Io(std::io::Error),
    /// A query that parsed but cannot be evaluated (unsupported feature,
    /// unresolvable prefix, ...).
    Query(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Encode(e) => write!(f, "{e}"),
            StoreError::InvalidKey(k) => write!(f, "not a canonical term key: {k:?}"),
            StoreError::Dictionary(e) => write!(f, "dictionary: {e}"),
            StoreError::DictionaryLoad(e) => write!(f, "dictionary load: {e}"),
            StoreError::Graph(e) => write!(f, "graph: {e}"),
            StoreError::Parse(e) => write!(f, "parse: {e}"),
            StoreError::Io(e) => write!(f, "I/O: {e}"),
            StoreError::Query(m) => write!(f, "query: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

macro_rules! from_error {
    ($($t:ty => $v:ident),* $(,)?) => {
        $(impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                StoreError::$v(e)
            }
        })*
    };
}

from_error!(
    EncodeError => Encode,
    s_dict::DictError => Dictionary,
    s_dict::LoadError => DictionaryLoad,
    s_graph::TreeError => Graph,
    s_parser::ParseError => Parse,
    std::io::Error => Io,
);

pub type Result<T> = std::result::Result<T, StoreError>;
