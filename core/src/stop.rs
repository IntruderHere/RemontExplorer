use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Сигнал остановки, общий для всех операций.
///
/// Семантика: стоп должен
/// возвращаться promptly — воркеры проверяют [`StopToken::is_set`] между
/// задачами и выходят; вызывающая сторона не сливает «висящие» задачи.
#[derive(Clone, Debug, Default)]
pub struct StopToken(Arc<AtomicBool>);

impl StopToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Запросить остановку (идемпотентно).
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Сбросить флаг перед началом новой операции.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
