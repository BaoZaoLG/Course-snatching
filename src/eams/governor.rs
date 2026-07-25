use super::types::{BackendErrorKind, CircuitStatus, NetworkSnapshot, RetryAdvice};
use parking_lot::Mutex;
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestPriority {
    Submission,
    Session,
    Refresh,
    KeepAlive,
}

impl RequestPriority {
    fn rank(self) -> u8 {
        match self {
            Self::Submission => 0,
            Self::Session => 1,
            Self::Refresh => 2,
            Self::KeepAlive => 3,
        }
    }
}

#[derive(Debug, Clone)]
struct GovernorPolicy {
    normal_requests_per_second: f64,
    burst_requests_per_second: f64,
    capacity: f64,
    rate_limit_window: Duration,
    rate_limit_threshold: usize,
    circuit_cooldown: Duration,
    retry_min: Duration,
    retry_max: Duration,
    jitter_fraction: f64,
}

impl Default for GovernorPolicy {
    fn default() -> Self {
        Self {
            normal_requests_per_second: 6.0,
            burst_requests_per_second: 10.0,
            capacity: 4.0,
            rate_limit_window: Duration::from_secs(120),
            rate_limit_threshold: 3,
            circuit_cooldown: Duration::from_secs(60),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(120),
            jitter_fraction: 0.1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Waiter {
    id: u64,
    sequence: u64,
    priority: RequestPriority,
}

#[derive(Debug, Clone, Copy)]
enum Circuit {
    Closed,
    Open { until: Instant },
    HalfOpen { probe_id: Option<u64> },
}

#[derive(Debug)]
struct GovernorState {
    tokens: f64,
    last_refill: Instant,
    burst_mode: bool,
    next_id: u64,
    next_sequence: u64,
    waiters: Vec<Waiter>,
    cooldown_until: Option<Instant>,
    circuit: Circuit,
    rate_limit_events: VecDeque<Instant>,
    request_events: VecDeque<Instant>,
    first_request_at: Option<Instant>,
    latency_ewma_ms: Option<f64>,
    total_rate_limits: u64,
    consecutive_errors: u32,
    last_error_kind: Option<BackendErrorKind>,
}

pub(crate) struct RequestGovernor {
    policy: GovernorPolicy,
    state: Mutex<GovernorState>,
    notify: Notify,
    refresh_gate: Arc<Semaphore>,
    submission_gate: Arc<Semaphore>,
}

#[derive(Debug)]
pub(crate) struct RequestPermit {
    id: u64,
    half_open_probe: bool,
    started_at: Instant,
    governor: Weak<RequestGovernor>,
    resolved: Cell<bool>,
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        if self.resolved.get() || !self.half_open_probe {
            return;
        }
        let Some(governor) = self.governor.upgrade() else {
            return;
        };
        let now = Instant::now();
        let mut state = governor.state.lock();
        if matches!(
            state.circuit,
            Circuit::HalfOpen { probe_id: Some(id) } if id == self.id
        ) {
            let until = now + governor.policy.circuit_cooldown;
            state.circuit = Circuit::Open { until };
            state.cooldown_until = Some(until);
            state.burst_mode = false;
        }
        drop(state);
        governor.notify.notify_waiters();
    }
}

struct WaitRegistration {
    governor: Arc<RequestGovernor>,
    id: u64,
    active: bool,
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.governor
            .state
            .lock()
            .waiters
            .retain(|waiter| waiter.id != self.id);
        self.governor.notify.notify_waiters();
    }
}

impl Default for RequestGovernor {
    fn default() -> Self {
        Self::with_policy(GovernorPolicy::default())
    }
}

impl RequestGovernor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn with_policy(policy: GovernorPolicy) -> Self {
        let now = Instant::now();
        Self {
            state: Mutex::new(GovernorState {
                tokens: policy.capacity,
                last_refill: now,
                burst_mode: false,
                next_id: 1,
                next_sequence: 1,
                waiters: Vec::new(),
                cooldown_until: None,
                circuit: Circuit::Closed,
                rate_limit_events: VecDeque::new(),
                request_events: VecDeque::new(),
                first_request_at: None,
                latency_ewma_ms: None,
                total_rate_limits: 0,
                consecutive_errors: 0,
                last_error_kind: None,
            }),
            policy,
            notify: Notify::new(),
            refresh_gate: Arc::new(Semaphore::new(1)),
            submission_gate: Arc::new(Semaphore::new(1)),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_semantic_response_tests() -> Arc<Self> {
        Arc::new(Self::with_policy(GovernorPolicy {
            normal_requests_per_second: 1_000.0,
            burst_requests_per_second: 1_000.0,
            capacity: 16.0,
            rate_limit_window: Duration::from_secs(10),
            rate_limit_threshold: 3,
            circuit_cooldown: Duration::from_millis(45),
            retry_min: Duration::from_millis(1),
            retry_max: Duration::from_millis(5),
            jitter_fraction: 0.0,
        }))
    }

    pub fn set_burst_mode(&self, enabled: bool) {
        let now = Instant::now();
        let mut state = self.state.lock();
        self.refill(&mut state, now);
        state.burst_mode = enabled && matches!(state.circuit, Circuit::Closed);
        drop(state);
        self.notify.notify_waiters();
    }

    pub fn circuit_is_open(&self) -> bool {
        !matches!(self.snapshot().circuit_status, CircuitStatus::Closed)
    }

    pub(crate) async fn enter_refresh(self: &Arc<Self>) -> OwnedSemaphorePermit {
        self.refresh_gate
            .clone()
            .acquire_owned()
            .await
            .expect("refresh semaphore must remain open")
    }

    /// Keepalive work is opportunistic. If a foreground refresh already owns
    /// the full-refresh gate, keepalive must skip instead of joining the FIFO
    /// semaphore queue ahead of a later foreground refresh.
    pub(crate) fn try_enter_refresh(self: &Arc<Self>) -> Option<OwnedSemaphorePermit> {
        self.refresh_gate.clone().try_acquire_owned().ok()
    }

    pub(crate) async fn enter_submission(self: &Arc<Self>) -> OwnedSemaphorePermit {
        self.submission_gate
            .clone()
            .acquire_owned()
            .await
            .expect("submission semaphore must remain open")
    }

    pub(crate) async fn acquire(self: &Arc<Self>, priority: RequestPriority) -> RequestPermit {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_id;
            state.next_id = state.next_id.wrapping_add(1);
            let sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.wrapping_add(1);
            state.waiters.push(Waiter {
                id,
                sequence,
                priority,
            });
            id
        };
        let mut registration = WaitRegistration {
            governor: self.clone(),
            id,
            active: true,
        };
        self.notify.notify_waiters();

        loop {
            let now = Instant::now();
            let wait_for = {
                let mut state = self.state.lock();
                self.refill(&mut state, now);
                self.advance_circuit(&mut state, now);

                let top = state
                    .waiters
                    .iter()
                    .min_by_key(|waiter| (waiter.priority.rank(), waiter.sequence))
                    .map(|waiter| waiter.id);
                if top != Some(id) {
                    Some(Duration::from_millis(25))
                } else if let Some(until) = self.blocked_until(&state, now) {
                    Some(until.saturating_duration_since(now))
                } else if matches!(state.circuit, Circuit::HalfOpen { probe_id: Some(_) }) {
                    Some(Duration::from_millis(25))
                } else if state.tokens < 1.0 {
                    let rate = self.active_rate(&state);
                    Some(Duration::from_secs_f64((1.0 - state.tokens) / rate))
                } else {
                    state.tokens -= 1.0;
                    state.waiters.retain(|waiter| waiter.id != id);
                    let half_open_probe = if let Circuit::HalfOpen { probe_id } = &mut state.circuit
                    {
                        *probe_id = Some(id);
                        true
                    } else {
                        false
                    };
                    state.request_events.push_back(now);
                    state.first_request_at.get_or_insert(now);
                    self.prune_request_events(&mut state, now);
                    registration.active = false;
                    return RequestPermit {
                        id,
                        half_open_probe,
                        started_at: now,
                        governor: Arc::downgrade(self),
                        resolved: Cell::new(false),
                    };
                }
            };

            let delay = wait_for
                .unwrap_or_else(|| Duration::from_millis(25))
                .max(Duration::from_millis(1));
            let _ = tokio::time::timeout(delay, self.notify.notified()).await;
        }
    }

