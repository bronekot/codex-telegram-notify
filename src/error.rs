use std::fmt;

#[derive(Debug)]
pub enum AppError {
    Config(String),
    Payload(String),
    Telegram(String),
    Cancelled(String),
}

impl AppError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) | Self::Cancelled(_) => 1,
            Self::Payload(_) => 2,
            Self::Telegram(_) => 3,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message)
            | Self::Payload(message)
            | Self::Telegram(message)
            | Self::Cancelled(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AppError {}
