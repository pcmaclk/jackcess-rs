//! Error type shared by the whole crate.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Generic jackcess error, equivalent to java.io.IOException with a
    /// message.
    Jackcess(String),
    /// Equivalent to java.lang.IllegalArgumentException / IllegalStateException
    /// thrown by the Java library.
    IllegalArgument(String),
    IllegalState(String),
    /// Equivalent to `com.healthmarketscience.jackcess.crypt
    /// .InvalidCredentialsException` (the password of an encrypted Office
    /// database is wrong).  The message is the Java exception message and is
    /// reported undecorated, the way Java reports it.
    InvalidCredentials(String),
    Unsupported(String),
    /// Equivalent to `com.healthmarketscience.jackcess
    /// .ConstraintViolationException` (a foreign key, unique or required
    /// constraint was violated).  The message is the Java exception message.
    ConstraintViolation(String),
    /// Equivalent to `com.healthmarketscience.jackcess.InvalidValueException`
    /// (a column validator rejected the value).  The message is the Java
    /// exception message; like a constraint violation it is not wrapped in the
    /// operation context.
    InvalidValue(String),
    /// Equivalent to `com.healthmarketscience.jackcess.BatchUpdateException`
    /// (`TableImpl.addRows` for a batch of rows): the message is the Java
    /// `getMessage()` (`"Failed adding rows (Db=...;Table=...): " + cause`),
    /// `update_count` is Java's `getUpdateCount()` and `cause` is the
    /// underlying failure (which stays inspectable, so the root cause is
    /// unchanged).
    BatchUpdate {
        update_count: i32,
        message: String,
        cause: Box<Error>,
    },
    /// Index out of bounds / buffer underflow, mirroring the exceptions which
    /// java.nio throws.
    BufferUnderflow(String),
    BufferOverflow(String),
    /// `java.nio.InvalidMarkException`: a buffer was reset without a (valid)
    /// mark, e.g. because a `Buffer.position(int)` call in front of the mark
    /// discarded it.
    ///
    /// Java's `InvalidMarkException` has no message (`getMessage()` is
    /// `null`), so the variant carries no payload: the port's diagnostic text
    /// (`"invalid mark"`) is not part of Java's exception.  It is the error the
    /// corrupt null mask path of [`crate::table::Table::read_row_at`] reports
    /// for `ByteBuffer.reset()` (`table.rs`), which keeps an error handler able
    /// to see the right exception kind without a text based canonicalizer.
    InvalidMark,
    /// `java.lang.NumberFormatException` (a `RuntimeException` the Java library
    /// does not catch): the message is Java's `getMessage()`, e.g.
    /// `For input string: "abc"` from `Double.valueOf(String)`.
    NumberFormat(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Jackcess(m) => write!(f, "{m}"),
            Error::IllegalArgument(m) => write!(f, "illegal argument: {m}"),
            Error::IllegalState(m) => write!(f, "illegal state: {m}"),
            Error::InvalidCredentials(m) => write!(f, "{m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::ConstraintViolation(m) => write!(f, "{m}"),
            Error::InvalidValue(m) => write!(f, "{m}"),
            // Java's `BatchUpdateException.getMessage()` is already the full
            // text (`msg + ": " + cause.toString()`), so it is reported as is
            Error::BatchUpdate { message, .. } => write!(f, "{message}"),
            Error::BufferUnderflow(m) => write!(f, "buffer underflow: {m}"),
            Error::BufferOverflow(m) => write!(f, "buffer overflow: {m}"),
            // the port's diagnostic text; Java's `InvalidMarkException` has no
            // message (see [`Error::java_to_string`])
            Error::InvalidMark => write!(f, "invalid mark"),
            // Java's `NumberFormatException.getMessage()` is the message
            Error::NumberFormat(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// `true` for the errors which correspond to jackcess'
    /// `ConstraintViolationException`.
    ///
    /// A [`Error::BatchUpdate`] is inspected through its cause as well: the
    /// batch exception is a wrapper, and the constraint is still the reason the
    /// write was refused.
    pub fn is_constraint_violation(&self) -> bool {
        match self {
            Error::ConstraintViolation(_) => true,
            Error::BatchUpdate { cause, .. } => cause.is_constraint_violation(),
            _ => false,
        }
    }

    /// `true` for the errors which correspond to a plain `java.io.IOException`
    /// (an I/O failure, not a `JackcessException`).  This is
    /// `TableImpl.isWriteFailure`'s test, which decides whether a failed batch
    /// write knows the state of any of its rows.
    pub fn is_write_failure(&self) -> bool {
        match self {
            Error::Io(_) => true,
            Error::BatchUpdate { cause, .. } => cause.is_write_failure(),
            _ => false,
        }
    }

    /// `true` for a [`Error::BatchUpdate`].
    pub fn is_batch_update(&self) -> bool {
        matches!(self, Error::BatchUpdate { .. })
    }

    /// Java's `BatchUpdateException.getUpdateCount()`, or `None` for any other
    /// error.
    pub fn update_count(&self) -> Option<i32> {
        match self {
            Error::BatchUpdate { update_count, .. } => Some(*update_count),
            _ => None,
        }
    }

    /// The underlying failure a [`Error::BatchUpdate`] wraps (Java's
    /// `Throwable.getCause()`), or `None`.
    pub fn batch_cause(&self) -> Option<&Error> {
        match self {
            Error::BatchUpdate { cause, .. } => Some(cause),
            _ => None,
        }
    }

    /// `new BatchUpdateException(updateCount, msg, cause)`: the message is
    /// Java's `msg + ": " + cause` (`cause` being `Throwable.toString()`).
    pub fn batch_update(update_count: i32, msg: impl fmt::Display, cause: Error) -> Error {
        let message = format!("{}: {}", msg, cause.java_to_string());
        Error::BatchUpdate {
            update_count,
            message,
            cause: Box::new(cause),
        }
    }

    /// Java's `Throwable.toString()`: the exception class name, then
    /// `": " + getMessage()` when the exception has a message.
    ///
    /// The Java NIO exceptions (`BufferUnderflowException`,
    /// `BufferOverflowException`, `NonWritableChannelException`) carry no
    /// message, so their text is the bare class name (the port keeps a
    /// diagnostic text in those variants, which is not part of Java's text).
    pub fn java_to_string(&self) -> String {
        match self {
            Error::Io(e) => {
                if e.to_string().contains("NonWritableChannelException") {
                    "java.nio.channels.NonWritableChannelException".to_string()
                } else {
                    format!("java.io.IOException: {e}")
                }
            }
            Error::Jackcess(m) => format!("java.io.IOException: {m}"),
            Error::IllegalArgument(m) => format!("java.lang.IllegalArgumentException: {m}"),
            Error::IllegalState(m) => format!("java.lang.IllegalStateException: {m}"),
            Error::InvalidCredentials(m) => {
                format!("com.healthmarketscience.jackcess.crypt.InvalidCredentialsException: {m}")
            }
            Error::Unsupported(m) => format!("java.lang.UnsupportedOperationException: {m}"),
            Error::ConstraintViolation(m) => {
                format!("com.healthmarketscience.jackcess.ConstraintViolationException: {m}")
            }
            Error::InvalidValue(m) => {
                format!("com.healthmarketscience.jackcess.InvalidValueException: {m}")
            }
            Error::BatchUpdate { message, .. } => {
                format!("com.healthmarketscience.jackcess.BatchUpdateException: {message}")
            }
            Error::BufferUnderflow(_) => "java.nio.BufferUnderflowException".to_string(),
            Error::BufferOverflow(_) => "java.nio.BufferOverflowException".to_string(),
            // like the other java.nio exceptions it has no message, so the bare
            // class name is Java's `Throwable.toString()` (the trace rendering
            // adds the `null` message)
            Error::InvalidMark => "java.nio.InvalidMarkException".to_string(),
            Error::NumberFormat(m) => format!("java.lang.NumberFormatException: {m}"),
        }
    }

    /// Wraps an error in a Jackcess error with the given operation context,
    /// leaving the error types which jackcess reports unchanged (a
    /// `ConstraintViolationException`, an `InvalidValueException`, a
    /// `BatchUpdateException`, an invalid-credentials error, and the Java
    /// runtime exceptions the port models: `java.nio.InvalidMarkException` and
    /// `NumberFormatException`, which Java never decorates with an operation
    /// context).
    pub fn with_context(self, context: impl std::fmt::Display) -> Error {
        match self {
            Error::ConstraintViolation(m) => Error::ConstraintViolation(m),
            Error::InvalidValue(m) => Error::InvalidValue(m),
            Error::InvalidCredentials(m) => Error::InvalidCredentials(m),
            Error::BatchUpdate { .. } => self,
            Error::InvalidMark => Error::InvalidMark,
            Error::NumberFormat(m) => Error::NumberFormat(m),
            other => Error::Jackcess(format!("{}: {}", context, other)),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(e: std::string::FromUtf8Error) -> Self {
        Error::Jackcess(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Convenience for `Error::Jackcess(...)`.
pub fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Jackcess(msg.into()))
}

/// Convenience for `Error::IllegalArgument(...)`.
pub fn illegal_arg<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::IllegalArgument(msg.into()))
}

/// Convenience for `Error::IllegalState(...)`.
pub fn illegal_state<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::IllegalState(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BatchUpdateException(int updateCount, String msg, Throwable cause)`:
    /// Java's `getMessage()` is `msg + ": " + cause` (the cause rendered as
    /// `Throwable.toString()`), `getUpdateCount()` is the count and the cause
    /// stays the root failure.
    #[test]
    fn batch_update_message_and_count_match_java() {
        let cause = Error::ConstraintViolation(
            "Adding new row [1] violates constraint X (Db=a.mdb;Table=T)".to_string(),
        );
        let error = Error::batch_update(2, "Failed adding rows (Db=a.mdb;Table=T)", cause);
        assert!(error.is_batch_update());
        assert_eq!(error.update_count(), Some(2));
        assert_eq!(
            error.to_string(),
            "Failed adding rows (Db=a.mdb;Table=T): \
             com.healthmarketscience.jackcess.ConstraintViolationException: \
             Adding new row [1] violates constraint X (Db=a.mdb;Table=T)"
        );
        assert_eq!(
            error.java_to_string(),
            format!(
                "com.healthmarketscience.jackcess.BatchUpdateException: {}",
                error
            )
        );
        // a constraint violation stays recognisable through the wrapper
        assert!(error.is_constraint_violation());
        assert!(!error.is_write_failure());
        // `with_context` does not wrap the batch exception again
        let message = error.to_string();
        let unchanged = error.with_context("outer");
        assert!(unchanged.is_batch_update());
        assert_eq!(unchanged.to_string(), message);
    }

    /// A `NonWritableChannelException` cause is Java's bare class name (its
    /// message is null) and counts as a write failure.
    #[test]
    fn batch_update_with_a_write_failure() {
        let cause = Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "NonWritableChannelException",
        ));
        assert!(cause.is_write_failure());
        assert_eq!(
            cause.java_to_string(),
            "java.nio.channels.NonWritableChannelException"
        );
        let error = Error::batch_update(0, "Failed adding rows (Db=a.mdb;Table=T)", cause);
        assert_eq!(
            error.to_string(),
            "Failed adding rows (Db=a.mdb;Table=T): \
             java.nio.channels.NonWritableChannelException"
        );
        assert_eq!(error.update_count(), Some(0));
        assert!(error.is_write_failure());
        // the root cause is still reachable
        assert_eq!(
            error.batch_cause().unwrap().java_to_string(),
            "java.nio.channels.NonWritableChannelException"
        );
    }
}
