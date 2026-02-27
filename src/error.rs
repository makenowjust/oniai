/// Errors produced by the regex engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parse(String),
    Compile(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Parse(msg) => write!(f, "parse error: {msg}"),
            Error::Compile(msg) => write!(f, "compile error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
