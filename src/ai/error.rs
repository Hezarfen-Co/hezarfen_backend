use crate::ai::protocol::FrameError;

/// Why an AI dispatch failed.
///
/// The split that matters is *whose* fault it is, because that decides whether
/// a retry can help: [`AiError::NoWorker`] and [`AiError::Timeout`] are ours to
/// wait out, [`AiError::Remote`] is the service's considered answer and will
/// repeat, and [`AiError::Transport`] / [`AiError::Protocol`] mean the
/// connection or the peer's framing is broken.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// No connected service declared this capability. Either none has dialled
    /// in yet, or every one of them dropped.
    #[error("no AI service registered for capability `{0}`")]
    NoWorker(String),
    /// Every registered worker for the capability is already at its declared
    /// `max_concurrent`. Distinct from [`AiError::NoWorker`]: the capacity is
    /// there, it is just busy right now, so a retry is worth making.
    #[error("all AI services for capability `{0}` are at capacity")]
    Busy(String),
    /// The service did not answer within the deadline. The request may still
    /// be executing on the far side — treat as "unknown", not "did not run".
    #[error("AI service did not answer within {0}ms")]
    Timeout(u64),
    /// The service answered with a handled failure of its own.
    #[error("AI service refused the request: {code}: {message}")]
    Remote { code: String, message: String },
    /// The QUIC connection or stream failed.
    #[error("AI transport failed: {0}")]
    Transport(String),
    /// The peer's bytes did not form a frame we could read.
    #[error("AI protocol violation: {0}")]
    Protocol(#[source] FrameError),
    /// The answer on this stream carried a different trace id than the request
    /// written to it. Correlation does not depend on the id — the stream does
    /// that — but a mismatch means the service has mixed up whose work this
    /// is, so the payload cannot be trusted as this request's result.
    #[error("AI service answered request {expected} with {got}")]
    IdMismatch { expected: String, got: String },
    /// The bridge could not be started at all (bind, certificate, config).
    #[error("AI bridge setup failed: {0}")]
    Setup(String),
}

impl From<FrameError> for AiError {
    fn from(e: FrameError) -> Self {
        AiError::Protocol(e)
    }
}

impl AiError {
    /// Is waiting and trying again plausibly useful? Used by callers that sit
    /// on an HTTP path to decide between a 503 (come back) and a 502 (this
    /// request is not going to work).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AiError::NoWorker(_) | AiError::Busy(_) | AiError::Transport(_)
        )
    }
}
