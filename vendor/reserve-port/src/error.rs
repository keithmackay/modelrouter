use ::thiserror::Error;

pub type Result<T> = ::std::result::Result<T, self::Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to find a free port to reserve")]
    FailedToReservePort,

    #[error("Failed to lock the global port finder")]
    InternalLockError,
}