    pub(crate) fn record_success(&self, permit: &RequestPermit) {
        permit.resolved.set(true);
        let now = Instant::now();
        let latency_ms = now.duration_since(permit.started_at).as_secs_f64() * 1000.0;
        let mut state = self.state.lock();
        state.latency_ewma_ms = Some(match state.latency_ewma_ms {
            Some(previous) => previous * 0.8 + latency_ms * 0.2,
            None => latency_ms,
        });
        state.consecutive_errors = 0;
        if permit.half_open_probe
            && matches!(
                state.circuit,
                Circuit::HalfOpen { probe_id: Some(id) } if id == permit.id
            )
        {
            state.circuit = Circuit::Closed;
            state.rate_limit_events.clear();
            state.cooldown_until = None;
        }
        drop(state);
        self.notify.notify_waiters();
    }

    pub(crate) fn record_failure(
        &self,
        permit: &RequestPermit,
        kind: BackendErrorKind,
        retry_after: Option<Duration>,
    ) -> RetryAdvice {
        permit.resolved.set(true);
        let now = Instant::now();
        let latency_ms = now.duration_since(permit.started_at).as_secs_f64() * 1000.0;
        let mut state = self.state.lock();
        state.latency_ewma_ms = Some(match state.latency_ewma_ms {
            Some(previous) => previous * 0.8 + latency_ms * 0.2,
            None => latency_ms,
        });
        // 只有会影响连接稳定性的失败才进入连续网络错误计数；业务结果、
        // 登录失效和解析异常不能触发网络熔断，也不能把旧的网络失败
        // 误报为仍在连续发生。
        if kind.needs_backoff() {
            state.consecutive_errors = state.consecutive_errors.saturating_add(1);
        } else {
            state.consecutive_errors = 0;
        }
        state.last_error_kind = Some(kind);

        if kind == BackendErrorKind::RateLimited {
            state.total_rate_limits = state.total_rate_limits.saturating_add(1);
            state.rate_limit_events.push_back(now);
            while state
                .rate_limit_events
                .front()
                .is_some_and(|event| now.duration_since(*event) > self.policy.rate_limit_window)
            {
                state.rate_limit_events.pop_front();
            }
        }

        let half_open_failed = permit.half_open_probe
            && matches!(
                state.circuit,
                Circuit::HalfOpen { probe_id: Some(id) } if id == permit.id
            );
        let threshold_reached = kind == BackendErrorKind::RateLimited
            && state.rate_limit_events.len() >= self.policy.rate_limit_threshold;

        let retry_delay = if kind.needs_backoff() {
            let base = if let Some(server_delay) = retry_after {
                server_delay.clamp(self.policy.retry_min, self.policy.retry_max)
            } else {
                retry_delay_for_failure(state.consecutive_errors)
            };
            Some(if retry_after.is_some() {
                add_positive_jitter(base, self.policy.jitter_fraction)
            } else {
                base
            })
        } else {
            None
        };

        if half_open_failed || threshold_reached {
            // A circuit trip must never shorten a server-requested Retry-After.
            // Half-open failures also restart at least the full circuit cooldown.
            let cooldown = if retry_after.is_some() {
                retry_delay
                    .expect("typed Retry-After failures always need backoff")
                    .max(self.policy.circuit_cooldown)
            } else {
                self.policy.circuit_cooldown
            };
            let until = now + cooldown;
            state.circuit = Circuit::Open { until };
            state.cooldown_until = Some(until);
            state.burst_mode = false;
            drop(state);
            self.notify.notify_waiters();
            return RetryAdvice::CircuitOpen(cooldown);
        }

        let advice = if let Some(delay) = retry_delay {
            let until = now + delay;
            state.cooldown_until = Some(
                state
                    .cooldown_until
                    .map_or(until, |current| current.max(until)),
            );
            RetryAdvice::Cooldown(delay)
        } else {
            RetryAdvice::None
        };
        drop(state);
        self.notify.notify_waiters();
        advice
    }

