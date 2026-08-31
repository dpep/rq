/// Seconds since the Unix epoch — the stamp every persisted timestamp, TTL and
/// recency decay in rq is measured in.
///
/// A clock that reads before the epoch is not a case worth propagating, so it
/// saturates at 0 rather than returning an error nobody could act on.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::now_unix;

    // Every TTL and recency decay is denominated in this, yet nothing else in
    // the suite notices a clock stuck at 0 or reading in milliseconds.
    #[test]
    fn reads_seconds_since_the_epoch() {
        let now = now_unix();
        assert!(now > 1_700_000_000, "{now} predates this code");
        assert!(now < 32_500_000_000, "{now} is not in seconds");
    }
}
