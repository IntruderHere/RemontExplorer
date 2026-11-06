use std::fmt;

use crate::i18n::{fill, global, tr};

/// Ошибки операций ядра.
#[derive(Debug)]
pub enum CoreError {
    /// Операция недоступна в этой версии.
    #[allow(dead_code)]
    NotPorted(&'static str),
    /// Несуществующий или недопустимый аргумент-путь.
    InvalidPath(String),
    Io(std::io::Error),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lang = global();
        match self {
            CoreError::NotPorted(op) => {
                f.write_str(&fill(tr(lang, "err_not_ported"), &[op.to_string()]))
            }
            CoreError::InvalidPath(p) => {
                f.write_str(&fill(tr(lang, "err_invalid_path"), &[p.clone()]))
            }
            CoreError::Io(e) => {
                f.write_str(&fill(tr(lang, "err_io"), &[e.to_string()]))
            }
        }
    }
}

impl std::error::Error for CoreError {}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e)
    }
}
