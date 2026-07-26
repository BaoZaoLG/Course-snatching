use super::types::{BackendErrorKind, CircuitStatus, NetworkSnapshot, RetryAdvice};
use parking_lot::Mutex;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// 同时在途的选课提交数上限。
///
/// 单许可（原值）虽然过于保守，但它确实是当时唯一防止提交风暴的机制，
/// 所以放开时必须明确：这里放开的是「同时在途的提交数」，速率上限仍由
/// 令牌桶承担，熔断与 hard cooldown 在其下不可绕过（见 governor 的三条
/// 并发不变式测试）。取 3 是因为教务高峰 RTT 常在 300–800ms，3 路并发
/// 已能把「第 5 个目标晚 1.5–4 秒才发出」压到亚秒级，再高只是徒增被风控
/// 识别的概率。
const SUBMISSION_CONCURRENCY: usize = 3;

// 退回单许可等于把 G-03 的并发提交整个作废，编译期就挡住。
const _: () = assert!(
    SUBMISSION_CONCURRENCY > 1,
    "serial submission is the bug G-03 fixed"
);

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
            // 与解析层 parse_retry_after_secs 的 300s 上限保持一致：服务器
            // 明确要求的冷却绝不能被治理层截断后提前重试（易升级封禁）。
            retry_max: Duration::from_secs(300),
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
    /// 服务器明确要求降速（限流/Retry-After）或熔断产生的冷却：
    /// 对所有优先级不可绕过。
    hard_cooldown_until: Option<Instant>,
    /// 瞬态网络失败（超时/连接/5xx）的本地退避：Submission 豁免——
    /// 冲刺期一次超时不应挡住紧随其后的提交；限流与熔断仍然全局生效。
    soft_cooldown_until: Option<Instant>,
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
            state.hard_cooldown_until = Some(until);
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
    /// 治理状态必须跨 client 重建存活：每次登录、会话失效重登都会重建
    /// EamsClient，若限流冷却、熔断与 429 历史随旧实例丢弃，反复点击登录
    /// 即可绕过全部退避。因此按 origin 维护进程级共享 governor，同源的新
    /// client 继续沿用旧状态。
    pub(crate) fn shared_for_origin(origin: &str) -> Arc<Self> {
        static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<RequestGovernor>>>> = OnceLock::new();
        REGISTRY
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .entry(origin.to_string())
            .or_default()
            .clone()
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
                hard_cooldown_until: None,
                soft_cooldown_until: None,
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
            submission_gate: Arc::new(Semaphore::new(SUBMISSION_CONCURRENCY)),
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
                } else if let Some(until) = self.blocked_until(&state, now, priority) {
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
            state.hard_cooldown_until = None;
            state.soft_cooldown_until = None;
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

        // 半开探针只对「链路层面的失败」负责。
        //
        // 拿到干净的 HTTP 200、只是业务被拒（选课失败）或登录失效，恰恰证明
        // 链路是通的——把它们算作探针失败会白白重开 60s 熔断，正好卡在恢复
        // 的那一刻。
        let link_level_failure = !matches!(
            kind,
            BackendErrorKind::Business | BackendErrorKind::AuthExpired | BackendErrorKind::Parse
        );
        let is_current_probe = permit.half_open_probe
            && matches!(
                state.circuit,
                Circuit::HalfOpen { probe_id: Some(id) } if id == permit.id
            );
        let half_open_failed = is_current_probe && link_level_failure;
        if is_current_probe && !link_level_failure {
            // 探针必须有归宿，否则熔断会永久停在 HalfOpen{probe_id:Some}：
            // `resolved` 已置位所以 Drop 不再兜底，而 acquire 见到这个状态只会
            // 每 25ms 重试——全进程网络请求无限排队，只能重启程序。
            //
            // 归宿是「关闭」而不是「放回半开」：拿到干净的 HTTP 200、只是业务
            // 被拒，本来就证明链路是通的，与 record_success 同一判断。
            state.circuit = Circuit::Closed;
            state.rate_limit_events.clear();
            state.hard_cooldown_until = None;
            state.soft_cooldown_until = None;
        }
        let threshold_reached = kind == BackendErrorKind::RateLimited
            && state.rate_limit_events.len() >= self.policy.rate_limit_threshold;

        let retry_delay = if kind.needs_backoff() {
            Some(if let Some(server_delay) = retry_after {
                // 服务器下发的 Retry-After 各客户端本来就是错开的，最不需要
                // 抖动（原实现恰好只给它加了抖动）；也绝不能被截短。
                server_delay.clamp(self.policy.retry_min, self.policy.retry_max)
            } else {
                // 本地推测的阶梯自带 decorrelated jitter——它才是需要打散相位的
                // 那一个：所有客户端的 consecutive_errors 会同时变成 1。
                retry_delay_for_failure(state.consecutive_errors)
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
            state.hard_cooldown_until = Some(until);
            state.burst_mode = false;
            drop(state);
            self.notify.notify_waiters();
            return RetryAdvice::CircuitOpen(cooldown);
        }

        let advice = if let Some(delay) = retry_delay {
            let until = now + delay;
            // 限流冷却（服务器明确要求降速）全局不可绕过；其余瞬态网络
            // 失败只做本地退避，Submission 可豁免（见 blocked_until）。
            let slot = if kind == BackendErrorKind::RateLimited {
                &mut state.hard_cooldown_until
            } else {
                &mut state.soft_cooldown_until
            };
            *slot = Some(slot.map_or(until, |current| current.max(until)));
            RetryAdvice::Cooldown(delay)
        } else {
            RetryAdvice::None
        };
        drop(state);
        self.notify.notify_waiters();
        advice
    }

    /// 只读快照。
    ///
    /// 界面每帧都会调它，所以这里**不能**有状态机副作用：`advance_circuit`
    /// 会推进熔断状态并清掉 hard cooldown，把它放在这里等于让熔断的推进
    /// 频率由渲染帧率决定——窗口最小化时推进变慢，高刷屏上推进变快。
    /// 状态推进只发生在 `acquire`（真正要发请求的时刻）。
    pub fn snapshot(&self) -> NetworkSnapshot {
        let now = Instant::now();
        let mut state = self.state.lock();
        self.prune_request_events(&mut state, now);
        let denominator = state
            .first_request_at
            .map(|first| now.duration_since(first).as_secs_f64().clamp(1.0, 60.0))
            .unwrap_or(60.0);
        // 快照取 Refresh 视角（软+硬冷却全部计入），供监控循环计算等待。
        let cooldown_until = self.blocked_until(&state, now, RequestPriority::Refresh);
        NetworkSnapshot {
            requests_per_second: state.request_events.len() as f64 / denominator,
            latency_ewma_ms: state.latency_ewma_ms,
            total_rate_limits: state.total_rate_limits,
            consecutive_errors: state.consecutive_errors,
            cooldown_remaining: cooldown_until
                .map(|until| until.saturating_duration_since(now))
                .unwrap_or(Duration::ZERO),
            last_error_kind: state.last_error_kind,
            circuit_status: Self::circuit_view(&state, now),
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_cooldown_for_tests(&self) {
        let mut state = self.state.lock();
        state.hard_cooldown_until = None;
        state.soft_cooldown_until = None;
        drop(state);
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn set_cooldown_for_tests(&self, duration: Duration) {
        self.state.lock().hard_cooldown_until = Some(Instant::now() + duration);
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
            state.hard_cooldown_until = None;
        }
    }

    /// `advance_circuit` 的只读版本：报告「如果此刻去 acquire，熔断会处于
    /// 什么状态」，但不真的推进它。
    ///
    /// 界面每帧都取快照，让状态机的推进频率由渲染帧率决定是错的（窗口最小化
    /// 时推进变慢、高刷屏上变快）。推进只发生在 `acquire`。
    fn circuit_view(state: &GovernorState, now: Instant) -> CircuitStatus {
        match state.circuit {
            Circuit::Closed => CircuitStatus::Closed,
            // 冷却已到期：下一次 acquire 就会转半开，快照如实这么报告。
            Circuit::Open { until } if now >= until => CircuitStatus::HalfOpen,
            Circuit::Open { .. } => CircuitStatus::Open,
            Circuit::HalfOpen { .. } => CircuitStatus::HalfOpen,
        }
    }

    fn blocked_until(
        &self,
        state: &GovernorState,
        now: Instant,
        priority: RequestPriority,
    ) -> Option<Instant> {
        let hard = state.hard_cooldown_until.filter(|until| *until > now);
        // 本地退避只约束轮询类请求：一次超时/连接失败不应把同轮里
        // 紧随其后的提交也挡住（限流与熔断仍走 hard/circuit 全局生效）。
        let soft = state
            .soft_cooldown_until
            .filter(|until| *until > now && priority != RequestPriority::Submission);
        let circuit = match state.circuit {
            Circuit::Open { until } if until > now => Some(until),
            _ => None,
        };
        [hard, soft, circuit].into_iter().flatten().max()
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
/// 本地推测的退避阶梯。
///
/// 走统一的 decorrelated jitter：原来是裸的 2/4/8/16/30 确定性阶梯，所有
/// 客户端在同一次服务器抖动后会整齐划一地同时重试。
fn retry_delay_for_failure(consecutive_errors: u32) -> Duration {
    super::backoff::backoff_for_attempt(
        consecutive_errors,
        super::backoff::BACKOFF_BASE,
        super::backoff::BACKOFF_MAX,
    )
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
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    // ── T-03 并发属性测试 ─────────────────────────────────────────────
    // governor 原有的 17 个测试全是单线程顺序断言：优先级队列、丢唤醒、
    // half-open 竞争这些只在并发下出现的问题一个都测不到。放开提交并发
    // （G-03）之前必须先有这层网。

    /// 不变式一：任意并发压力下，实际速率不超过令牌桶配置。
    ///
    /// 这是「放开并发不会变成 DDoS 学校」的核心保证。
    #[test]
    fn concurrent_acquires_never_exceed_the_configured_rate() {
        runtime().block_on(async {
            let mut policy = test_policy();
            policy.normal_requests_per_second = 20.0;
            policy.burst_requests_per_second = 20.0;
            policy.capacity = 4.0;
            let governor = Arc::new(RequestGovernor::with_policy(policy));

            const TOTAL: usize = 40;
            let started = Instant::now();
            let mut handles = Vec::new();
            for _ in 0..TOTAL {
                let governor = governor.clone();
                handles.push(tokio::spawn(async move {
                    let permit = governor.acquire(RequestPriority::Submission).await;
                    governor.record_success(&permit);
                }));
            }
            for handle in handles {
                handle.await.unwrap();
            }
            let elapsed = started.elapsed();
            // 前 capacity 个可以瞬时放行，其余必须按 20/s 补充。
            let minimum = Duration::from_secs_f64((TOTAL as f64 - 4.0) / 20.0);
            assert!(
                elapsed >= minimum.mul_f64(0.9),
                "40 concurrent acquires finished in {elapsed:?}, faster than the {minimum:?} \
                 the token bucket allows — the rate limit is not holding under concurrency"
            );
        });
    }

    /// 不变式二：hard cooldown 期间零请求通过，最高优先级也不例外。
    ///
    /// 服务器明确要求的降速必须全局不可绕过，否则「被限流后继续猛打」
    /// 正是升级成封禁的路径。
    #[test]
    fn hard_cooldown_blocks_every_priority_including_submission() {
        runtime().block_on(async {
            let mut policy = test_policy();
            policy.normal_requests_per_second = 1_000.0;
            policy.burst_requests_per_second = 1_000.0;
            policy.capacity = 64.0;
            policy.retry_min = Duration::from_millis(1);
            policy.retry_max = Duration::from_secs(300);
            let governor = Arc::new(RequestGovernor::with_policy(policy));

            let permit = governor.acquire(RequestPriority::Refresh).await;
            let cooldown = Duration::from_millis(180);
            governor.record_failure(&permit, BackendErrorKind::RateLimited, Some(cooldown));

            let started = Instant::now();
            let mut handles = Vec::new();
            for priority in [
                RequestPriority::Submission,
                RequestPriority::Session,
                RequestPriority::Refresh,
                RequestPriority::KeepAlive,
            ] {
                let governor = governor.clone();
                handles.push(tokio::spawn(async move {
                    let permit = governor.acquire(priority).await;
                    let waited = started.elapsed();
                    governor.record_success(&permit);
                    (priority, waited)
                }));
            }
            for handle in handles {
                let (priority, waited) = handle.await.unwrap();
                assert!(
                    waited >= cooldown.mul_f64(0.8),
                    "{priority:?} slipped through a hard cooldown after only {waited:?}"
                );
            }
        });
    }

    /// 不变式三：并发等待时，Submission 的等待时长永远不长于同时刻的 Refresh。
    ///
    /// 抢课是零和的先到先得，提交被后台刷新挡住等于直接输掉。
    #[test]
    fn submission_never_waits_longer_than_a_concurrent_refresh() {
        runtime().block_on(async {
            let mut policy = test_policy();
            policy.normal_requests_per_second = 8.0;
            policy.burst_requests_per_second = 8.0;
            policy.capacity = 1.0;
            let governor = Arc::new(RequestGovernor::with_policy(policy));
            // 先耗光令牌，保证两个请求都要排队。
            let warm = governor.acquire(RequestPriority::Refresh).await;
            governor.record_success(&warm);

            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            for _ in 0..4 {
                let governor = governor.clone();
                let sender = sender.clone();
                tokio::spawn(async move {
                    let permit = governor.acquire(RequestPriority::Refresh).await;
                    let _ = sender.send(RequestPriority::Refresh);
                    governor.record_success(&permit);
                });
            }
            wait_for_waiters(&governor, 4).await;
            let governor_for_submission = governor.clone();
            let submission_sender = sender.clone();
            tokio::spawn(async move {
                let permit = governor_for_submission
                    .acquire(RequestPriority::Submission)
                    .await;
                let _ = submission_sender.send(RequestPriority::Submission);
                governor_for_submission.record_success(&permit);
            });
            drop(sender);

            let first = receiver.recv().await.expect("someone must be served");
            assert_eq!(
                first,
                RequestPriority::Submission,
                "a queued submission must not wait behind background refreshes"
            );
        });
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
    fn shared_governor_is_reused_per_origin() {
        let first = RequestGovernor::shared_for_origin("https://registry-a.test:443");
        let second = RequestGovernor::shared_for_origin("https://registry-a.test:443");
        let other = RequestGovernor::shared_for_origin("https://registry-b.test:443");
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[test]
    fn transient_failure_cooldown_exempts_submission_but_not_refresh() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let failed = governor.acquire(RequestPriority::Submission).await;
            governor.record_failure(&failed, BackendErrorKind::Timeout, None);

            // 本地退避首档为 2s：不得挡住同轮里紧随其后的另一次提交。
            let started = Instant::now();
            let next = governor.acquire(RequestPriority::Submission).await;
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "submission was blocked by a transient local backoff"
            );
            governor.record_success(&next);

            // 轮询类请求仍要遵守本地退避。
            let blocked = tokio::time::timeout(
                Duration::from_millis(50),
                governor.acquire(RequestPriority::Refresh),
            )
            .await;
            assert!(blocked.is_err(), "refresh must honour the local backoff");
        });
    }

    #[test]
    fn production_retry_ceiling_matches_the_parse_layer() {
        // parse_retry_after_secs 将 Retry-After 钳到 300s；治理层不得再截短，
        // 否则会在服务器要求的冷却结束前重试。
        assert_eq!(
            GovernorPolicy::default().retry_max,
            Duration::from_secs(300)
        );
    }

    #[test]
    // 本地退避改成 decorrelated jitter 后，锁死的是「包络 + 真的被打散」，
    // 而不是精确序列——精确序列恰恰是重试雪崩的成因。
    fn retry_schedule_matches_the_documented_sequence() {
        for attempt in [1u32, 2, 3, 4, 5, 99] {
            let delay = retry_delay_for_failure(attempt);
            assert!(
                delay >= Duration::from_secs(2) && delay <= Duration::from_secs(30),
                "attempt {attempt} produced {delay:?} outside [2s, 30s]"
            );
        }
        let samples: Vec<Duration> = (0..64).map(|_| retry_delay_for_failure(3)).collect();
        assert!(
            samples.windows(2).any(|w| w[0] != w[1]),
            "local backoff ladder must be jittered, not deterministic"
        );
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
    // 刷新闸门仍是独占的（一次全量刷新不该和另一次重叠）；提交闸门放宽到
    // SUBMISSION_CONCURRENCY——放开的是「同时在途的提交数」，速率上限仍由
    // 令牌桶承担。两个闸门相互独立。
    fn refresh_and_submission_gates_are_independent_but_each_is_exclusive() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let refresh = governor.enter_refresh().await;
            // 占满提交闸门的全部许可。
            let mut submissions = Vec::new();
            for _ in 0..SUBMISSION_CONCURRENCY {
                submissions.push(governor.enter_submission().await);
            }

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
            assert!(
                submission_rx.try_recv().is_err(),
                "the {SUBMISSION_CONCURRENCY}th+1 submission must still queue"
            );

            // 释放刷新许可不得连带放行排队中的提交：两个闸门是独立的。
            drop(refresh);
            refresh_rx.recv().await.unwrap();
            assert!(submission_rx.try_recv().is_err());
            drop(submissions.pop());
            submission_rx.recv().await.unwrap();
        });
    }

    // G-03 的边界：提交并发度必须恰好是 SUBMISSION_CONCURRENCY，
    // 既不该退回单许可（那样并发提交等于没做），也不该无上限。
    #[test]
    fn submission_gate_admits_exactly_the_configured_concurrency() {
        runtime().block_on(async {
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            let mut held = Vec::new();
            for index in 0..SUBMISSION_CONCURRENCY {
                let permit =
                    tokio::time::timeout(Duration::from_millis(200), governor.enter_submission())
                        .await;
                assert!(
                    permit.is_ok(),
                    "submission #{index} should not have queued behind the others"
                );
                held.push(permit.unwrap());
            }
            // 再多一个必须排队。
            let extra =
                tokio::time::timeout(Duration::from_millis(80), governor.enter_submission()).await;
            assert!(extra.is_err(), "submission concurrency must stay bounded");
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
    // A-03：半开探针只对「链路层面的失败」负责。
    //
    // 拿到干净的 HTTP 200、只是业务被拒（选课失败），恰恰证明链路是通的——
    // 把它算作探针失败会白白重开一整轮熔断，正好卡在恢复的那一刻。
    // 链路层面的失败（超时/连接/限流/5xx）当然仍要重开。
    fn business_rejection_does_not_reopen_the_circuit_but_link_failure_does() {
        runtime().block_on(async {
            async fn trip(governor: &Arc<RequestGovernor>) {
                for _ in 0..3 {
                    let permit = governor.acquire(RequestPriority::Refresh).await;
                    governor.record_failure(
                        &permit,
                        BackendErrorKind::RateLimited,
                        Some(Duration::from_millis(12)),
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // 业务被拒：探针算通过，熔断关闭。
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            trip(&governor).await;
            let probe = governor.acquire(RequestPriority::Session).await;
            assert!(probe.half_open_probe);
            let advice = governor.record_failure(&probe, BackendErrorKind::Business, None);
            assert_ne!(
                advice,
                RetryAdvice::CircuitOpen(Duration::from_millis(45)),
                "a business rejection over a clean 200 proves the link is healthy"
            );
            assert_ne!(
                governor.snapshot().circuit_status,
                CircuitStatus::Open,
                "the circuit must not slam shut right when we are recovering"
            );
            // 探针必须有归宿。只断言「没重新熔断」是不够的——把探针留在
            // HalfOpen{probe_id:Some} 同样不算熔断，但会让后续每一个请求
            // 无限排队。必须真的能再拿到许可。
            let resumed = tokio::time::timeout(
                Duration::from_millis(500),
                governor.acquire(RequestPriority::Refresh),
            )
            .await;
            assert!(
                resumed.is_ok(),
                "a business rejection must not strand the probe and hang every later request"
            );
            governor.record_success(&resumed.unwrap());

            // 链路失败：仍然重开整轮冷却。
            let governor = Arc::new(RequestGovernor::with_policy(test_policy()));
            trip(&governor).await;
            let probe = governor.acquire(RequestPriority::Session).await;
            assert!(probe.half_open_probe);
            assert_eq!(
                governor.record_failure(&probe, BackendErrorKind::Timeout, None),
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
            let governor = Arc::new(RequestGovernor::default());

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