    pub fn snapshot(&self) -> NetworkSnapshot {
        let now = Instant::now();
        let mut state = self.state.lock();
        self.advance_circuit(&mut state, now);
        self.prune_request_events(&mut state, now);
        let denominator = state
            .first_request_at
            .map(|first| now.duration_since(first).as_secs_f64().clamp(1.0, 60.0))
            .unwrap_or(60.0);
        let cooldown_until = self.blocked_until(&state, now);
        NetworkSnapshot {
            requests_per_second: state.request_events.len() as f64 / denominator,
            latency_ewma_ms: state.latency_ewma_ms,
            total_rate_limits: state.total_rate_limits,
            consecutive_errors: state.consecutive_errors,
            cooldown_remaining: cooldown_until
                .map(|until| until.saturating_duration_since(now))
                .unwrap_or(Duration::ZERO),
            last_error_kind: state.last_error_kind,
            circuit_status: match state.circuit {
                Circuit::Closed => CircuitStatus::Closed,
                Circuit::Open { .. } => CircuitStatus::Open,
                Circuit::HalfOpen { .. } => CircuitStatus::HalfOpen,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_cooldown_for_tests(&self) {
        self.state.lock().cooldown_until = None;
        self.notify.notify_waiters();
    }

    fn active_rate(&self, state: &GovernorState) -> f64 {
        if state.burst_mode && matches!(state.circuit, Circuit::Closed) {
            self.policy.burst_requests_per_second
        } else {
            self.policy.normal_requests_per_second
        }
    }

    fn refill(&self, state: &mut GovernorState, now: Instant) {
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();
        state.tokens = (state.tokens + elapsed * self.active_rate(state)).min(self.policy.capacity);
        state.last_refill = now;
    }

    fn advance_circuit(&self, state: &mut GovernorState, now: Instant) {
        if matches!(state.circuit, Circuit::Open { until } if now >= until) {
            state.circuit = Circuit::HalfOpen { probe_id: None };
            state.cooldown_until = None;
        }
    }

    fn blocked_until(&self, state: &GovernorState, now: Instant) -> Option<Instant> {
        let cooldown = state.cooldown_until.filter(|until| *until > now);
        let circuit = match state.circuit {
            Circuit::Open { until } if until > now => Some(until),
            _ => None,
        };
        match (cooldown, circuit) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (Some(until), None) | (None, Some(until)) => Some(until),
            (None, None) => None,
        }
    }

    fn prune_request_events(&self, state: &mut GovernorState, now: Instant) {
        while state
            .request_events
            .front()
            .is_some_and(|event| now.duration_since(*event) > Duration::from_secs(60))
        {
            state.request_events.pop_front();
        }
    }
}

/// 无服务器 Retry-After 时的固定退避序列。最后一项会持续使用，避免
/// 连续失败后把恢复时间无界拉长。
fn retry_delay_for_failure(consecutive_errors: u32) -> Duration {
    const RETRY_DELAYS: [Duration; 5] = [
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(16),
        Duration::from_secs(30),
    ];
    let index = consecutive_errors.saturating_sub(1).min(4) as usize;
    RETRY_DELAYS[index]
}

fn add_positive_jitter(duration: Duration, fraction: f64) -> Duration {
    if fraction <= 0.0 {
        return duration;
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let unit = f64::from(nanos % 10_001) / 10_000.0;
    duration.mul_f64(1.0 + unit * fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> GovernorPolicy {
        GovernorPolicy {
            normal_requests_per_second: 50.0,
            burst_requests_per_second: 100.0,
            capacity: 1.0,
            rate_limit_window: Duration::from_millis(250),
            rate_limit_threshold: 3,
            circuit_cooldown: Duration::from_millis(45),
            retry_min: Duration::from_millis(12),
            retry_max: Duration::from_millis(100),
            jitter_fraction: 0.0,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn token_bucket_waits_for_refill() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let first = governor.acquire(RequestPriority::Refresh).await;
            governor.record_success(&first);
            let started = Instant::now();
            let second = governor.acquire(RequestPriority::Refresh).await;
            assert!(started.elapsed() >= Duration::from_millis(15));
            governor.record_success(&second);
        });
    }

    #[test]
    fn higher_priority_jumps_the_waiting_queue() {
        runtime().block_on(async {
            let mut policy = test_policy();
            // Keep enough time between the first empty token bucket and its
            // first refill to deterministically place both requests in queue.
            policy.normal_requests_per_second = 10.0;
            policy.burst_requests_per_second = 10.0;
            let governor = Arc::new(RequestGovernor::with_policy(policy));
            let initial = governor.acquire(RequestPriority::Refresh).await;
            governor.record_success(&initial);
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let low = governor.clone();
            let low_sender = sender.clone();
            tokio::spawn(async move {
                let permit = low.acquire(RequestPriority::KeepAlive).await;
                low_sender.send(RequestPriority::KeepAlive).unwrap();
                low.record_success(&permit);
            });
            wait_for_waiters(&governor, 1).await;
            let high = governor.clone();
            tokio::spawn(async move {
                let permit = high.acquire(RequestPriority::Submission).await;
                sender.send(RequestPriority::Submission).unwrap();
                high.record_success(&permit);
            });
            wait_for_waiters(&governor, 2).await;
            assert_eq!(receiver.recv().await, Some(RequestPriority::Submission));
            assert_eq!(receiver.recv().await, Some(RequestPriority::KeepAlive));
        });
    }

    async fn wait_for_waiters(governor: &Arc<RequestGovernor>, expected: usize) {
        for _ in 0..50 {
            if governor.state.lock().waiters.len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("request was not queued in time");
    }

    #[test]
    fn retry_after_creates_non_bypassable_cooldown() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let permit = governor.acquire(RequestPriority::Refresh).await;
            let advice = governor.record_failure(
                &permit,
                BackendErrorKind::RateLimited,
                Some(Duration::from_millis(30)),
            );
            assert_eq!(advice, RetryAdvice::Cooldown(Duration::from_millis(30)));
            let started = Instant::now();
            let next = governor.acquire(RequestPriority::Submission).await;
            assert!(started.elapsed() >= Duration::from_millis(25));
            governor.record_success(&next);
        });
    }

    #[test]
    fn three_rate_limits_open_circuit_and_successful_probe_closes_it() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            for attempt in 0..3 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                let advice = governor.record_failure(
                    &permit,
                    BackendErrorKind::RateLimited,
                    Some(Duration::from_millis(12)),
                );
                if attempt == 2 {
                    assert!(matches!(advice, RetryAdvice::CircuitOpen(_)));
                }
            }
            assert_eq!(governor.snapshot().circuit_status, CircuitStatus::Open);
            let probe = governor.acquire(RequestPriority::Session).await;
            assert!(probe.half_open_probe);
            assert_eq!(governor.snapshot().circuit_status, CircuitStatus::HalfOpen);
            governor.record_success(&probe);
            assert_eq!(governor.snapshot().circuit_status, CircuitStatus::Closed);
        });
    }

    #[test]
    fn non_network_errors_do_not_accumulate_network_failure_count() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let network_failure = governor.acquire(RequestPriority::Refresh).await;
            governor.record_failure(&network_failure, BackendErrorKind::Transport, None);
            assert_eq!(governor.snapshot().consecutive_errors, 1);

            let business_failure = governor.acquire(RequestPriority::Refresh).await;
            governor.record_failure(&business_failure, BackendErrorKind::Business, None);
            let snapshot = governor.snapshot();
            assert_eq!(snapshot.consecutive_errors, 0);
            assert_eq!(snapshot.last_error_kind, Some(BackendErrorKind::Business));
            assert_eq!(snapshot.circuit_status, CircuitStatus::Closed);
        });
    }

    #[test]
    fn retry_schedule_matches_the_documented_sequence() {
        assert_eq!(retry_delay_for_failure(1), Duration::from_secs(2));
        assert_eq!(retry_delay_for_failure(2), Duration::from_secs(4));
        assert_eq!(retry_delay_for_failure(3), Duration::from_secs(8));
        assert_eq!(retry_delay_for_failure(4), Duration::from_secs(16));
        assert_eq!(retry_delay_for_failure(5), Duration::from_secs(30));
        assert_eq!(retry_delay_for_failure(99), Duration::from_secs(30));
    }

    #[test]
    fn half_open_allows_only_one_probe_at_a_time() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            for _ in 0..3 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                governor.record_failure(
                    &permit,
                    BackendErrorKind::RateLimited,
                    Some(Duration::from_millis(12)),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            let probe = governor.acquire(RequestPriority::Session).await;
            assert!(probe.half_open_probe);
            let blocked = tokio::time::timeout(
                Duration::from_millis(20),
                governor.acquire(RequestPriority::Submission),
            )
            .await;
            assert!(blocked.is_err(), "a second half-open probe must not pass");

            governor.record_success(&probe);
            let next = governor.acquire(RequestPriority::Submission).await;
            governor.record_success(&next);
        });
    }

    #[test]
    fn failed_half_open_probe_restarts_the_full_cooldown() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            for _ in 0..3 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                governor.record_failure(
                    &permit,
                    BackendErrorKind::RateLimited,
                    Some(Duration::from_millis(12)),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            let probe = governor.acquire(RequestPriority::Session).await;
            assert!(probe.half_open_probe);
            let advice = governor.record_failure(&probe, BackendErrorKind::Transport, None);
            assert_eq!(advice, RetryAdvice::CircuitOpen(Duration::from_millis(45)));
            let snapshot = governor.snapshot();
            assert_eq!(snapshot.circuit_status, CircuitStatus::Open);
            assert!(snapshot.cooldown_remaining >= Duration::from_millis(35));
        });
    }

    #[test]
    fn circuit_trip_never_shortens_a_longer_retry_after() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            for _ in 0..2 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                governor.record_failure(
                    &permit,
                    BackendErrorKind::RateLimited,
                    Some(Duration::from_millis(12)),
                );
            }
            let third = governor.acquire(RequestPriority::Refresh).await;
            let advice = governor.record_failure(
                &third,
                BackendErrorKind::RateLimited,
                Some(Duration::from_millis(90)),
            );
            assert_eq!(advice, RetryAdvice::CircuitOpen(Duration::from_millis(90)));
            let snapshot = governor.snapshot();
            assert_eq!(snapshot.circuit_status, CircuitStatus::Open);
            assert!(snapshot.cooldown_remaining >= Duration::from_millis(80));
        });
    }

    #[test]
    fn refresh_and_submission_gates_are_independent_but_each_is_exclusive() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let refresh = governor.enter_refresh().await;
            let submission = governor.enter_submission().await;

            let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel(1);
            let refresh_governor = governor.clone();
            tokio::spawn(async move {
                let _permit = refresh_governor.enter_refresh().await;
                refresh_tx.send(()).await.unwrap();
            });
            let (submission_tx, mut submission_rx) = tokio::sync::mpsc::channel(1);
            let submission_governor = governor.clone();
            tokio::spawn(async move {
                let _permit = submission_governor.enter_submission().await;
                submission_tx.send(()).await.unwrap();
            });

            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(refresh_rx.try_recv().is_err());
            assert!(submission_rx.try_recv().is_err());

            // Releasing a refresh must not release a waiting submission. The two
            // single-permit gates can therefore protect one full refresh and one
            // complete submission at the same time.
            drop(refresh);
            refresh_rx.recv().await.unwrap();
            assert!(submission_rx.try_recv().is_err());
            drop(submission);
            submission_rx.recv().await.unwrap();
        });
    }

    #[test]
    fn keepalive_refresh_gate_is_opportunistic_and_never_queues() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let active_refresh = governor.enter_refresh().await;
            assert!(governor.try_enter_refresh().is_none());

            let foreground = governor.clone();
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
                let _permit = foreground.enter_refresh().await;
                sender.send(()).await.unwrap();
            });
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(receiver.try_recv().is_err());

            drop(active_refresh);
            receiver.recv().await.unwrap();
        });
    }

    #[test]
    fn any_failed_half_open_probe_restarts_the_cooldown() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            for _ in 0..3 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                governor.record_failure(
                    &permit,
                    BackendErrorKind::RateLimited,
                    Some(Duration::from_millis(12)),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            let probe = governor.acquire(RequestPriority::Session).await;
            assert!(probe.half_open_probe);
            assert_eq!(
                governor.record_failure(&probe, BackendErrorKind::Business, None),
                RetryAdvice::CircuitOpen(Duration::from_millis(45))
            );
            let snapshot = governor.snapshot();
            assert_eq!(snapshot.circuit_status, CircuitStatus::Open);
            assert!(snapshot.cooldown_remaining >= Duration::from_millis(35));
        });
    }

    #[test]
    fn cancelled_half_open_probe_restarts_the_cooldown() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            for _ in 0..3 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                governor.record_failure(
                    &permit,
                    BackendErrorKind::RateLimited,
                    Some(Duration::from_millis(12)),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            let probe = governor.acquire(RequestPriority::Session).await;
            assert!(probe.half_open_probe);
            drop(probe);

            let snapshot = governor.snapshot();
            assert_eq!(snapshot.circuit_status, CircuitStatus::Open);
            assert!(snapshot.cooldown_remaining >= Duration::from_millis(35));
        });
    }

    #[test]
    fn production_budget_honors_normal_and_burst_steady_rates() {
        runtime().block_on(async {
            let governor = RequestGovernor::new();

            // Drain the documented instantaneous capacity first. The following
            // tokens must then arrive at no more than the steady 6 req/s budget.
            for _ in 0..4 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                governor.record_success(&permit);
            }
            let normal_started = Instant::now();
            for _ in 0..6 {
                let permit = governor.acquire(RequestPriority::Refresh).await;
                governor.record_success(&permit);
            }
            assert!(
                normal_started.elapsed() >= Duration::from_millis(900),
                "normal budget refilled faster than 6 req/s"
            );

            governor.set_burst_mode(true);
            // No tokens remain after the normal measurement. Ten more requests
            // therefore require roughly one second at the 10 req/s burst budget.
            let burst_started = Instant::now();
            for _ in 0..10 {
                let permit = governor.acquire(RequestPriority::Submission).await;
                governor.record_success(&permit);
            }
            assert!(
                burst_started.elapsed() >= Duration::from_millis(900),
                "burst budget refilled faster than 10 req/s"
            );
        });
    }
}
