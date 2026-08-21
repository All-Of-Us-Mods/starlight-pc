use std::fmt::{Display, Formatter};

pub type AppResult<T> = Result<T, AppError>;

/// A backend failure carrying a user-presentable message. No structured
/// variants — nothing in the app matches on error kind, they only display it.
#[derive(Debug)]
pub struct AppError(pub String);

impl AppError {
    pub fn validation(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn process(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn platform(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn state(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Display for AppError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self(format!("I/O error: {value}"))
    }
}

impl From<reqwest::Error> for AppError {
    fn from(value: reqwest::Error) -> Self {
        Self(format!("HTTP error: {value}"))
    }
}

impl From<zip::result::ZipError> for AppError {
    fn from(value: zip::result::ZipError) -> Self {
        Self(format!("Zip error: {value}"))
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self(format!("Parse error: {value}"))
    }
}

#[cfg(test)]
mod tests {
    use super::AppError;

    #[test]
    fn display_smoke() {
        let err = AppError::validation("invalid");
        assert_eq!(err.to_string(), "invalid");
    }
}
