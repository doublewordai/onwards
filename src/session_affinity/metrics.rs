use metrics::counter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAffinityResult {
    Hit,
    Miss,
    Stale,
    Bound,
    Unavailable,
    Error,
    Rebound,
    FallbackNoRebind,
}

impl SessionAffinityResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Stale => "stale",
            Self::Bound => "bound",
            Self::Unavailable => "unavailable",
            Self::Error => "error",
            Self::Rebound => "rebound",
            Self::FallbackNoRebind => "fallback_no_rebind",
        }
    }
}

pub fn record_result(model: &str, result: SessionAffinityResult) {
    counter!(
        "session_affinity_total",
        "model" => model.to_string(),
        "result" => result.as_str()
    )
    .increment(1);
}

pub fn record_cleanup(model: &str, reason: &'static str, removed: usize) {
    if removed == 0 {
        return;
    }

    counter!(
        "session_affinity_cleanup_total",
        "model" => model.to_string(),
        "reason" => reason
    )
    .increment(removed as u64);
}
